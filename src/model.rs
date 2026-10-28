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

fn reasoning_output_limit(
    configured: u32,
    cap: Option<u32>,
    setting: &crate::reasoning::Setting,
) -> Result<u32> {
    let cap = cap.map(|cap| {
        if let crate::reasoning::Setting::TokenBudget(tokens) = setting {
            // Short watchdog/diagnostic replies still need room for the user's
            // explicit thinking budget; the configured allowance remains final.
            cap.max(tokens.saturating_add(2048))
        } else {
            cap
        }
    });
    let output = cap.map_or(configured, |cap| configured.min(cap));
    if let crate::reasoning::Setting::TokenBudget(tokens) = setting {
        anyhow::ensure!(
            *tokens < output,
            "Reasoning token budget ({tokens}) must be smaller than the response limit ({output}) so there is room for an answer. Raise the response limit or reduce the reasoning budget."
        );
    }
    Ok(output)
}

pub struct Model {
    client: Client,
    url: String,
    name: String,
    context: u32,
    output: u32,
    output_cap: Cell<Option<u32>>,
    watchdog_call: Cell<bool>,
    observer_call: Cell<bool>,
    stop: Arc<AtomicBool>,
    trace: RefCell<Option<PathBuf>>,
    sequence: Cell<u32>,
    settings_path: Option<PathBuf>,
    chat_settings: Option<PathBuf>,
    completed_messages: RefCell<Option<Vec<Value>>>,
    completed_reasoning: RefCell<String>,
    last_context_usage: RefCell<Option<crate::context_usage::Usage>>,
    context_pressure: Cell<bool>,
    run_controls: Option<(Arc<AtomicBool>, Instant)>,
    pub(crate) controls: Arc<crate::run_control::RunControl>,
    request_target: RefCell<(String, String)>,
    pinned_target: Option<(String, String, PathBuf)>,
    vision_capabilities: RefCell<HashMap<(String, String), bool>>,
    reasoning: crate::reasoning::Setting,
    sampling: crate::sampling::Settings,
    reasoning_role: ReasoningRole,
    reasoning_capabilities:
        RefCell<HashMap<(String, String), (Instant, crate::reasoning::Capabilities)>>,
    sampling_capabilities:
        RefCell<HashMap<(String, String), (Instant, crate::sampling::Capabilities)>>,
    request_timeout_seconds: u64,
    #[cfg(test)]
    responses_test_token: Option<String>,
}
#[derive(Clone, Copy)]
enum ReasoningRole {
    Loop,
    Chat,
    Helper,
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
            url: crate::cloud::url(url, name),
            name: name.into(),
            context,
            output,
            output_cap: Cell::new(None),
            watchdog_call: Cell::new(false),
            observer_call: Cell::new(false),
            stop,
            trace: RefCell::new(None),
            sequence: Cell::new(0),
            settings_path: None,
            chat_settings: None,
            completed_messages: RefCell::new(None),
            completed_reasoning: RefCell::new(String::new()),
            last_context_usage: RefCell::new(None),
            context_pressure: Cell::new(false),
            run_controls: None,
            controls: Arc::new(crate::run_control::RunControl::default()),
            request_target: RefCell::new((name.into(), crate::cloud::url(url, name))),
            pinned_target: None,
            vision_capabilities: RefCell::new(HashMap::new()),
            reasoning: crate::reasoning::Setting::default(),
            sampling: crate::sampling::Settings::default(),
            reasoning_role: ReasoningRole::Loop,
            reasoning_capabilities: RefCell::new(HashMap::new()),
            sampling_capabilities: RefCell::new(HashMap::new()),
            request_timeout_seconds: 1800,
            #[cfg(test)]
            responses_test_token: None,
        })
    }
    pub fn set_reasoning(&mut self, setting: crate::reasoning::Setting) {
        self.reasoning = setting;
    }
    pub fn set_sampling(&mut self, setting: crate::sampling::Settings) {
        self.sampling = setting;
    }
    pub fn set_request_timeout(&mut self, seconds: u64) {
        self.request_timeout_seconds = seconds;
    }
    fn chatgpt_token(&self, url: &str) -> Result<String> {
        #[cfg(test)]
        if let Some(token) = &self.responses_test_token {
            return Ok(token.clone());
        }
        anyhow::ensure!(
            url == "https://api.openai.com/v1/responses",
            "ChatGPT plan credentials can only be sent to the official Responses endpoint"
        );
        crate::chatgpt::access_token()
    }
    fn renew_chatgpt_token(&self, rejected: &str) -> Result<String> {
        #[cfg(test)]
        anyhow::ensure!(
            self.responses_test_token.is_none(),
            "Mock ChatGPT token was rejected"
        );
        crate::chatgpt::refresh_access_token(rejected)
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
        if crate::chatgpt::model_id(name).is_some() {
            let supported = crate::chatgpt::model_info(name)
                .ok()
                .and_then(|metadata| {
                    metadata["supports_images"].as_bool().or_else(|| {
                        ["input_modalities", "supported_input_modalities"]
                            .into_iter()
                            .find_map(|field| {
                                metadata[field].as_array().map(|modalities| {
                                    modalities.iter().any(|value| value == "image")
                                })
                            })
                    })
                })
                .unwrap_or(true);
            self.vision_capabilities
                .borrow_mut()
                .insert(cache_key, supported);
            return supported;
        }
        if crate::cloud::provider(name).is_some() {
            // Cloud catalog capabilities are validated before the request. If
            // an endpoint rejects an image, the ordinary text recovery applies.
            return true;
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
    /// Usage belongs to the last call, including a response that reached its limit.
    pub(crate) fn take_context_usage(&self) -> Option<crate::context_usage::Usage> {
        self.last_context_usage.borrow_mut().take()
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
        self.reasoning_role = ReasoningRole::Chat;
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
    fn selected_reasoning<'a>(
        &'a self,
        c: &'a crate::runner::Config,
    ) -> &'a crate::reasoning::Setting {
        match self.reasoning_role {
            ReasoningRole::Loop => &c.reasoning,
            ReasoningRole::Chat => &c.chat_reasoning,
            ReasoningRole::Helper => &c.helpers.reasoning,
        }
    }
    fn selected_sampling<'a>(
        &'a self,
        c: Option<&'a crate::runner::Config>,
    ) -> &'a crate::sampling::Settings {
        // Creative project profiles must not alter the observers which diagnose
        // repetition or decide whether a command should continue running.
        if self.observer_call.get() {
            return &crate::sampling::Settings::Legacy;
        }
        let Some(c) = c else {
            return &self.sampling;
        };
        match self.reasoning_role {
            ReasoningRole::Loop => &c.sampling,
            ReasoningRole::Chat => &c.chat_sampling,
            ReasoningRole::Helper => &c.helpers.sampling,
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
        helper.reasoning_role = ReasoningRole::Helper;
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
                crate::cloud::url(&c.ollama_url, self.selected_name(&c)),
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
        let observer = self.observer_call.replace(true);
        let result = self.chat(messages, Some(tools), false);
        self.output_cap.set(previous);
        self.observer_call.set(observer);
        result
    }
    pub fn watchdog_chat(&self, messages: &[Value], tools: Value) -> Result<Value> {
        let previous = self.output_cap.replace(Some(2048));
        let mode = self.watchdog_call.replace(true);
        let observer = self.observer_call.replace(true);
        *self.completed_messages.borrow_mut() = None;
        self.completed_reasoning.borrow_mut().clear();
        // Observers must not disappear into an indefinite provider quota retry.
        let result = self.chat_format_once(messages, Some(tools), None);
        self.output_cap.set(previous);
        self.watchdog_call.set(mode);
        self.observer_call.set(observer);
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
        *self.last_context_usage.borrow_mut() = None;
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
        if self
            .last_context_usage
            .borrow()
            .as_ref()
            .is_some_and(|usage| usage.model != name)
        {
            *self.last_context_usage.borrow_mut() = None;
        }
        let url = live
            .as_ref()
            .map(|c| crate::cloud::url(&c.ollama_url, self.selected_name(c)))
            .unwrap_or_else(|| self.url.clone());
        let url = self
            .pinned_target
            .as_ref()
            .map_or(url, |(_, url, _)| url.clone());
        let timeout = live
            .as_ref()
            .map(|c| c.request_timeout_seconds)
            .unwrap_or(self.request_timeout_seconds);
        let timeout = if self.watchdog_call.get() {
            if timeout == 0 { 120 } else { timeout.min(120) }
        } else {
            timeout
        };
        let groq = crate::groq::model_id(name);
        let cloud = crate::cloud::provider(name);
        let chatgpt = crate::chatgpt::model_id(name);
        let remote = cloud.is_some() || chatgpt.is_some();
        let openai = groq.is_some() || remote;
        // Metadata refresh errors belong to this target as well. Recording a
        // previous model here would let a live switch bypass provider backoff.
        *self.request_target.borrow_mut() = (name.to_owned(), url.clone());
        let reasoning = live
            .as_ref()
            .map(|c| self.selected_reasoning(c))
            .unwrap_or(&self.reasoning);
        let sampling = self.selected_sampling(live.as_ref());
        let sampling_capabilities = if sampling.has_override() {
            let cache_key = (url.clone(), name.to_owned());
            let cached = self
                .sampling_capabilities
                .borrow()
                .get(&cache_key)
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(300))
                .map(|(_, capabilities)| capabilities.clone());
            let capabilities = match cached {
                Some(capabilities) => capabilities,
                None => {
                    let capabilities = crate::sampling::discover(&url, name)?;
                    self.sampling_capabilities
                        .borrow_mut()
                        .insert(cache_key, (Instant::now(), capabilities.clone()));
                    capabilities
                }
            };
            capabilities.validate_selection(sampling)?;
            Some(capabilities)
        } else {
            None
        };
        let thinking_capabilities = if reasoning.is_default() {
            None
        } else {
            let cache_key = (url.clone(), name.to_owned());
            let cached = self
                .reasoning_capabilities
                .borrow()
                .get(&cache_key)
                .filter(|(at, _)| at.elapsed() < Duration::from_secs(300))
                .map(|(_, capabilities)| capabilities.clone());
            let capabilities = match cached {
                Some(capabilities) => capabilities,
                None => {
                    let capabilities = crate::reasoning::discover(&url, name)?;
                    self.reasoning_capabilities
                        .borrow_mut()
                        .insert(cache_key, (Instant::now(), capabilities.clone()));
                    capabilities
                }
            };
            capabilities.validate_selection(reasoning)?;
            Some(capabilities)
        };
        // Validate/read credentials before reserving capacity. Never include them in traces.
        let key = if chatgpt.is_some() {
            Some(self.chatgpt_token(&url)?)
        } else if groq.is_some() {
            Some(crate::groq::key()?)
        } else {
            crate::cloud::key(name)?
        };
        if cloud.is_some() {
            anyhow::ensure!(
                cloud != Some(crate::cloud::Provider::OpenRouter) || key.is_some(),
                "Add your OpenRouter API key in Global settings → OpenRouter connection"
            );
            crate::cloud::validate(name)?;
        }
        let output = live
            .as_ref()
            .map(|c| c.output_tokens)
            .unwrap_or(self.output);
        let context = live
            .as_ref()
            .map(|c| c.context_tokens)
            .unwrap_or(self.context);
        let output = if groq.is_some() {
            output.min(crate::groq::Limits::load()?.max_response_tokens)
        } else {
            output
        };
        let output = reasoning_output_limit(output, self.output_cap.get(), reasoning)?;
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
        let wire_messages = if !openai {
            crate::vision::ollama_messages(messages)?
        } else {
            messages.to_vec()
        };
        let mut body = json!({"model":name,"messages":wire_messages,"stream":true,"options":{"num_ctx":context,"num_predict":output}});
        if let Some(t) = tools {
            body["tools"] = t;
        }
        if let Some(schema) = format {
            body["format"] = schema;
        }
        if chatgpt.is_some() {
            body =
                crate::responses::request(name, messages, body.get("tools"), body.get("format"))?;
        } else if let Some(id) = groq.or_else(|| crate::cloud::model_id(name)) {
            let formatted = body.get("format").is_some();
            let wire_messages = if cloud.is_some() {
                crate::openai::messages_for(messages, name)?
            } else {
                crate::openai::messages(messages)?
            };
            let mut wire = json!({"model":id,"messages":wire_messages,"stream":true,"stream_options":{"include_usage":true}});
            wire[if groq.is_some() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }] = json!(output);
            if cloud == Some(crate::cloud::Provider::OpenRouter) {
                // Never route this free integration to a paid endpoint, even
                // if a model's preview pricing changes between requests.
                wire["provider"] =
                    json!({"max_price":{"prompt":0,"completion":0,"request":0,"image":0}});
            }
            if let Some(t) = body.get("tools") {
                wire["tools"] = t.clone();
            }
            if formatted && (groq.is_some() || crate::cloud::supports_json(name)?) {
                wire["response_format"] = json!({"type":"json_object"});
            }
            body = wire;
        }
        crate::reasoning::apply(&mut body, name, reasoning, thinking_capabilities.as_ref())?;
        let resolved_sampling =
            crate::sampling::apply(&mut body, name, sampling, sampling_capabilities.as_ref())?;
        let sequence = self.sequence.get();
        self.sequence.set(sequence + 1);
        let mut trace = if let Some(path) = self.trace.borrow().as_ref() {
            std::fs::write(
                path.join(format!("connection-{sequence:03}.json")),
                serde_json::to_vec_pretty(
                    &json!({"model":name,"request_timeout_seconds":timeout,"reasoning":reasoning,"sampling":sampling,"sampling_resolved":resolved_sampling}),
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
        let cloud_client = if openai {
            Some(crate::groq::client()?)
        } else {
            None
        };
        let client = cloud_client.as_ref().unwrap_or(&self.client);
        let request = client.post(url).json(&body);
        let request = if let Some(key) = key.as_deref() {
            request.bearer_auth(key)
        } else {
            request
        };
        let request = if remote {
            request.header(
                reqwest::header::USER_AGENT,
                concat!("chuggin/", env!("CARGO_PKG_VERSION")),
            )
        } else {
            request
        };
        let request = if timeout == 0 {
            request
        } else {
            request.timeout(Duration::from_secs(timeout))
        };
        let response = if chatgpt.is_some() {
            send_responses_request(
                client,
                &self.request_target.borrow().1,
                &body,
                key.as_deref().context("Missing ChatGPT access token")?,
                timeout,
                |rejected| self.renew_chatgpt_token(rejected),
            )
        } else {
            request.send().map_err(anyhow::Error::from)
        }
        .map_err(|error| {
            if remote && error.downcast_ref::<reqwest::Error>().is_some() {
                error.context(crate::provider::Unavailable {
                    reason: "Cloud provider connection failed or timed out",
                    retry_after: None,
                })
            } else {
                error
            }
        })?;
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
            if let Err(error) = response.take(16384).read_to_string(&mut body) {
                if remote {
                    if let Some(provider) = crate::provider::classify(status, "", retry_after) {
                        return Err(provider.into());
                    }
                    return Err(
                        anyhow::Error::from(error).context(crate::provider::Unavailable {
                            reason: "Cloud provider error response was interrupted",
                            retry_after,
                        }),
                    );
                }
                return Err(error.into());
            }
            if matches!(status, 400 | 422)
                && crate::vision::has_images(messages)
                && ["image", "vision", "multimodal"]
                    .iter()
                    .any(|word| body.to_lowercase().contains(word))
            {
                return Err(VisionRejected.into());
            }
            if chatgpt.is_some()
                && let Ok(value) = serde_json::from_str::<Value>(&body)
                && value["error"]["code"].as_str().is_some()
            {
                let mut error = crate::responses::error(&value);
                if let Some(unavailable) = error.downcast_mut::<crate::provider::Unavailable>() {
                    unavailable.retry_after = retry_after;
                }
                return Err(error);
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
        let mut stream = crate::openai::Stream::default();
        let mut responses = crate::responses::Stream::default();
        let mut finish_reason = String::new();
        for line in BufReader::new(response).lines() {
            anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
            let line = line.map_err(|error| {
                let error = anyhow::Error::from(error);
                if remote {
                    error.context(crate::provider::Unavailable {
                        reason: "Cloud provider stream interrupted",
                        retry_after: None,
                    })
                } else {
                    error
                }
            })?;
            if let Some(file) = trace.as_mut() {
                writeln!(file, "{line}")?;
            }
            if line.is_empty() {
                continue;
            }
            let d: Value = if chatgpt.is_some() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let previous = responses.usage;
                let parsed = responses.frame(data);
                if responses.usage != previous
                    && let Some((prompt, generated)) = responses.usage
                {
                    if prompt > 0 {
                        *self.last_context_usage.borrow_mut() = Some(crate::context_usage::Usage {
                            prompt,
                            model: name.to_owned(),
                            context_tokens: context,
                            recorded_at: Some(chrono::Utc::now().to_rfc3339()),
                        });
                    }
                    self.context_pressure.set(
                        prompt.saturating_add(generated)
                            >= context.saturating_sub(output.saturating_add(1024)) as u64,
                    );
                    crate::events::send(crate::events::Event::Metrics {
                        prompt,
                        generated,
                        seconds: started.elapsed().as_secs_f64(),
                    });
                }
                parsed.map_err(|mut error| {
                    if let Some(unavailable) = error.downcast_mut::<crate::provider::Unavailable>()
                    {
                        unavailable.retry_after = unavailable.retry_after.or(retry_after);
                    }
                    error
                })?
            } else if openai {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    anyhow::ensure!(
                        !finish_reason.is_empty(),
                        "Cloud stream ended without a finish reason"
                    );
                    if let Some((prompt, generated)) = stream.usage {
                        if let Some(r) = &reservation {
                            r.settle(prompt, generated)?;
                        }
                        self.context_pressure.set(
                            prompt + generated
                                >= context.saturating_sub(output.saturating_add(1024)) as u64,
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
                        "Cloud response ended with {finish_reason}"
                    );
                    calls = stream.calls()?;
                    anyhow::ensure!(
                        finish_reason != "tool_calls" || !calls.is_empty(),
                        "Cloud finished tool calls without a complete call"
                    );
                    done = true;
                    break;
                }
                let prior_usage = stream.usage;
                let parsed = stream.frame(data);
                if stream.usage != prior_usage
                    && let Some((prompt, _)) = stream.usage.filter(|(prompt, _)| *prompt > 0)
                {
                    *self.last_context_usage.borrow_mut() = Some(crate::context_usage::Usage {
                        prompt,
                        model: name.to_owned(),
                        context_tokens: context,
                        recorded_at: Some(chrono::Utc::now().to_rfc3339()),
                    });
                }
                let frame = parsed?;
                if let Some(reason) = frame["done_reason"].as_str() {
                    finish_reason = reason.into();
                }
                frame
            } else {
                serde_json::from_str(&line).context("Invalid Ollama stream JSON")?
            };
            if let Some(e) = d.get("error") {
                let code = e["code"]
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .unwrap_or(200);
                if let Some(provider) = crate::provider::classify(code, &e.to_string(), retry_after)
                {
                    return Err(provider.into());
                }
                bail!("Model provider stream error: {e}")
            }
            if let Some(s) = d["message"]["thinking"].as_str() {
                thinking.push_str(s);
                if !s.is_empty() {
                    crate::events::send(crate::events::Event::ThinkingDelta(s.into()));
                }
            }
            if let Some(s) = d["message"]["content"].as_str() {
                content.push_str(s);
                if !s.is_empty() {
                    crate::events::send(crate::events::Event::Delta(s.into()));
                }
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
                if let Some(prompt) = d["prompt_eval_count"].as_u64().filter(|prompt| *prompt > 0) {
                    *self.last_context_usage.borrow_mut() = Some(crate::context_usage::Usage {
                        prompt,
                        model: name.to_owned(),
                        context_tokens: context,
                        recorded_at: Some(chrono::Utc::now().to_rfc3339()),
                    });
                }
                let used = d["prompt_eval_count"].as_u64().unwrap_or(0)
                    + d["eval_count"].as_u64().unwrap_or(0);
                if chatgpt.is_none() {
                    self.context_pressure
                        .set(used >= context.saturating_sub(output.saturating_add(1024)) as u64);
                    crate::events::send(crate::events::Event::Metrics {
                        prompt: d["prompt_eval_count"].as_u64().unwrap_or(0),
                        generated: d["eval_count"].as_u64().unwrap_or(0),
                        seconds: d["eval_duration"].as_f64().unwrap_or(0.) / 1_000_000_000.,
                    });
                }
                if d["done_reason"] == "length" {
                    return Err(InterruptedResponse {
                        reason: "Model reached its generation limit; produce a shorter complete response",
                        prefix: String::new(),
                    }.into());
                }
                if chatgpt.is_some() {
                    calls = responses.calls()?;
                }
                done = true;
                break;
            }
        }
        if !done && remote {
            return Err(crate::provider::Unavailable {
                reason: "Cloud provider stream disconnected before completion",
                retry_after: None,
            }
            .into());
        }
        anyhow::ensure!(done, "Model stream disconnected before completion");
        let mut reply = json!({"role":"assistant","content":content,"tool_calls":calls});
        if chatgpt.is_some() && !self.watchdog_call.get() {
            let items = responses.items()?;
            if !items.is_empty() {
                reply["_chuggin_responses"] = json!({"model":name,"items":items});
            }
        }
        if cloud.is_some() && !self.watchdog_call.get() {
            reply["_chuggin_reasoning"] =
                json!({"model":name,"content":thinking,"details":stream.reasoning_details});
        }
        if !self.watchdog_call.get() {
            *self.completed_reasoning.borrow_mut() = thinking;
        }
        Ok(reply)
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

fn send_responses_request(
    client: &Client,
    url: &str,
    body: &Value,
    token: &str,
    timeout: u64,
    refresh: impl FnOnce(&str) -> Result<String>,
) -> Result<reqwest::blocking::Response> {
    let send = |token: &str| {
        let request = client.post(url).bearer_auth(token).json(body).header(
            reqwest::header::USER_AGENT,
            concat!("chuggin/", env!("CARGO_PKG_VERSION")),
        );
        let request = if timeout == 0 {
            request
        } else {
            request.timeout(Duration::from_secs(timeout))
        };
        request.send().map_err(anyhow::Error::from)
    };
    let response = send(token)?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(response);
    }
    // Only retry admission, before reading/acting on any streamed inference.
    // Passing the rejected token avoids duplicate concurrent token rotations.
    drop(response);
    let renewed = refresh(token)?;
    send(&renewed)
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
     {"type":"function","function":{"name":"save_progress_note","description":"Save a complete persistent progress note. Replace by default; append=true adds to it. Record findings, failed approaches, dependencies and next actions. Optional structured fields survive handoffs separately; read_context_memory retrieves them. read_progress_note retrieves the full freeform note. Notes are advisory, not proof. Empty replacement clears it.","parameters":{"type":"object","properties":{"note":{"type":"string"},"append":{"type":"boolean"},"observations":{"type":"array","items":{"type":"string"},"description":"Observed discoveries with source references when available; claims remain unverified."},"decisions":{"type":"array","items":{"type":"string"},"description":"Choices made and their reasons; these are not human instructions."},"reasons":{"type":"array","items":{"type":"string"}},"open_questions":{"type":"array","items":{"type":"string"}},"attempts":{"type":"array","items":{"type":"string"},"description":"Attempted approaches, outcomes and evidence references."}},"required":["note"]}}},
     {"type":"function","function":{"name":"read_context_memory","description":"Retrieve complete source-linked memory after a handoff, in UTF-8 byte pages. human contains delivered human messages and operator directions; episode contains model-authored unverified observations, decisions, reasons, open questions and attempts. Follow next_offset with the returned revision. Does not fetch unread mailbox messages or create a new goal.","parameters":{"type":"object","properties":{"section":{"type":"string","enum":["human","episode"]},"record_id":{"type":"string","description":"Optional exact record id from a memory preview."},"offset":{"type":"integer","minimum":0},"revision":{"type":"string","description":"Use the revision returned by the initial page for every continuation; restart at offset 0 if memory changed."}}}}},
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
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::messenger::schemas());
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::notebook::schemas());
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

#[cfg(test)]
mod sampling_transport_tests {
    use super::*;
    fn config(directory: &Path, url: &str) -> crate::runner::Config {
        serde_json::from_value(json!({
            "repo":directory,"goal":"A useful project","ollama_url":url,
            "model":"sampling-fixture","context_tokens":4096,"output_tokens":256,
            "implementation_calls":4,"checks":[{"argv":["true"],"timeout_seconds":10}],"state_dir":directory.join(".chuggin"),"retry_seconds":1
        })).unwrap()
    }
    #[test]
    fn live_sampling_applies_after_format_conversion_and_preserves_in_flight_selection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("chuggin.json");
        let trace = directory.path().join("trace");
        std::fs::create_dir(&trace).unwrap();
        let server_config = config_path.clone();
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(
                    request.contains("/api/chat"),
                    "Sampling must not trigger inference/metadata probes"
                );
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut raw = vec![0; length];
                reader.read_exact(&mut raw).unwrap();
                let body: Value = serde_json::from_slice(&raw).unwrap();
                match index {
                    0 => assert_eq!(body["options"]["temperature"], 0.4),
                    1 => {
                        assert_eq!(body["format"], "json");
                        assert_eq!(body["options"]["temperature"], 0.9);
                        // Settings change while this response is in flight. It
                        // remains valid and the next call sees the new setting.
                        let mut config: Value =
                            serde_json::from_slice(&std::fs::read(&server_config).unwrap())
                                .unwrap();
                        config["sampling"] = json!({"mode":"provider_default"});
                        crate::setup::save(&server_config, &config).unwrap();
                    }
                    2 => {
                        assert_eq!(body["format"], "json");
                        assert!(body["options"].get("temperature").is_none());
                    }
                    3 => {
                        assert_eq!(body["options"]["temperature"], 1.2);
                        assert_eq!(body["options"]["top_k"], 0);
                        assert_eq!(body["options"]["min_p"], 0.05);
                        assert_eq!(body["options"]["presence_penalty"], 0.3);
                        assert_eq!(body["options"]["frequency_penalty"], 0.2);
                        assert_eq!(body["options"]["seed"], 7);
                    }
                    _ => unreachable!(),
                }
                let reply = json!({"message":{"role":"assistant","content":"{\"ok\":true}"},"done":true,"done_reason":"stop"}).to_string()+"\n";
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        let mut config = config(directory.path(), &url);
        crate::setup::save(&config_path, &config).unwrap();
        let mut model = Model::new(
            &url,
            &config.model,
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.use_project_settings(&config_path);
        model.trace_to(&trace);
        let messages = [json!({"role":"user","content":"Return JSON"})];
        model.chat(&messages, None, false).unwrap();
        config.sampling = crate::sampling::Settings::Exploratory;
        crate::setup::save(&config_path, &config).unwrap();
        model.chat(&messages, None, true).unwrap();
        model.chat(&messages, None, true).unwrap();
        config.sampling = crate::sampling::Settings::Custom(crate::sampling::Values {
            temperature: Some(1.2),
            top_p: Some(0.75),
            top_k: Some(0),
            min_p: Some(0.05),
            repeat_penalty: Some(1.1),
            presence_penalty: Some(0.3),
            frequency_penalty: Some(0.2),
            seed: Some(7),
        });
        crate::setup::save(&config_path, &config).unwrap();
        model.chat(&messages, None, false).unwrap();
        server.join().unwrap();
        let first: Value =
            serde_json::from_slice(&std::fs::read(trace.join("connection-001.json")).unwrap())
                .unwrap();
        assert_eq!(first["sampling_resolved"]["overrides"]["temperature"], 0.9);
        let next: Value =
            serde_json::from_slice(&std::fs::read(trace.join("connection-002.json")).unwrap())
                .unwrap();
        assert_eq!(next["sampling_resolved"]["overrides"], json!({}));
    }
    #[test]
    fn role_sampling_is_independent_and_fresh_diagnostics_remain_legacy() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(directory.path(), "http://unused");
        config.sampling = crate::sampling::Settings::Focused;
        config.setup_sampling = crate::sampling::Settings::Exploratory;
        config.chat_sampling = crate::sampling::Settings::ProviderDefault;
        config.helpers.sampling = crate::sampling::Settings::Wildcard;
        let mut main = Model::new(
            "http://unused",
            "main",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(main.selected_sampling(Some(&config)), &config.sampling);
        main.use_chat_settings(&directory.path().join("chuggin.json"), directory.path());
        assert_eq!(main.selected_sampling(Some(&config)), &config.chat_sampling);
        let mut helper = Model::new(
            "http://unused",
            "helper",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        main.configure_helper(
            &mut helper,
            &directory.path().join("chuggin.json"),
            directory.path(),
            "helper",
            "http://unused",
        );
        assert_eq!(
            helper.selected_sampling(Some(&config)),
            &config.helpers.sampling
        );
        let mut setup = Model::new(
            "http://unused",
            "setup",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        setup.set_sampling(config.setup_sampling.clone());
        assert_eq!(setup.sampling, config.setup_sampling);
        let diagnostic = Model::new(
            "http://unused",
            "diagnostic",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(diagnostic.sampling, crate::sampling::Settings::Legacy);
    }
    #[test]
    fn chaos_sampling_does_not_leak_into_diagnostics_or_watchdogs() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (index, temperature) in [0.9, 0.4, 0.4, 0.9, 0.4, 0.9].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.contains("/api/chat"));
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut raw = vec![0; length];
                reader.read_exact(&mut raw).unwrap();
                let body: Value = serde_json::from_slice(&raw).unwrap();
                assert_eq!(
                    body["options"]["temperature"], temperature,
                    "request {index}"
                );
                let (status, reply) = if index == 4 {
                    (
                        "400 Bad Request",
                        json!({"error":"fixture observer failure"}).to_string(),
                    )
                } else {
                    ("200 OK", json!({"message":{"role":"assistant","content":"Done"},"done":true,"done_reason":"stop"}).to_string()+"\n")
                };
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chuggin.json");
        let trace = directory.path().join("trace");
        std::fs::create_dir(&trace).unwrap();
        let mut config = config(directory.path(), &url);
        config.activity_mode = crate::activity::Mode::Chaos;
        config.goal.clear();
        config.checks.clear();
        config.sampling = crate::sampling::Settings::Exploratory;
        config.chat_sampling = crate::sampling::Settings::Wildcard;
        config.helpers.sampling = crate::sampling::Settings::Wildcard;
        crate::setup::save(&path, &config).unwrap();
        let mut model = Model::new(
            &url,
            &config.model,
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.use_project_settings(&path);
        model.trace_to(&trace);
        let messages = [json!({"role":"user","content":"Observe the fixture"})];
        model.chat(&messages, None, false).unwrap();
        model.diagnostic_chat(&messages, json!([])).unwrap();
        model.watchdog_chat(&messages, json!([])).unwrap();
        model.chat(&messages, None, false).unwrap();
        assert!(model.diagnostic_chat(&messages, json!([])).is_err());
        model.chat(&messages, None, false).unwrap();
        server.join().unwrap();
        for (index, temperature) in [0.9, 0.4, 0.4, 0.9, 0.4, 0.9].into_iter().enumerate() {
            let connection: Value = serde_json::from_slice(
                &std::fs::read(trace.join(format!("connection-{index:03}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(
                connection["sampling_resolved"]["overrides"]["temperature"],
                temperature
            );
            assert_eq!(
                connection["sampling"]["mode"],
                if index == 1 || index == 2 || index == 4 {
                    "legacy"
                } else {
                    "exploratory"
                }
            );
        }
        // The same observer guard applies when a fresh model was explicitly
        // configured, rather than inheriting its sampler from project settings.
        model.settings_path = None;
        model.set_sampling(crate::sampling::Settings::Wildcard);
        model.observer_call.set(true);
        assert_eq!(
            model.selected_sampling(None),
            &crate::sampling::Settings::Legacy
        );
        model.observer_call.set(false);
        assert_eq!(
            model.selected_sampling(None),
            &crate::sampling::Settings::Wildcard
        );
    }
}

#[cfg(test)]
mod thinking_controls_transport_tests {
    use super::*;

    #[test]
    fn diagnostic_caps_preserve_a_valid_budget_and_reject_insufficient_configured_output() {
        let budget = crate::reasoning::Setting::TokenBudget(4096);
        assert_eq!(
            reasoning_output_limit(8192, Some(2048), &budget).unwrap(),
            6144
        );
        assert_eq!(
            reasoning_output_limit(5000, Some(2048), &budget).unwrap(),
            5000
        );
        assert!(
            reasoning_output_limit(4096, Some(2048), &budget)
                .unwrap_err()
                .to_string()
                .contains("Raise the response limit")
        );
        assert_eq!(
            reasoning_output_limit(8192, Some(2048), &crate::reasoning::Setting::Default).unwrap(),
            2048
        );
    }

    #[test]
    fn default_skips_metadata_and_live_overrides_apply_on_the_next_call() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut raw = vec![0; length];
                reader.read_exact(&mut raw).unwrap();
                let body: Value = serde_json::from_slice(&raw).unwrap();
                let reply = if index == 1 {
                    assert!(request.contains("/api/show"));
                    json!({"thinking":{"values":[true,false],"default":true}})
                } else {
                    assert!(request.contains("/api/chat"));
                    match index {
                        0 => assert!(body.get("think").is_none(), "Default must not force thinking off"),
                        2 => assert_eq!(body["think"], true),
                        3 => assert_eq!(body["think"], false),
                        _ => unreachable!(),
                    }
                    json!({"message":{"role":"assistant","content":"Done"},"done":true,"done_reason":"stop"})
                }.to_string()+"\n";
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("chuggin.json");
        let mut c: crate::runner::Config = serde_json::from_value(json!({
            "repo":directory.path(),"goal":"A useful project","ollama_url":url,
            "model":"local-fixture","context_tokens":4096,"output_tokens":256,
            "implementation_calls":4,"checks":[{"argv":["true"],"timeout_seconds":10}],
            "state_dir":directory.path().join(".chuggin"),"retry_seconds":1
        }))
        .unwrap();
        crate::setup::save(&config, &c).unwrap();
        let mut model =
            Model::new(&url, &c.model, 4096, 256, Arc::new(AtomicBool::new(false))).unwrap();
        model.use_project_settings(&config);
        let messages = [json!({"role":"user","content":"Work"})];
        model.chat(&messages, None, false).unwrap();
        c.reasoning = crate::reasoning::Setting::On;
        crate::setup::save(&config, &c).unwrap();
        model.chat(&messages, None, false).unwrap();
        c.reasoning = crate::reasoning::Setting::Off;
        crate::setup::save(&config, &c).unwrap();
        model.chat(&messages, None, false).unwrap();
        c.reasoning = crate::reasoning::Setting::Effort("high".into());
        crate::setup::save(&config, &c).unwrap();
        let error = model.chat(&messages, None, false).unwrap_err();
        assert!(error.to_string().contains("not advertised"), "{error:#}");
        server.join().unwrap();
    }

    #[test]
    fn role_settings_are_independent() {
        let directory = tempfile::tempdir().unwrap();
        let mut c: crate::runner::Config = serde_json::from_value(json!({
            "repo":directory.path(),"goal":"A useful project","ollama_url":"http://unused",
            "model":"main","chat_model":"chat","setup_model":"setup",
            "context_tokens":4096,"output_tokens":256,"implementation_calls":4,"checks":[],
            "state_dir":directory.path().join(".chuggin"),"retry_seconds":1
        }))
        .unwrap();
        c.reasoning = crate::reasoning::Setting::Off;
        c.chat_reasoning = crate::reasoning::Setting::Effort("high".into());
        c.helpers.reasoning = crate::reasoning::Setting::On;
        let mut model = Model::new(
            "http://unused",
            "main",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(model.selected_reasoning(&c), &c.reasoning);
        model.use_chat_settings(&directory.path().join("chuggin.json"), directory.path());
        assert_eq!(model.selected_reasoning(&c), &c.chat_reasoning);
        assert_eq!(model.selected_name(&c), "chat");
        let mut helper = Model::new(
            "http://unused",
            "helper",
            4096,
            256,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.configure_helper(
            &mut helper,
            &directory.path().join("chuggin.json"),
            directory.path(),
            "helper",
            "http://unused",
        );
        assert_eq!(helper.selected_reasoning(&c), &c.helpers.reasoning);
        assert_eq!(helper.selected_name(&c), "helper");
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
    fn context_usage_belongs_to_one_call_and_missing_counts_remain_unknown() {
        let reported = |prompt| {
            vec![json!({
                "message":{"content":"reply"},"done":true,"done_reason":"stop",
                "prompt_eval_count":prompt,"eval_count":10
            })]
        };
        let (model, server) = fixture(vec![
            reported(321),
            reported(654),
            completed("missing usage", ""),
            reported(17),
            completed("after observer", ""),
        ]);
        let messages = vec![json!({"role":"user","content":"fixture"})];
        model.chat(&messages, None, false).unwrap();
        let usage = model.take_context_usage().unwrap();
        assert_eq!(usage.prompt, 321);
        assert_eq!(usage.model, "reasoning-fixture");
        assert_eq!(usage.context_tokens, 4096);
        assert!(usage.recorded_at.is_some());
        assert!(model.take_context_usage().is_none());

        model.chat(&messages, None, false).unwrap();
        // An unconsumed earlier measurement must not fill in absent metadata.
        model.chat(&messages, None, false).unwrap();
        assert!(model.take_context_usage().is_none());
        model.watchdog_chat(&messages, json!([])).unwrap();
        model.chat(&messages, None, false).unwrap();
        assert!(model.take_context_usage().is_none());
        server.join().unwrap();

        model.stop.store(true, Ordering::SeqCst);
        assert!(model.chat(&messages, None, false).is_err());
        assert!(model.take_context_usage().is_none());
    }

    #[test]
    fn context_usage_survives_a_reported_generation_limit_error() {
        let (model, server) = fixture(
            (101..=103)
                .map(|prompt| {
                    vec![json!({
                        "message":{"content":"partial"},"done":true,"done_reason":"length",
                        "prompt_eval_count":prompt,"eval_count":256
                    })]
                })
                .collect(),
        );
        let messages = vec![json!({"role":"user","content":"fixture"})];
        assert!(model.chat(&messages, None, false).is_err());
        assert_eq!(model.take_context_usage().unwrap().prompt, 103);
        server.join().unwrap();
    }

    #[test]
    fn context_usage_is_not_carried_to_a_new_model_during_recovery() {
        let (mut model, server) = fixture(vec![
            vec![json!({
                "message":{"content":"partial"},"done":true,"done_reason":"length",
                "prompt_eval_count":321,"eval_count":256
            })],
            completed("same-model response without usage", ""),
            completed("new-model response without usage", ""),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chuggin.json");
        let mut c: crate::runner::Config = serde_json::from_value(json!({
            "repo":directory.path(),"state_dir":directory.path().join(".chuggin"),
            "goal":"Retain only relevant usage","model":"reasoning-fixture",
            "ollama_url":model.url.strip_suffix("/api/chat").unwrap(),
            "context_tokens":4096,"output_tokens":256,"implementation_calls":4,
            "checks":[{"argv":["true"],"timeout_seconds":10}],"retry_seconds":1
        }))
        .unwrap();
        crate::setup::save(&path, &c).unwrap();
        model.use_project_settings(&path);
        let messages = vec![json!({"role":"user","content":"fixture"})];
        let error = model.chat_format_once(&messages, None, None).unwrap_err();
        assert!(
            error.downcast_ref::<InterruptedResponse>().is_some(),
            "{error:#}"
        );
        assert_eq!(
            model.last_context_usage.borrow().as_ref().unwrap().prompt,
            321
        );
        model.chat_format_once(&messages, None, None).unwrap();
        assert_eq!(
            model.last_context_usage.borrow().as_ref().unwrap().prompt,
            321
        );
        c.model = "new-local-fixture".into();
        crate::setup::save(&path, &c).unwrap();
        model.chat_format_once(&messages, None, None).unwrap();
        assert!(model.take_context_usage().is_none());
        server.join().unwrap();
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
mod responses_transport_tests {
    use super::*;
    use std::net::TcpListener;

    fn read_request(socket: &std::net::TcpStream) -> (String, Value) {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut headers = String::new();
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
            headers.push_str(&line);
        }
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).unwrap();
        (headers, serde_json::from_slice(&bytes).unwrap())
    }
    fn fixture(frames: Vec<Vec<Value>>) -> (Model, std::thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut model = Model::new(
            "http://unused",
            "chatgpt/mock-model",
            32768,
            4096,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.url = format!("http://{}/responses", listener.local_addr().unwrap());
        model.responses_test_token = Some("fake-fixture-token".into());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for frames in frames {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (headers, body) = read_request(&socket);
                assert!(
                    headers
                        .to_lowercase()
                        .contains("authorization: bearer fake-fixture-token")
                );
                requests.push(body);
                let mut text = String::new();
                for frame in frames {
                    text.push_str(&format!(
                        "event: {}\ndata: {frame}\n\n",
                        frame["type"].as_str().unwrap()
                    ));
                }
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len()).unwrap();
            }
            requests
        });
        (model, server)
    }
    #[test]
    fn model_responses_branch_keeps_tools_history_reasoning_and_usage() {
        let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-model-state"});
        let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","namespace":"chuggin","name":"read_file","arguments":"{\"path\":\"a.txt\"}","status":"completed"});
        let message = json!({"type":"message","id":"msg_2","role":"assistant","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":"Checked","annotations":[]}]});
        let (model, server) = fixture(vec![
            vec![
                json!({"type":"response.reasoning_summary_text.delta","delta":"Inspecting project evidence"}),
                json!({"type":"response.completed","response":{"status":"completed","output":[reasoning,call],"usage":{"input_tokens":1024,"output_tokens":30}}}),
            ],
            vec![
                json!({"type":"response.output_text.delta","delta":"Checked"}),
                json!({"type":"response.completed","response":{"status":"completed","output":[message],"usage":{"input_tokens":1134,"output_tokens":10}}}),
            ],
        ]);
        let trace = tempfile::tempdir().unwrap();
        model.trace_to(trace.path());
        let mut messages = vec![
            json!({"role":"system","content":"Long project instructions ".repeat(1000)}),
            json!({"role":"user","content":"Read a.txt"}),
        ];
        let reply = model.chat(&messages, Some(tools()), false).unwrap();
        assert_eq!(
            reply["tool_calls"][0]["function"]["arguments"]["path"],
            "a.txt"
        );
        assert_eq!(
            model.take_completed_reasoning(),
            "Inspecting project evidence"
        );
        assert_eq!(model.take_context_usage().unwrap().prompt, 1024);
        messages.push(reply);
        messages.push(json!({"role":"tool","tool_call_id":"call_1","content":"Source evidence"}));
        let reply = model.chat(&messages, Some(tools()), false).unwrap();
        assert_eq!(reply["content"], "Checked");
        assert_eq!(
            reply["_chuggin_responses"]["items"][0]["phase"],
            "final_answer"
        );
        let usage = model.take_context_usage().unwrap();
        assert_eq!(usage.prompt, 1134);
        assert_eq!(usage.model, "chatgpt/mock-model");
        let requests = server.join().unwrap();
        assert_eq!(requests[0]["input"][0]["role"], "developer");
        assert!(
            requests[0]["input"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .len()
                > 20000
        );
        assert_eq!(
            requests[1]["input"][2]["encrypted_content"],
            "opaque-model-state"
        );
        assert_eq!(
            requests[1]["input"][3]["call_id"],
            requests[1]["input"][4]["call_id"]
        );
        for request in requests {
            assert_eq!(request["store"], false);
            assert_eq!(request["stream"], true);
            assert_eq!(request["tools"][0]["type"], "namespace");
            for key in [
                "temperature",
                "max_output_tokens",
                "max_tokens",
                "options",
                "previous_response_id",
            ] {
                assert!(request.get(key).is_none(), "{key}");
            }
        }
        for file in std::fs::read_dir(trace.path()).unwrap() {
            let text = std::fs::read_to_string(file.unwrap().path()).unwrap();
            assert!(!text.contains("fake-fixture-token"));
        }
    }
    #[test]
    fn model_never_returns_partial_or_failed_responses_calls() {
        let partial = json!({"type":"function_call","id":"fc_1","call_id":"call_1","namespace":"chuggin","name":"read_file","arguments":"{\"path\":","status":"in_progress"});
        let (model, server) = fixture(vec![
            vec![
                json!({"type":"response.output_item.added","output_index":0,"item":partial}),
                json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"},"usage":{"input_tokens":987,"output_tokens":2}}}),
            ],
            vec![json!({"type":"response.output_item.added","output_index":0,"item":partial})],
            vec![
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":988,"output_tokens":3}}}),
            ],
        ]);
        let messages = [json!({"role":"user","content":"Read evidence"})];
        let error = model
            .chat_format_once(&messages, Some(tools()), None)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::provider::Unavailable>()
                .is_some()
        );
        assert_eq!(model.take_context_usage().unwrap().prompt, 987);
        let error = model
            .chat_format_once(&messages, Some(tools()), None)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::provider::Unavailable>()
                .is_some()
        );
        let error = model
            .chat_format_once(&messages, Some(tools()), None)
            .unwrap_err();
        assert!(error.downcast_ref::<InterruptedResponse>().is_some());
        assert_eq!(model.take_context_usage().unwrap().prompt, 988);
        server.join().unwrap();
    }
    #[test]
    fn admission_refreshes_once_without_touching_account_storage() {
        for final_status in ["200 OK", "401 Unauthorized"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/responses", listener.local_addr().unwrap());
            let body = crate::responses::request(
                "chatgpt/mock-model",
                &[json!({"role":"system","content":"Inspect this project"})],
                Some(&json!([])),
                None,
            )
            .unwrap();
            let expected = body.clone();
            let server = std::thread::spawn(move || {
                for (index, status) in ["401 Unauthorized", final_status].into_iter().enumerate() {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let (headers, body) = read_request(&socket);
                    assert_eq!(body, expected);
                    assert!(headers.to_lowercase().contains(if index == 0 {
                        "authorization: bearer fake-old"
                    } else {
                        "authorization: bearer fake-new"
                    }));
                    let frames = if status == "200 OK" {
                        format!(
                            "data: {}\n\ndata: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":"Ready"}),
                            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Ready"}]}],"usage":{"input_tokens":31,"output_tokens":2}}})
                        )
                    } else {
                        "{}".into()
                    };
                    write!(socket,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{frames}",frames.len()).unwrap();
                }
            });
            let renewals = Cell::new(0);
            let response = send_responses_request(
                &crate::groq::client().unwrap(),
                &url,
                &body,
                "fake-old",
                5,
                |rejected| {
                    assert_eq!(rejected, "fake-old");
                    renewals.set(renewals.get() + 1);
                    Ok("fake-new".into())
                },
            )
            .unwrap();
            assert_eq!(renewals.get(), 1);
            if final_status == "200 OK" {
                let mut stream = crate::responses::Stream::default();
                for line in BufReader::new(response).lines() {
                    let line = line.unwrap();
                    if let Some(data) = line.strip_prefix("data:") {
                        stream.frame(data.trim()).unwrap();
                    }
                }
                assert!(stream.completed);
                assert_eq!(stream.usage, Some((31, 2)));
                assert!(stream.calls().unwrap().is_empty());
            } else {
                assert_eq!(response.status().as_u16(), 401);
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn successful_admission_never_refreshes_or_retries_generation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&socket);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let response = send_responses_request(
            &crate::groq::client().unwrap(),
            &url,
            &json!({"model":"mock"}),
            "fake-token",
            5,
            |_| panic!("successful admission must not refresh"),
        )
        .unwrap();
        assert!(response.status().is_success());
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

#[cfg(test)]
mod cloud_transport_tests {
    use super::*;

    #[test]
    fn free_cloud_transports_preserve_tools_reasoning_and_full_conversation() {
        for provider in ["zen", "openrouter", "missing-key"] {
            let home = tempfile::tempdir().unwrap();
            let config = home.path().join("chuggin");
            std::fs::create_dir_all(&config).unwrap();
            if provider != "missing-key" {
                std::fs::write(config.join("openrouter.key"), "fixture-cloud-secret").unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "model::cloud_transport_tests::cloud_worker",
                    "--ignored",
                ])
                .env("XDG_CONFIG_HOME", home.path())
                .env("CHUGGIN_CLOUD_FIXTURE", provider)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{provider}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    #[ignore = "Isolated local mock launched by free_cloud_transports_preserve_tools_reasoning_and_full_conversation"]
    fn cloud_worker() {
        let provider = std::env::var("CHUGGIN_CLOUD_FIXTURE").unwrap();
        if provider == "missing-key" {
            assert!(
                crate::cloud::key("openrouter/connection-check")
                    .unwrap()
                    .is_none()
            );
            let model = Model::new(
                "http://unused",
                "openrouter/stealth/space-bunny-alpha",
                32768,
                4096,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let error = model.chat_format_once(&[], None, None).unwrap_err();
            assert!(error.to_string().contains("Add your OpenRouter API key"));
            return;
        }
        let zen = provider == "zen";
        let name = if zen {
            "zen/space-bunny-free"
        } else {
            "openrouter/stealth/space-bunny-alpha"
        };
        let id = crate::cloud::model_id(name).unwrap().to_owned();
        let metadata = if zen {
            json!({"id":id,"cost":{"input":0,"output":0},"tool_call":true,"modalities":{"input":["text","image"],"output":["text"]},"provider":{"npm":"@ai-sdk/openai-compatible"}})
        } else {
            json!({"id":id,"pricing":{"prompt":"0","completion":"0"},"architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},"supported_parameters":["tools"]})
        };
        crate::cloud::seed_catalog(
            crate::cloud::provider(name).unwrap(),
            &json!({"data":[metadata]}),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut model = Model::new(
            "http://unused",
            name,
            1_048_576,
            4096,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        model.url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let trace = tempfile::tempdir().unwrap();
        model.trace_to(trace.path());
        let server = std::thread::spawn(move || {
            for round in 0..6 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(size) = line.to_lowercase().strip_prefix("content-length:") {
                        length = size.trim().parse().unwrap();
                    }
                    headers.push_str(&line);
                }
                assert!(headers.to_lowercase().contains(if zen {
                    "authorization: bearer public"
                } else {
                    "authorization: bearer fixture-cloud-secret"
                }));
                assert!(headers.to_lowercase().contains("user-agent: chuggin/"));
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["model"], id);
                assert_eq!(body["max_tokens"], 4096);
                for field in ["options", "think", "temperature", "max_completion_tokens"] {
                    assert!(body.get(field).is_none(), "{field}");
                }
                assert_eq!(
                    body["messages"][0]["content"].as_str().unwrap().len(),
                    30000,
                    "Cloud never uses Groq's token trimming"
                );
                if !zen {
                    assert_eq!(
                        body["provider"]["max_price"],
                        json!({"prompt":0,"completion":0,"request":0,"image":0})
                    );
                }
                if round == 1 {
                    assert_eq!(
                        body["messages"][1]["reasoning_content"],
                        "Private cloud reasoning"
                    );
                    assert_eq!(
                        body["messages"][1]["tool_calls"][0]["function"]["arguments"],
                        "{\"path\":\"fixture.txt\"}"
                    );
                    assert_eq!(body["messages"][2]["tool_call_id"], "cloud-call");
                    assert_eq!(body["messages"][2]["content"], "Tool evidence");
                }
                if round == 2 {
                    write!(socket,"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                if round == 4 {
                    write!(socket,"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 321\r\nContent-Length: 1\r\nConnection: close\r\n\r\n").unwrap();
                    socket.write_all(&[0xff]).unwrap();
                    continue;
                }
                let frames = match round {
                    0 => vec![
                        json!({"choices":[{"delta":{"reasoning_content":"Private cloud reasoning"}}]}),
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"cloud-call","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}),
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"fixture.txt\"}"}}]},"finish_reason":"tool_calls"}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":9000,"completion_tokens":50}}),
                    ],
                    1 => vec![
                        json!({"choices":[{"delta":{"content":"{\"goal\":\"Complete\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9100,"completion_tokens":20}}),
                    ],
                    5 => vec![
                        json!({"choices":[{"delta":{"content":"Partial response"}}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":9876,"completion_tokens":3}}),
                    ],
                    _ => vec![json!({"error":{"code":503,"message":"Upstream maintenance"}})],
                };
                let mut data: String = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect();
                if round != 5 {
                    data.push_str("data: [DONE]\n\n");
                }
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",data.len()).unwrap();
            }
        });
        let mut messages = vec![json!({"role":"user","content":"x".repeat(30000)})];
        let first = model.chat(&messages, Some(tools()), false).unwrap();
        let usage = model.take_context_usage().unwrap();
        assert_eq!(usage.prompt, 9000);
        assert_eq!(usage.model, name);
        assert_eq!(usage.context_tokens, 1_048_576);
        assert!(usage.recorded_at.is_some());
        assert_eq!(
            first["tool_calls"][0]["function"]["arguments"],
            json!({"path":"fixture.txt"})
        );
        assert_eq!(model.take_completed_reasoning(), "Private cloud reasoning");
        assert!(model.take_completed_reasoning().is_empty());
        messages.push(first);
        messages.push(json!({"role":"tool","tool_call_id":"cloud-call","tool_name":"read_file","content":"Tool evidence"}));
        let second = model.chat(&messages, None, true).unwrap();
        assert_eq!(model.take_context_usage().unwrap().prompt, 9100);
        assert_eq!(second["content"], "{\"goal\":\"Complete\"}");
        let error = model.chat_format_once(&messages, None, None).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::provider::Unavailable>()
                .unwrap()
                .retry_after,
            Some(Duration::from_secs(123))
        );
        assert!(
            model
                .chat_format_once(&messages, None, None)
                .unwrap_err()
                .downcast_ref::<crate::provider::Unavailable>()
                .is_some()
        );
        let error = model.chat_format_once(&messages, None, None).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::provider::Unavailable>()
                .unwrap()
                .retry_after,
            Some(Duration::from_secs(321))
        );
        // A reported usage frame is authoritative even if the stream then ends
        // without [DONE]. Use one attempt rather than retrying the provider.
        let error = model.chat_format_once(&messages, None, None).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::provider::Unavailable>()
                .is_some()
        );
        let usage = model.take_context_usage().unwrap();
        assert_eq!(usage.prompt, 9876);
        assert_eq!(usage.model, name);
        assert_eq!(usage.context_tokens, 1_048_576);
        server.join().unwrap();
        for file in std::fs::read_dir(trace.path()).unwrap().flatten() {
            assert!(
                !std::fs::read_to_string(file.path())
                    .unwrap()
                    .contains("fixture-cloud-secret")
            );
        }
    }
}

#[cfg(test)]
mod zen_live_tests {
    use super::*;

    #[test]
    #[ignore = "Set CHUGGIN_LIVE_ZEN=1 to run two tiny public OpenCode Zen requests; never uses Ollama"]
    fn public_zen_streaming_tool_round_trip() {
        assert_eq!(std::env::var("CHUGGIN_LIVE_ZEN").as_deref(), Ok("1"));
        let model = Model::new(
            "http://unused",
            "zen/space-bunny-free",
            32768,
            4096,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let trace = tempfile::tempdir().unwrap();
        model.trace_to(trace.path());
        let mut messages = vec![
            json!({"role":"system","content":"Client integration check: call lookup_token exactly once to obtain a token, then return only a JSON object with that token. Do not invent the token or repeat the lookup."}),
            json!({"role":"user","content":"Look up the token now."}),
        ];
        let tools = json!([{"type":"function","function":{"name":"lookup_token","description":"Obtain the integration fixture token.","parameters":{"type":"object","properties":{}}}}]);
        let response = model.chat(&messages, Some(tools), false).unwrap();
        assert_eq!(response["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(
            response["tool_calls"][0]["function"]["name"],
            "lookup_token"
        );
        let id = response["tool_calls"][0]["id"].clone();
        messages.push(response);
        messages.push(json!({"role":"tool","tool_name":"lookup_token","tool_call_id":id,"content":"{\"token\":\"CHUGGIN_ZEN_ROUNDTRIP_OK\"}"}));
        let response = model.chat(&messages, None, true).unwrap();
        let reply: Value = parse_reply(response["content"].as_str().unwrap()).unwrap();
        assert_eq!(reply["token"], "CHUGGIN_ZEN_ROUNDTRIP_OK");
        println!(
            "Public Zen tool request and JSON reply succeeded through Chuggin's production client."
        );
    }
}
