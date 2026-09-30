use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
struct InterruptedResponse {
    reason: &'static str,
    prefix: String,
}
#[derive(Debug)]
struct VisionRejected;
impl std::fmt::Display for VisionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Provider rejected image input for the selected model")
    }
}
impl std::error::Error for VisionRejected {}
#[derive(Debug)]
struct ProviderTargetChanged;
impl std::fmt::Display for ProviderTargetChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Selected provider changed while awaiting request admission")
    }
}
impl std::error::Error for ProviderTargetChanged {}
impl std::fmt::Display for InterruptedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for InterruptedResponse {}

pub struct Model {
    client: Client,
    url: String,
    name: String,
    context: u32,
    output: u32,
    output_cap: Cell<Option<u32>>,
    watchdog_call: Cell<bool>,
    stop: Arc<AtomicBool>,
    trace: RefCell<Option<PathBuf>>,
    sequence: Cell<u32>,
    settings_path: Option<PathBuf>,
    chat_settings: Option<PathBuf>,
    completed_messages: RefCell<Option<Vec<Value>>>,
    completed_reasoning: RefCell<String>,
    context_pressure: Cell<bool>,
    run_controls: Option<(Arc<AtomicBool>, Instant)>,
    pub(crate) controls: Arc<crate::run_control::RunControl>,
    request_target: RefCell<(String, String)>,
    pinned_target: Option<(String, String, PathBuf)>,
    vision_capabilities: RefCell<HashMap<(String, String), bool>>,
}
impl Model {
    pub fn new(
        url: &str,
        name: &str,
        context: u32,
        output: u32,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            url: crate::groq::url(url, name),
            name: name.into(),
            context,
            output,
            output_cap: Cell::new(None),
            watchdog_call: Cell::new(false),
            stop,
            trace: RefCell::new(None),
            sequence: Cell::new(0),
            settings_path: None,
            chat_settings: None,
            completed_messages: RefCell::new(None),
            completed_reasoning: RefCell::new(String::new()),
            context_pressure: Cell::new(false),
            run_controls: None,
            controls: Arc::new(crate::run_control::RunControl::default()),
            request_target: RefCell::new((name.into(), crate::groq::url(url, name))),
            pinned_target: None,
            vision_capabilities: RefCell::new(HashMap::new()),
        })
    }
    fn supports_vision(&self, name: &str, url: &str) -> bool {
        if let Some(id) = crate::groq::model_id(name) {
            // Groq documents this vision+tools model; other Groq models need a
            // text fallback rather than a permanently rejected conversation.
            return id == "qwen/qwen3.8-27b"
                || id.contains("llama-4-scout")
                || id.contains("llama-4-maverick");
        }
        let cache_key = (url.to_owned(), name.to_owned());
        if let Some(supported) = self.vision_capabilities.borrow().get(&cache_key) {
            return *supported;
        }
        let Some(base) = url.strip_suffix("/api/chat") else {
            return true;
        };
        let response = self
            .client
            .post(format!("{base}/api/show"))
            .json(&json!({"model":name}))
            .timeout(Duration::from_secs(10))
            .send();
        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(metadata) = response.json::<Value>()
            && let Some(capabilities) = metadata["capabilities"].as_array()
        {
            let supported = capabilities.iter().any(|c| c == "vision");
            self.vision_capabilities
                .borrow_mut()
                .insert(cache_key, supported);
            supported
        } else {
            // Older servers may not advertise capabilities. The chat endpoint
            // gets one chance; a vision-specific rejection recovers as text.
            self.vision_capabilities
                .borrow_mut()
                .insert(cache_key, true);
            true
        }
    }
    pub fn take_completed_messages(&self) -> Option<Vec<Value>> {
        self.completed_messages.borrow_mut().take()
    }
    /// Drain reasoning from the latest successful request without adding it to
    /// returned assistant messages or the provider's future conversation.
    pub fn take_completed_reasoning(&self) -> String {
        std::mem::take(&mut *self.completed_reasoning.borrow_mut())
    }
    pub fn command_review_seconds(&self, fallback: u64) -> u64 {
        self.settings_path
            .as_ref()
            .and_then(|p| crate::runner::load(p).ok())
            .map(|c| c.command_review_seconds)
            .unwrap_or(fallback)
    }
    pub fn context_pressure(&self) -> bool {
        self.context_pressure.get()
    }
    pub fn use_project_settings(&mut self, path: &Path) {
        self.settings_path = Some(path.to_owned());
    }
    pub fn use_chat_settings(&mut self, path: &Path, session: &Path) {
        self.settings_path = Some(path.into());
        self.chat_settings = Some(session.join("provider-wait.json"));
    }
    fn selected_name<'a>(&'a self, c: &'a crate::runner::Config) -> &'a str {
        if let Some((name, _, _)) = &self.pinned_target {
            name
        } else if self.chat_settings.is_some() && !c.chat_model.trim().is_empty() {
            &c.chat_model
        } else {
            &c.model
        }
    }
    pub fn use_run_controls(&mut self, stop: Arc<AtomicBool>, started: Instant) {
        self.run_controls = Some((stop, started));
    }
    pub(crate) fn stopped(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
    pub(crate) fn configure_helper(
        &self,
        helper: &mut Model,
        config_path: &Path,
        jobdir: &Path,
        name: &str,
        url: &str,
    ) {
        helper.stop = self.stop.clone();
        helper.controls = self.controls.clone();
        helper.run_controls = self.run_controls.clone();
        helper.settings_path = Some(config_path.into());
        helper.pinned_target = Some((name.into(), url.into(), jobdir.join("provider-wait.json")));
    }
    pub fn pause_point(&self) {
        self.controls
            .wait_until_resumed(|| self.stop.load(Ordering::SeqCst));
    }
    fn provider_target(&self) -> Result<(String, String, Option<PathBuf>)> {
        if let Some((name, url, wait)) = &self.pinned_target {
            return Ok((name.clone(), url.clone(), Some(wait.clone())));
        }
        if let Some(path) = &self.settings_path {
            let c = crate::runner::load(path)?;
            Ok((
                self.selected_name(&c).into(),
                crate::groq::url(&c.ollama_url, self.selected_name(&c)),
                Some(
                    self.chat_settings
                        .clone()
                        .unwrap_or_else(|| c.state_dir.join("provider-wait.json")),
                ),
            ))
        } else {
            Ok((self.name.clone(), self.url.clone(), None))
        }
    }
    fn wait_for_provider(&self, record: &Value) -> Result<()> {
        self.controls.provider_wait_started();
        let result = self.wait_for_provider_inner(record);
        self.controls.provider_wait_finished();
        result
    }
    fn wait_for_provider_inner(&self, record: &Value) -> Result<()> {
        let until = record["until"].as_u64().unwrap_or(0);
        let mut last_seconds = u64::MAX;
        loop {
            let current = self.provider_target()?;
            if self.stop.load(Ordering::SeqCst)
                || self
                    .run_controls
                    .as_ref()
                    .is_some_and(|(s, _)| s.load(Ordering::SeqCst))
            {
                return Err(crate::provider::Stopped(
                    "Stopped while waiting for provider; work and conversation retained".into(),
                )
                .into());
            }
            if let Some((_, started)) = &self.run_controls {
                let c = crate::runner::load(
                    self.settings_path
                        .as_ref()
                        .context("Missing run settings")?,
                )?;
                if c.run_duration_seconds > 0
                    && self.controls.active_elapsed(*started).as_secs() >= c.run_duration_seconds
                {
                    return Err(crate::provider::Stopped(
                        "Run timer reached while waiting for provider; saving work".into(),
                    )
                    .into());
                }
            }
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if self.controls.pause_requested() {
                crate::events::send(crate::events::Event::ProviderWait {
                    reason: record["reason"]
                        .as_str()
                        .unwrap_or("Provider unavailable")
                        .into(),
                    seconds: until.saturating_sub(now),
                });
                self.pause_point();
                continue;
            }
            let manual = self.controls.take_retry();
            if manual || current.0 != record["model"] || current.1 != record["url"] || now >= until
            {
                if let Some(path) = current.2 {
                    let _ = std::fs::remove_file(path);
                }
                if manual {
                    crate::events::log(
                        if record["reason"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("Groq budget:"))
                        {
                            "Manual retry: rechecking Groq budget."
                        } else {
                            "Manual retry: trying the provider now."
                        }
                        .into(),
                    );
                }
                return Ok(());
            }
            let seconds = until.saturating_sub(now);
            if seconds != last_seconds {
                crate::events::send(crate::events::Event::ProviderWait {
                    reason: record["reason"]
                        .as_str()
                        .unwrap_or("Provider unavailable")
                        .into(),
                    seconds,
                });
                last_seconds = seconds;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    pub fn trace_to(&self, path: &Path) {
        *self.trace.borrow_mut() = Some(path.into());
        self.sequence.set(0);
    }
    pub fn chat(
        &self,
        messages: &[Value],
        tools: Option<Value>,
        structured: bool,
    ) -> Result<Value> {
        self.chat_format(messages, tools, structured.then(|| json!("json")))
    }
    pub fn diagnostic_chat(&self, messages: &[Value], tools: Value) -> Result<Value> {
        let previous = self.output_cap.replace(Some(2048));
        let result = self.chat(messages, Some(tools), false);
        self.output_cap.set(previous);
        result
    }
    pub fn watchdog_chat(&self, messages: &[Value], tools: Value) -> Result<Value> {
        let previous = self.output_cap.replace(Some(2048));
        let mode = self.watchdog_call.replace(true);
        *self.completed_messages.borrow_mut() = None;
        self.completed_reasoning.borrow_mut().clear();
        // Observers must not disappear into an indefinite provider quota retry.
        let result = self.chat_format_once(messages, Some(tools), None);
        self.output_cap.set(previous);
        self.watchdog_call.set(mode);
        self.completed_reasoning.borrow_mut().clear();
        if result.is_err() {
            crate::events::send(crate::events::Event::RequestFinished);
        }
        result
    }
    fn chat_format(
        &self,
        messages: &[Value],
        tools: Option<Value>,
        format: Option<Value>,
    ) -> Result<Value> {
        *self.completed_messages.borrow_mut() = None;
        self.completed_reasoning.borrow_mut().clear();
        let mut conversation = messages.to_vec();
        let canonical_tools = tools;
        let mut transport_retries = 0;
        let mut generation_retries = 0;
        let mut provider_attempts = 0u32;
        if let Some(path) = self.provider_target()?.2
            && path.exists()
        {
            let record: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            provider_attempts = record["attempt"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
            self.wait_for_provider(&record)?;
        }
        loop {
            // Groq documentation compaction lasts one target attempt. A later
            // local model always receives the original complete descriptions.
            let mut tools = canonical_tools.clone();
            let image_attempt = crate::vision::has_images(&conversation);
            self.pause_point();
            let (target, target_url, _) = self.provider_target()?;
            if crate::vision::has_images(&conversation) {
                let supported = self.supports_vision(&target, &target_url);
                conversation = crate::vision::prepare(
                    &conversation,
                    supported,
                    &format!("Model {target} does not support images"),
                );
                if !supported {
                    crate::events::log(format!(
                        "Model {target} cannot inspect images; returning an explicit image-tool failure and continuing with text."
                    ));
                }
            }
            if crate::groq::model_id(&target).is_some() {
                if crate::vision::has_images(&conversation)
                    && let Err(error) =
                        crate::vision::ensure_groq_payload(&conversation, tools.as_ref())
                {
                    conversation =
                        crate::vision::prepare(&conversation, false, &format!("{error:#}"));
                    crate::events::log("Image observation exceeds Groq's encoded request size; returning an explicit tool failure and continuing as text.".into());
                }
                let configured_output = self
                    .settings_path
                    .as_ref()
                    .map(|p| crate::runner::load(p).map(|c| c.output_tokens))
                    .transpose()?
                    .unwrap_or(self.output);
                let output = self
                    .output_cap
                    .get()
                    .unwrap_or(configured_output)
                    .min(configured_output)
                    .min(crate::groq::Limits::load()?.max_response_tokens);
                let prepared = match crate::groq::prepare(
                    &target,
                    &conversation,
                    tools.as_ref(),
                    output as u64,
                ) {
                    Ok(prepared) => prepared,
                    Err(_) if image_attempt => {
                        if let Some(schema) = &tools {
                            tools = Some(crate::groq::compact_descriptions(schema));
                        }
                        match crate::groq::prepare(
                            &target,
                            &conversation,
                            tools.as_ref(),
                            output as u64,
                        ) {
                            Ok(prepared) => {
                                crate::events::log("Groq image budget: shortened tool documentation, preserving all tools and argument constraints.".into());
                                prepared
                            }
                            Err(error) => {
                                let reason = format!(
                                    "Groq's request token allowance cannot fit the images plus the current goal/tools: {error:#}. Use a larger configured allowance only if your account supports it, or use a local vision model"
                                );
                                conversation =
                                    crate::vision::prepare(&conversation, false, &reason);
                                crate::events::log("Image observation could not fit Groq's request allowance; returning an explicit tool failure and continuing as text without sending the oversized request.".into());
                                crate::groq::prepare(
                                    &target,
                                    &conversation,
                                    tools.as_ref(),
                                    output as u64,
                                )?
                            }
                        }
                    }
                    Err(error) => return Err(error),
                };
                if prepared != conversation
                    && let Some(path) = self.trace.borrow().as_ref()
                {
                    std::fs::write(
                        path.join(format!(
                            "groq-context-before-{:03}.json",
                            self.sequence.get()
                        )),
                        serde_json::to_vec_pretty(&conversation)?,
                    )?;
                }
                conversation = prepared;
            }
            let result = self.chat_format_once(&conversation, tools.clone(), format.clone());
            if let Err(error) = &result {
                crate::events::send(crate::events::Event::RequestFinished);
                if error.downcast_ref::<ProviderTargetChanged>().is_some() {
                    conversation = messages.to_vec();
                    continue;
                }
                if error.downcast_ref::<VisionRejected>().is_some() {
                    let (failed_name, failed_url) = self.request_target.borrow().clone();
                    self.vision_capabilities
                        .borrow_mut()
                        .insert((failed_url, failed_name.clone()), false);
                    conversation = crate::vision::prepare(
                        &conversation,
                        false,
                        &format!("Provider rejected image input for {failed_name}"),
                    );
                    crate::events::log("Image input rejected: returning an explicit image-tool failure and continuing the same conversation as text. Select a vision-capable model and request the image again.".into());
                    continue;
                }
                if let Some(provider) = error.downcast_ref::<crate::provider::Unavailable>() {
                    provider_attempts = provider_attempts.saturating_add(1);
                    let delay = provider
                        .retry_after
                        .unwrap_or_else(|| crate::provider::backoff(provider_attempts));
                    let (model, url) = self.request_target.borrow().clone();
                    // Round up to the next second so persistence never shortens Retry-After.
                    let record = json!({"reason":provider.reason,"model":model,"url":url,"attempt":provider_attempts,"until":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().saturating_add(delay.as_secs()).saturating_add(1),"delay_seconds":delay.as_secs()});
                    if let Some(path) = self.trace.borrow().as_ref() {
                        std::fs::write(
                            path.join(format!(
                                "provider-error-{:03}.json",
                                self.sequence.get().saturating_sub(1)
                            )),
                            serde_json::to_vec_pretty(&record)?,
                        )?;
                    }
                    if let Some(path) = self.provider_target()?.2 {
                        let temp = path.with_extension("tmp");
                        std::fs::write(&temp, serde_json::to_vec_pretty(&record)?)?;
                        std::fs::rename(temp, path)?;
                    }
                    crate::events::log(format!(
                        "{}; waiting {} seconds before retrying the same conversation.",
                        provider.reason,
                        delay.as_secs()
                    ));
                    self.wait_for_provider(&record)?;
                    continue;
                }
                let transient = error.chain().any(|e| {
                    e.downcast_ref::<reqwest::Error>()
                        .is_some_and(|e| e.is_timeout() || e.is_connect())
                });
                let interrupted = error.downcast_ref::<InterruptedResponse>();
                let retry = !self.stop.load(Ordering::SeqCst)
                    && ((transient && transport_retries < 1)
                        || (interrupted.is_some() && generation_retries < 2));
                if let Some(path) = self.trace.borrow().as_ref() {
                    std::fs::write(
                        path.join(format!(
                            "request-failure-{:03}.json",
                            self.sequence.get().saturating_sub(1)
                        )),
                        serde_json::to_vec_pretty(
                            &json!({"error":format!("{error:#}"),"retry_same_stage":retry,"generation_retries":generation_retries,"transport_retries":transport_retries}),
                        )?,
                    )?;
                }
                if transient && retry {
                    transport_retries += 1;
                    crate::events::log("Model request timed out or failed to connect; retrying the same stage once with current settings. Completed edits and checks are retained.".into());
                    continue;
                }
                if let Some(interrupted) = interrupted.filter(|_| retry) {
                    generation_retries += 1;
                    // Rebuild from the last completed turn, never accumulating failed drafts.
                    conversation = messages.to_vec();
                    if format.is_none() && !interrupted.prefix.is_empty() {
                        conversation.push(json!({"role":"assistant","content":interrupted.prefix}));
                    }
                    let instruction = if format.is_some() {
                        "Return a complete replacement JSON response matching the required schema. Do not continue a partial JSON fragment."
                    } else {
                        "Continue from the last completed action. Use the existing tool results and current task. Take the next concrete action with a tool, or finish if the work is complete. Any interrupted tool calls were NOT executed; issue complete calls again if needed."
                    };
                    conversation.push(json!({"role":"user","content":format!("The previous response was interrupted: {}. Recovery attempt {generation_retries}. Completed actions and file changes are retained. Do not repeat the interrupted narration. {instruction}",interrupted.reason)}));
                    crate::events::log(format!(
                        "{}; continuing the same conversation (recovery {generation_retries}/2). Completed tool results and edits are retained.",
                        interrupted.reason
                    ));
                    continue;
                }
            } else if generation_retries > 0 {
                crate::events::log("Model response recovered; continuing the current task.".into());
            }
            if result.is_ok() {
                crate::vision::mark_observed(&mut conversation);
                *self.completed_messages.borrow_mut() = Some(conversation);
            }
            return result;
        }
    }

    fn chat_format_once(
        &self,
        messages: &[Value],
        tools: Option<Value>,
        format: Option<Value>,
    ) -> Result<Value> {
        // Retries and early failures must not expose a prior completed request.
        self.completed_reasoning.borrow_mut().clear();
        self.pause_point();
        if self.controls.stopped_while_held() {
            return Err(crate::provider::Stopped(
                "Stopped while paused; conversation retained".into(),
            )
            .into());
        }
        // Snapshot settings once; edits never alter an in-flight request.
        let live = self
            .settings_path
            .as_ref()
            .map(|p| crate::runner::load(p))
            .transpose()?;
        let name = live
            .as_ref()
            .map(|c| self.selected_name(c))
            .unwrap_or(&self.name);
        let url = live
            .as_ref()
            .map(|c| crate::groq::url(&c.ollama_url, self.selected_name(c)))
            .unwrap_or_else(|| self.url.clone());
        let url = self
            .pinned_target
            .as_ref()
            .map_or(url, |(_, url, _)| url.clone());
        let timeout = live
            .as_ref()
            .map(|c| c.request_timeout_seconds)
            .unwrap_or(1800);
        let timeout = if self.watchdog_call.get() {
            if timeout == 0 { 120 } else { timeout.min(120) }
        } else {
            timeout
        };
        let groq = crate::groq::model_id(name);
        // Validate/read credentials before reserving capacity. Never include them in traces.
        let key = groq.map(|_| crate::groq::key()).transpose()?;
        let output = live
            .as_ref()
            .map(|c| c.output_tokens)
            .unwrap_or(self.output);
        let output = self.output_cap.get().map_or(output, |cap| output.min(cap));
        let output = if groq.is_some() {
            output.min(crate::groq::Limits::load()?.max_response_tokens)
        } else {
            output
        };
        let (_permit, reservation) = loop {
            let permit = crate::inference::acquire(&url, &self.controls, &self.stop)?;
            if groq.is_none() {
                break (permit, None);
            }
            match crate::groq::admit(name, messages, tools.as_ref(), output as u64)? {
                crate::groq::Admission::Ready(r) => break (permit, Some(r)),
                crate::groq::Admission::Wait(seconds) => {
                    drop(permit);
                    if self.watchdog_call.get() {
                        anyhow::bail!(
                            "Groq budget unavailable for watchdog; command remains running"
                        );
                    }
                    let record = json!({"reason":"Groq budget: waiting before sending a request", "model":name,"url":url,"until":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()+seconds});
                    self.wait_for_provider(&record)?;
                    if self.provider_target()?.0 != name {
                        return Err(ProviderTargetChanged.into());
                    }
                }
            }
        };
        if self.controls.stopped_while_held() {
            return Err(crate::provider::Stopped("Stopped while awaiting inference".into()).into());
        }
        *self.request_target.borrow_mut() = (name.to_owned(), url.clone());
        crate::events::send(crate::events::Event::RequestModel(name.to_owned()));
        crate::events::send(crate::events::Event::Request);
        anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
        let wire_messages = if groq.is_none() {
            crate::vision::ollama_messages(messages)?
        } else {
            messages.to_vec()
        };
        let mut body = json!({"model":name,"messages":wire_messages,"stream":true,"think":false,"options":{"num_ctx":self.context,"num_predict":output,"temperature":0.4}});
        if let Some(t) = tools {
            body["tools"] = t;
        }
        if let Some(schema) = format {
            body["format"] = schema;
            body["options"]["temperature"] = json!(0);
        }
        if let Some(id) = groq {
            let formatted = body.get("format").is_some();
            let mut wire = json!({"model":id,"messages":crate::groq::messages(messages)?,"stream":true,"stream_options":{"include_usage":true},"max_completion_tokens":output,"temperature":if formatted{0.0}else{0.4}});
            if let Some(t) = body.get("tools") {
                wire["tools"] = t.clone();
            }
            if formatted {
                wire["response_format"] = json!({"type":"json_object"});
            }
            body = wire;
        }
        let sequence = self.sequence.get();
        self.sequence.set(sequence + 1);
        let mut trace = if let Some(path) = self.trace.borrow().as_ref() {
            std::fs::write(
                path.join(format!("connection-{sequence:03}.json")),
                serde_json::to_vec_pretty(
                    &json!({"model":name,"request_timeout_seconds":timeout}),
                )?,
            )?;
            std::fs::write(
                path.join(format!("request-{sequence:03}.json")),
                serde_json::to_vec_pretty(&crate::vision::trace_body(&body, messages))?,
            )?;
            Some(std::fs::File::create(
                path.join(format!("response-{sequence:03}.ndjson")),
            )?)
        } else {
            None
        };
        let groq_client = if groq.is_some() {
            Some(crate::groq::client()?)
        } else {
            None
        };
        let request = groq_client
            .as_ref()
            .unwrap_or(&self.client)
            .post(url)
            .json(&body);
        let request = if let Some(key) = key {
            request.bearer_auth(key)
        } else {
            request
        };
        let request = if timeout == 0 {
            request
        } else {
            request.timeout(Duration::from_secs(timeout))
        };
        let response = request.send()?;
        if let Some(r) = &reservation {
            r.headers(response.headers())?;
        }
        let retry_after = crate::provider::retry_after(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            SystemTime::now(),
        );
        if !response.status().is_success() {
            let status = response.status().as_u16();
            if status == 429
                && let Some(r) = &reservation
            {
                r.defer(retry_after.unwrap_or(Duration::from_secs(60)).as_secs())?;
            }
            let mut body = String::new();
            response.take(16384).read_to_string(&mut body)?;
            if matches!(status, 400 | 422)
                && crate::vision::has_images(messages)
                && ["image", "vision", "multimodal"]
                    .iter()
                    .any(|word| body.to_lowercase().contains(word))
            {
                return Err(VisionRejected.into());
            }
            if let Some(provider) = crate::provider::classify(status, &body, retry_after) {
                return Err(provider.into());
            }
            bail!("Model provider HTTP {status}: request rejected");
        }
        let mut content = String::new();
        let mut thinking = String::new();
        let mut calls = Vec::new();
        let mut done = false;
        let started = Instant::now();
        let mut stream = crate::groq::Stream::default();
        let mut finish_reason = String::new();
        for line in BufReader::new(response).lines() {
            anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
            let line = line?;
            if let Some(file) = trace.as_mut() {
                writeln!(file, "{line}")?;
            }
            if line.is_empty() {
                continue;
            }
            let d: Value = if groq.is_some() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    anyhow::ensure!(
                        !finish_reason.is_empty(),
                        "Groq stream ended without a finish reason"
                    );
                    if let Some((prompt, generated)) = stream.usage {
                        if let Some(r) = &reservation {
                            r.settle(prompt, generated)?;
                        }
                        self.context_pressure.set(
                            prompt + generated >= self.context.saturating_sub(output + 1024) as u64,
                        );
                        crate::events::send(crate::events::Event::Metrics {
                            prompt,
                            generated,
                            seconds: started.elapsed().as_secs_f64(),
                        });
                    }
                    if finish_reason == "length" {
                        return Err(InterruptedResponse { reason:"Model reached its generation limit; produce a shorter complete response", prefix:String::new() }.into());
                    }
                    anyhow::ensure!(
                        matches!(finish_reason.as_str(), "stop" | "tool_calls"),
                        "Groq response ended with {finish_reason}"
                    );
                    calls = stream.calls()?;
                    anyhow::ensure!(
                        finish_reason != "tool_calls" || !calls.is_empty(),
                        "Groq finished tool calls without a complete call"
                    );
                    done = true;
                    break;
                }
                let frame = stream.frame(data)?;
                if let Some(reason) = frame["done_reason"].as_str() {
                    finish_reason = reason.into();
                }
                frame
            } else {
                serde_json::from_str(&line).context("Invalid Ollama stream JSON")?
            };
            if let Some(e) = d.get("error") {
                if let Some(provider) = crate::provider::classify(200, &e.to_string(), retry_after)
                {
                    return Err(provider.into());
                }
                bail!("Ollama: {e}")
            }
            if let Some(s) = d["message"]["content"].as_str() {
                content.push_str(s);
                crate::events::send(crate::events::Event::Delta(s.into()));
            }
            if let Some(s) = d["message"]["thinking"].as_str() {
                thinking.push_str(s);
                crate::events::send(crate::events::Event::Delta(s.into()));
            }
            if let Some(c) = d["message"]["tool_calls"].as_array() {
                calls.extend(c.clone());
            }
            let repetition = crate::repetition::start(&thinking)
                .map(|_| 0)
                .or_else(|| crate::repetition::start(&content));
            if let Some(start) = repetition {
                return Err(InterruptedResponse {
                    reason: "Repetitive model output interrupted",
                    // Never retain thinking, partial tool arguments, JSON, or code fragments.
                    prefix: if calls.is_empty() && !content.contains(['`', '{', '[']) {
                        content[..start].trim().to_owned()
                    } else {
                        String::new()
                    },
                }
                .into());
            }
            if d["done"].as_bool() == Some(true) {
                let used = d["prompt_eval_count"].as_u64().unwrap_or(0)
                    + d["eval_count"].as_u64().unwrap_or(0);
                self.context_pressure
                    .set(used >= self.context.saturating_sub(self.output + 1024) as u64);
                crate::events::send(crate::events::Event::Metrics {
                    prompt: d["prompt_eval_count"].as_u64().unwrap_or(0),
                    generated: d["eval_count"].as_u64().unwrap_or(0),
                    seconds: d["eval_duration"].as_f64().unwrap_or(0.) / 1_000_000_000.,
                });
                if d["done_reason"] == "length" {
                    return Err(InterruptedResponse {
                        reason: "Model reached its generation limit; produce a shorter complete response",
                        prefix: String::new(),
                    }.into());
                }
                done = true;
                break;
            }
        }
        anyhow::ensure!(done, "Model stream disconnected before completion");
        if !self.watchdog_call.get() {
            *self.completed_reasoning.borrow_mut() = thinking;
        }
        Ok(json!({"role":"assistant","content":content,"tool_calls":calls}))
    }
    pub fn structured<T: serde::de::DeserializeOwned + schemars::JsonSchema>(
        &self,
        system: &str,
        input: Value,
    ) -> Result<T> {
        let schema = serde_json::to_value(schemars::schema_for!(T))?;
        let system = format!("{system}\nRequired JSON schema: {schema}");
        self.structured_messages(
            &[
                json!({"role":"system","content":system}),
                json!({"role":"user","content":input.to_string()}),
            ],
            schema,
        )
    }

    fn structured_messages<T: serde::de::DeserializeOwned>(
        &self,
        messages: &[Value],
        schema: Value,
    ) -> Result<T> {
        let m = self.chat_format(messages, None, Some(schema.clone()))?;
        match parse_reply(m["content"].as_str().unwrap_or("")) {
            Ok(value) => Ok(value),
            Err(_) => {
                let mut repair = messages.to_vec();
                repair.push(m);
                repair.push(json!({"role":"user","content":"The prior reply could not be read as the required JSON. Return a complete corrected response matching the schema, using the evidence above. Double-quote keys; do not add commentary or Markdown."}));
                let repaired = self.chat_format(&repair, None, Some(schema))?;
                parse_reply(repaired["content"].as_str().unwrap_or(""))
                    .context("Could not read the model's reply after a formatting retry")
            }
        }
    }
}

fn parse_reply<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    // Extract one complete JSON value from surrounding prose, without rewriting its data.
    // Reject ambiguous multiple matching objects rather than guessing which to execute.
    if let Ok(value) = serde_json::from_str(text.trim()) {
        return Ok(value);
    }
    let repaired = quote_object_keys(text);
    let text = repaired.as_str();
    let mut matches = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find('{') {
        let start = offset + start;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next() {
            offset = start + stream.byte_offset();
            if let Ok(value) = serde_json::from_value::<T>(value) {
                matches.push(value);
            }
        } else {
            offset = start + 1;
        }
    }
    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    anyhow::ensure!(
        matches.is_empty(),
        "Model returned multiple conflicting answers"
    );
    let text = text.trim();
    let text = if text.starts_with("```") {
        let (_, body) = text
            .split_once('\n')
            .context("Incomplete formatted reply")?;
        body.trim_end()
            .strip_suffix("```")
            .context("Incomplete formatted reply")?
            .trim()
    } else {
        text
    };
    serde_json::from_str(text).with_context(|| {
        format!(
            "The reply was incomplete or had unexpected fields. Reply: {}",
            crate::project::excerpt(text, 2000)
        )
    })
}
#[cfg(test)]
fn repetitive(s: &str) -> bool {
    crate::repetition::start(s).is_some()
}
pub fn tools() -> Value {
    let mut tools = json!([
     {"type":"function","function":{"name":"finish_nudge","description":"Mark only the active user nudge complete, with concrete evidence. Does not finish the overall goal or current task. Use the exact nudge id from the latest priority update.","parameters":{"type":"object","properties":{"nudge_id":{"type":"integer","minimum":1},"summary":{"type":"string"},"evidence":{"type":"string"}},"required":["nudge_id","summary","evidence"]}}},
     {"type":"function","function":{"name":"save_progress_note","description":"Save a complete persistent progress note. Replace by default; append=true adds to it. Record findings, failed approaches, dependencies and next actions. Handoffs carry an excerpt; read_progress_note retrieves the full note. Notes are advisory, not proof. Empty replacement clears it.","parameters":{"type":"object","properties":{"note":{"type":"string"},"append":{"type":"boolean"}},"required":["note"]}}},
     {"type":"function","function":{"name":"read_progress_note","description":"Read the saved progress note in UTF-8 byte pages. Follow next_offset to retrieve the rest.","parameters":{"type":"object","properties":{"offset":{"type":"integer","minimum":0}}}}},
     {"type":"function","function":{"name":"project_map","description":"Inspect project file paths plus optional Rust declarations and module reachability. For other formats use search and read_file to inspect content. The map is inventory, not proof of correctness.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"set_task","description":"Record or revise the current task. Plans and file lists are advisory; work already on disk is always retained.","parameters":{"type":"object","properties":{"title":{"type":"string"},"objective":{"type":"string"},"acceptance":{"type":"array","items":{"type":"string"}},"files":{"type":"array","items":{"type":"string"}},"out_of_scope":{"type":"array","items":{"type":"string"}}},"required":["title","objective"]}}},
     {"type":"function","function":{"name":"finish_task","description":"Report completion of the active task only. Optional task_id must match set_task. Closed tasks cannot be completed again. The harness checks and saves work, retaining failures for repair.","parameters":{"type":"object","properties":{"summary":{"type":"string"},"task_id":{"type":"integer","minimum":1}},"required":["summary"]}}},
     {"type":"function","function":{"name":"read_file","description":"Read numbered lines from files of any size. Follow start_line continuation. For long lines or exact text use byte_offset and follow next_byte_offset.","parameters":{"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer"},"line_count":{"type":"integer"},"byte_offset":{"type":"integer","minimum":0}},"required":["path"]}}},
     {"type":"function","function":{"name":"edit_file","description":"Replace an exact, unique old_text occurrence with new_text, atomically. Reports changed=false for identical content. Supply exact file text without line-number prefixes.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}}},
     {"type":"function","function":{"name":"list_files","description":"List project paths.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"write_file","description":"Create or replace a project file. Read existing files first. Use project-relative paths; preserve operator settings and Git metadata.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}},
     {"type":"function","function":{"name":"run_checks","description":"Start the operator-configured validation commands sequentially. If unfinished, returns running=true and command_id; use command_status to inspect or wait. A running check is not a passing check. Long-running checks are reviewed by a watchdog rather than automatically killed.","parameters":{"type":"object","properties":{}}}}
    ]);
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::dev_tools::schemas());
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::image_tools::schemas());
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::tool_requests::schemas());
    tools.as_array_mut().unwrap().push(crate::symbols::schema());
    tools.as_array_mut().unwrap().push(crate::search::schema());
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::history::schemas());
    tools.as_array_mut().unwrap().push(json!({"type":"function","function":{"name":"read_task_evidence","description":"Read the active task, validation evidence tagged with the checked file state, attempted actions, separate tool errors and older task notes. Check current_files_match before trusting a previous validation result.","parameters":{"type":"object","properties":{}}}}));
    // Reading old reports never authorizes a new helper request.
    tools
        .as_array_mut()
        .unwrap()
        .push(crate::agents::schemas().remove(1));
    if crate::web_tools::enabled() {
        tools
            .as_array_mut()
            .unwrap()
            .extend(crate::web_tools::schemas());
    }
    tools
}
pub(crate) fn project_tools(root: &Path) -> Value {
    let mut tools = tools();
    let rust_project = root.join("Cargo.toml").is_file();
    tools.as_array_mut().unwrap().retain(|tool| {
        rust_project
            || !matches!(
                tool["function"]["name"].as_str(),
                Some("compiler_diagnostics" | "lookup_symbol")
            )
    });
    tools
}
#[cfg(test)]
mod tests {
    use super::*;
    #[derive(serde::Deserialize)]
    struct Goal {
        goal: String,
    }
    #[test]
    fn accepts_markdown_wrapped_json() {
        let response = "```json\n{\n  \"goal\": \"Match Word 2016 capabilities.\"\n}\n```";
        let goal: Goal = parse_reply(response).unwrap();
        assert_eq!(goal.goal, "Match Word 2016 capabilities.");
    }
    #[test]
    fn rejects_missing_fields_and_truncated_json() {
        assert!(parse_reply::<Goal>("{}").is_err());
        assert!(parse_reply::<Goal>("```json\n{\"goal\":\"unfinished").is_err());
        assert_eq!(
            parse_reply::<Goal>("Intro\n{\"goal\": \"ok\"} trailing explanation")
                .unwrap()
                .goal,
            "ok"
        );
        assert!(parse_reply::<Goal>("{\"goal\":\"one\"} {\"goal\":\"two\"}").is_err());
    }
    #[test]
    fn detects_the_observed_reasoning_cycle() {
        let sentence = "Let me look at the font size formatting before creating a helper to fix the serializer. ";
        assert!(repetitive(&sentence.repeat(5)));
        assert!(!repetitive(sentence));
    }
}

// Some local models omit opening quotes on object keys even in JSON mode.
// Repair keys only; never rewrite string values or coerce booleans/numbers.
fn quote_object_keys(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut stack = Vec::new();
    let (mut string, mut escape, mut key) = (false, false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                string = false;
            }
            i += 1;
            continue;
        }
        if key && stack.last() == Some(&'{') && (c.is_ascii_alphabetic() || c == '_') {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let end = i;
            if chars.get(i) == Some(&'"') {
                i += 1;
            }
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            if chars.get(i) == Some(&':') {
                out.push('"');
                out.extend(&chars[start..end]);
                out.push('"');
                continue;
            }
            out.extend(&chars[start..i]);
            continue;
        }
        if !key && !stack.is_empty() && c.is_ascii_alphabetic() {
            let end = (i..chars.len())
                .find(|&n| !chars[n].is_ascii_alphabetic())
                .unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            if matches!(
                word.as_str(),
                "accept" | "partial" | "repair" | "replan" | "rollback"
            ) {
                out.push('"');
                out.push_str(&word);
                out.push('"');
                i = end;
                continue;
            }
        }
        match c {
            '"' => string = true,
            '{' => {
                stack.push(c);
                key = true;
            }
            '[' => {
                stack.push(c);
                key = false;
            }
            '}' | ']' => {
                stack.pop();
                key = false;
            }
            ',' => key = stack.last() == Some(&'{'),
            ':' => key = false,
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod formatting_tests {
    use super::*;
    #[test]
    fn shared_json_constraints_are_not_a_reasoning_loop() {
        let constraint = "DOCX/RTF/PDF serialization, UI, persistence, collaboration, spell-check, track changes, mail merge";
        let tasks = serde_json::to_string_pretty(&json!({"tasks": (0..5).map(|n| json!({"title":format!("Step {n}"),"out_of_scope":[constraint]})).collect::<Vec<_>>()})).unwrap();
        assert!(!repetitive(&tasks));
        assert!(repetitive(&format!("{constraint}\n").repeat(5)));
    }
    #[test]
    fn repairs_keys_without_touching_values() {
        #[derive(serde::Deserialize)]
        struct Reply {
            decision: String,
            reason: String,
            criteria: Vec<Value>,
        }
        let r: Reply = parse_reply(
            r#"{decision:"rollback",reason":"text with {x: y} stays intact",criteria:[]}"#,
        )
        .unwrap();
        assert_eq!(r.decision, "rollback");
        assert_eq!(r.reason, "text with {x: y} stays intact");
        assert!(r.criteria.is_empty());
        let bare: Reply = parse_reply(
            r#"{"decision": rollback, "reason": "accept is only text here", "criteria": []}"#,
        )
        .unwrap();
        assert_eq!(bare.decision, "rollback");
        assert_eq!(bare.reason, "accept is only text here");
    }
}

pub fn goal_completion_tool() -> Value {
    json!({"type":"function","function":{"name":"finish_project","description":"Mark the overall project goal complete when it has been fully achieved and verified. Completing an individual task or nudge does not complete the project. This saves work and stops the loop.","parameters":{"type":"object","properties":{"summary":{"type":"string"},"evidence":{"type":"string"}},"required":["summary","evidence"]}}})
}

#[cfg(test)]
mod groq_transport_tests {
    use super::*;
    #[test]
    fn groq_transport_and_quota_controls() {
        for case in ["stream", "wait"] {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("chuggin");
            std::fs::create_dir_all(&base).unwrap();
            std::fs::write(base.join("groq.key"), "fixture-secret-not-real").unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "model::groq_transport_tests::groq_worker",
                    "--ignored",
                ])
                .env("XDG_CONFIG_HOME", dir.path())
                .env("CHUGGIN_GROQ_FIXTURE", case)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    #[test]
    #[ignore = "Isolated mock fixture launched by groq_transport_and_quota_controls"]
    fn groq_worker() {
        let case = std::env::var("CHUGGIN_GROQ_FIXTURE").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut model =
            Model::new("http://unused", "groq/fixture", 8192, 512, stop.clone()).unwrap();
        model.url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let messages = vec![json!({"role":"user","content":"fixture"})];
        if case == "wait" {
            let limits = crate::groq::Limits {
                requests_per_minute: 1,
                ..Default::default()
            };
            crate::setup::save(&crate::groq::path("groq-limits.json").unwrap(), &limits).unwrap();
            assert!(matches!(
                crate::groq::admit("groq/fixture", &messages, None, 512).unwrap(),
                crate::groq::Admission::Ready(_)
            ));
            let controls = model.controls.clone();
            let thread = std::thread::spawn(move || {
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_millis(120));
                    controls.retry_now();
                }
                controls.toggle_pause();
                std::thread::sleep(Duration::from_millis(120));
                controls.resume();
                stop.store(true, Ordering::SeqCst);
            });
            let error = model.chat(&messages, None, false).unwrap_err();
            assert!(error.downcast_ref::<crate::provider::Stopped>().is_some());
            thread.join().unwrap();
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            return;
        }
        let thread = std::thread::spawn(move || {
            for interrupted in [false, true] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                        size = n.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes).unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["model"], "fixture");
                assert!(body.get("options").is_none());
                assert!(body.get("think").is_none());
                let mut data = format!(
                    "data: {}\n\n",
                    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]})
                );
                if !interrupted {
                    data += &format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"file\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":20}})
                    );
                }
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",data.len(),data).unwrap();
            }
        });
        let trace = tempfile::tempdir().unwrap();
        model.trace_to(trace.path());
        let result = model.chat_format_once(&messages, None, None).unwrap();
        assert_eq!(
            result["tool_calls"][0]["function"]["arguments"],
            json!({"path":"file"})
        );
        assert!(model.chat_format_once(&messages, None, None).is_err());
        thread.join().unwrap();
        for entry in std::fs::read_dir(trace.path()).unwrap().flatten() {
            let s = std::fs::read_to_string(entry.path()).unwrap();
            assert!(!s.contains("fixture-secret-not-real"));
        }
    }
}

#[cfg(test)]
mod reasoning_transport_tests {
    use super::*;

    fn fixture(responses: Vec<Vec<Value>>) -> (Model, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for frames in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(size) = line.to_lowercase().strip_prefix("content-length:") {
                        length = size.trim().parse().unwrap();
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                let request: Value = serde_json::from_slice(&request).unwrap();
                assert!(!request.to_string().contains("PRIVATE_REASONING"));
                let response: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let model = Model::new(
            &url,
            "reasoning-fixture",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        (model, server)
    }

    fn completed(content: &str, thinking: &str) -> Vec<Value> {
        vec![
            json!({"message":{"thinking":thinking},"done":false}),
            json!({"message":{"content":content},"done":true,"done_reason":"stop"}),
        ]
    }

    #[test]
    fn reasoning_is_drained_once_and_is_not_sent_in_later_requests() {
        let (model, server) = fixture(vec![
            vec![
                json!({"message":{"thinking":"PRIVATE_REASONING first "},"done":false}),
                json!({"message":{"thinking":"continuation","content":"first reply","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"fixture.rs"}}}]},"done":true,"done_reason":"stop"}),
            ],
            completed("second reply", "PRIVATE_REASONING second"),
            completed("third reply", ""),
        ]);
        let mut messages = vec![json!({"role":"user","content":"fixture"})];
        let first = model.chat(&messages, None, false).unwrap();
        assert!(first.get("thinking").is_none());
        assert_eq!(first["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            model.take_completed_reasoning(),
            "PRIVATE_REASONING first continuation"
        );
        assert!(model.take_completed_reasoning().is_empty());
        assert!(
            !serde_json::to_string(&model.take_completed_messages())
                .unwrap()
                .contains("PRIVATE_REASONING")
        );
        messages.push(first);
        let second = model.chat(&messages, None, false).unwrap();
        assert_eq!(
            &*model.completed_reasoning.borrow(),
            "PRIVATE_REASONING second"
        );
        // An unconsumed prior thought disappears even when the next reply has none.
        messages.push(second);
        model.chat(&messages, None, false).unwrap();
        assert!(model.take_completed_reasoning().is_empty());
        server.join().unwrap();
        *model.completed_reasoning.borrow_mut() = "PRIVATE_REASONING stale".into();
        model.stop.store(true, Ordering::SeqCst);
        assert!(model.chat(&messages, None, false).is_err());
        assert!(model.take_completed_reasoning().is_empty());
    }

    #[test]
    fn watchdog_and_incomplete_responses_do_not_leave_reasoning() {
        let (model, server) = fixture(vec![
            completed("before watchdog", "PRIVATE_REASONING previous main"),
            completed("watchdog reply", "PRIVATE_REASONING watchdog"),
            vec![
                json!({"message":{"thinking":"PRIVATE_REASONING unfinished","content":"draft"},"done":false}),
            ],
            completed("after failure", "PRIVATE_REASONING new main"),
        ]);
        let messages = vec![json!({"role":"user","content":"fixture"})];
        model.chat(&messages, None, false).unwrap();
        assert!(!model.completed_reasoning.borrow().is_empty());
        model.watchdog_chat(&messages, json!([])).unwrap();
        assert!(model.take_completed_reasoning().is_empty());
        assert!(model.chat(&messages, None, false).is_err());
        assert!(model.take_completed_reasoning().is_empty());
        model.chat(&messages, None, false).unwrap();
        assert_eq!(
            model.take_completed_reasoning(),
            "PRIVATE_REASONING new main"
        );
        server.join().unwrap();
    }
}

#[cfg(test)]
mod vision_transport_tests {
    use super::*;
    fn request(socket: &std::net::TcpStream) -> (String, Value) {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let mut size = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(length) = line.to_lowercase().strip_prefix("content-length:") {
                size = length.trim().parse().unwrap();
            }
        }
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).unwrap();
        (first, serde_json::from_slice(&bytes).unwrap())
    }
    fn respond(socket: &mut std::net::TcpStream, status: &str, value: Value) {
        let text = format!("{value}\n");
        write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len()).unwrap();
    }
    #[test]
    fn native_vision_capability_is_cached_and_traces_hold_only_artifact_refs() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (path, body) = request(&socket);
                if index == 0 {
                    assert!(path.contains("/api/show"));
                    respond(
                        &mut socket,
                        "200 OK",
                        json!({"capabilities":["completion","tools","vision"]}),
                    );
                } else {
                    assert!(path.contains("/api/chat"));
                    assert_eq!(body["messages"][3]["role"], "user");
                    assert!(body["messages"][3]["images"][0].as_str().is_some());
                    respond(
                        &mut socket,
                        "200 OK",
                        json!({"message":{"role":"assistant","content":"Observed red"},"done":true,"done_reason":"stop"}),
                    );
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let picture = directory.path().join("image.png");
        image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]))
            .save(&picture)
            .unwrap();
        let model = Model::new(
            &url,
            "vision-fixture",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.trace_to(directory.path());
        let mut conversation = vec![
            json!({"role":"user","content":"Inspect the image"}),
            json!({"role":"assistant","tool_calls":[{"id":"img","function":{"name":"view_image","arguments":{"path":"image.png"}}}],"content":""}),
            crate::vision::tool_reply(
                "view_image",
                &json!({"_chuggin_images":[{"path":picture,"mime_type":"image/png","width":2,"height":2}]}),
                Some(&json!("img")),
            ),
        ];
        model.chat(&conversation, None, false).unwrap();
        conversation = model.take_completed_messages().unwrap();
        assert_eq!(conversation[2]["_chuggin_images_observed"], true);
        model.chat(&conversation, None, false).unwrap();
        server.join().unwrap();
        for name in ["request-000.json", "request-001.json"] {
            let trace: Value =
                serde_json::from_slice(&std::fs::read(directory.path().join(name)).unwrap())
                    .unwrap();
            assert!(trace.get("_chuggin_image_artifacts").is_some());
            assert!(trace["messages"][3].is_null());
        }
    }
    #[test]
    #[ignore = "Requires user-authorized live Ollama vision server"]
    fn live_ollama_image_observation() {
        let url = std::env::var("CHUGGIN_VISION_URL").expect("Set CHUGGIN_VISION_URL");
        let name = std::env::var("CHUGGIN_VISION_MODEL").expect("Set CHUGGIN_VISION_MODEL");
        let directory = tempfile::tempdir().unwrap();
        let picture = directory.path().join("shapes.png");
        let mut pixels = image::RgbImage::from_pixel(320, 160, image::Rgb([255, 255, 255]));
        // Two shapes: red square left, green circle right; no labels reveal colors.
        for x in 25..125 {
            for y in 30..130 {
                pixels.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        for x in 180i32..300 {
            for y in 20i32..140 {
                if (x - 240).pow(2) + (y - 80).pow(2) <= 2500 {
                    pixels.put_pixel(x as u32, y as u32, image::Rgb([0, 180, 0]));
                }
            }
        }
        pixels.save(&picture).unwrap();
        let model = Model::new(&url, &name, 8192, 512, Arc::new(AtomicBool::new(false))).unwrap();
        model.trace_to(directory.path());
        let conversation = vec![
            json!({"role":"user","content":"Describe the shapes in the tool's image: color, shape, and left/right location. Answer briefly from the actual pixels."}),
            json!({"role":"assistant","tool_calls":[{"id":"img","function":{"name":"view_image","arguments":{"path":"shapes.png"}}}],"content":""}),
            crate::vision::tool_reply(
                "view_image",
                &json!({"_chuggin_images":[{"path":picture,"mime_type":"image/png","width":320,"height":160}]}),
                Some(&json!("img")),
            ),
        ];
        let response = model.chat(&conversation, None, false).unwrap();
        let text = response["content"].as_str().unwrap_or("").to_lowercase();
        println!("Live vision observation: {text}");
        assert!(
            text.contains("red")
                && text.contains("green")
                && text.contains("left")
                && text.contains("right")
                && text.contains("square")
                && text.contains("circle")
        );
        let saved = model.take_completed_messages().unwrap();
        let (encoded, _) = crate::vision::encoded_image(&saved[2]["_chuggin_images"][0]).unwrap();
        assert!(!serde_json::to_string(&saved).unwrap().contains(&encoded));
        let trace = std::fs::read_to_string(directory.path().join("request-000.json")).unwrap();
        assert!(!trace.contains(&encoded));
    }
    #[test]
    fn image_rejection_recovers_once_as_explicit_text_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (_, body) = request(&socket);
                match index {
                    0 => respond(&mut socket, "200 OK", json!({})), // old server: capability unknown
                    1 => respond(
                        &mut socket,
                        "400 Bad Request",
                        json!({"error":"model does not support images"}),
                    ),
                    _ => {
                        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
                        assert!(
                            body["messages"][2]["content"]
                                .as_str()
                                .unwrap()
                                .contains("Image observation failed")
                        );
                        respond(
                            &mut socket,
                            "200 OK",
                            json!({"message":{"role":"assistant","content":"I cannot see it"},"done":true,"done_reason":"stop"}),
                        );
                    }
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let picture = directory.path().join("image.png");
        image::RgbImage::new(2, 2).save(&picture).unwrap();
        let model = Model::new(
            &url,
            "text-fixture",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let conversation = vec![
            json!({"role":"user","content":"Inspect the image"}),
            json!({"role":"assistant","tool_calls":[{"id":"img","function":{"name":"view_image","arguments":{"path":"image.png"}}}],"content":""}),
            crate::vision::tool_reply(
                "view_image",
                &json!({"_chuggin_images":[{"path":picture,"mime_type":"image/png","width":2,"height":2}]}),
                Some(&json!("img")),
            ),
        ];
        assert_eq!(
            model.chat(&conversation, None, false).unwrap()["content"],
            "I cannot see it"
        );
        assert!(!crate::vision::has_images(
            &model.take_completed_messages().unwrap()
        ));
        server.join().unwrap();
    }
}
