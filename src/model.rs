use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
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
    stop: Arc<AtomicBool>,
    trace: RefCell<Option<PathBuf>>,
    sequence: Cell<u32>,
    settings_path: Option<PathBuf>,
    completed_messages: RefCell<Option<Vec<Value>>>,
    context_pressure: Cell<bool>,
    run_controls: Option<(Arc<AtomicBool>, Instant)>,
    request_target: RefCell<(String, String)>,
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
            url: format!("{}/api/chat", url.trim_end_matches('/')),
            name: name.into(),
            context,
            output,
            stop,
            trace: RefCell::new(None),
            sequence: Cell::new(0),
            settings_path: None,
            completed_messages: RefCell::new(None),
            context_pressure: Cell::new(false),
            run_controls: None,
            request_target: RefCell::new((
                name.into(),
                format!("{}/api/chat", url.trim_end_matches('/')),
            )),
        })
    }
    pub fn take_completed_messages(&self) -> Option<Vec<Value>> {
        self.completed_messages.borrow_mut().take()
    }
    pub fn context_pressure(&self) -> bool {
        self.context_pressure.get()
    }
    pub fn use_project_settings(&mut self, path: &Path) {
        self.settings_path = Some(path.to_owned());
    }
    pub fn use_run_controls(&mut self, stop: Arc<AtomicBool>, started: Instant) {
        self.run_controls = Some((stop, started));
    }
    fn provider_target(&self) -> Result<(String, String, Option<PathBuf>)> {
        if let Some(path) = &self.settings_path {
            let c = crate::runner::load(path)?;
            Ok((
                c.model,
                format!("{}/api/chat", c.ollama_url.trim_end_matches('/')),
                Some(c.state_dir.join("provider-wait.json")),
            ))
        } else {
            Ok((self.name.clone(), self.url.clone(), None))
        }
    }
    fn wait_for_provider(&self, record: &Value) -> Result<()> {
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
                    && started.elapsed().as_secs() >= c.run_duration_seconds
                {
                    return Err(crate::provider::Stopped(
                        "Run timer reached while waiting for provider; saving work".into(),
                    )
                    .into());
                }
            }
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if current.0 != record["model"] || current.1 != record["url"] || now >= until {
                if let Some(path) = current.2 {
                    let _ = std::fs::remove_file(path);
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
    fn chat_format(
        &self,
        messages: &[Value],
        tools: Option<Value>,
        format: Option<Value>,
    ) -> Result<Value> {
        *self.completed_messages.borrow_mut() = None;
        let mut conversation = messages.to_vec();
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
            let result = self.chat_format_once(&conversation, tools.clone(), format.clone());
            if let Err(error) = &result {
                crate::events::send(crate::events::Event::RequestFinished);
                if let Some(provider) = error.downcast_ref::<crate::provider::Unavailable>() {
                    provider_attempts = provider_attempts.saturating_add(1);
                    let delay = provider
                        .retry_after
                        .unwrap_or_else(|| crate::provider::backoff(provider_attempts));
                    let (model, url) = self.request_target.borrow().clone();
                    // Round up to the next second so persistence never shortens Retry-After.
                    let record = json!({"reason":provider.reason,"pause":provider.pause,"model":model,"url":url,"attempt":provider_attempts,"until":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().saturating_add(delay.as_secs()).saturating_add(1),"delay_seconds":delay.as_secs()});
                    if let Some(path) = self.trace.borrow().as_ref() {
                        std::fs::write(
                            path.join(format!(
                                "provider-error-{:03}.json",
                                self.sequence.get().saturating_sub(1)
                            )),
                            serde_json::to_vec_pretty(&record)?,
                        )?;
                    }
                    if provider.pause {
                        return Err(crate::provider::Stopped(format!("{}. Resolve billing/access or select another model, then resume. No automatic retries.",provider.reason)).into());
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
        // Snapshot settings once; edits never alter an in-flight request.
        let live = self
            .settings_path
            .as_ref()
            .map(|p| crate::runner::load(p))
            .transpose()?;
        let name = live
            .as_ref()
            .map(|c| c.model.as_str())
            .unwrap_or(&self.name);
        let url = live
            .as_ref()
            .map(|c| format!("{}/api/chat", c.ollama_url.trim_end_matches('/')))
            .unwrap_or_else(|| self.url.clone());
        let timeout = live
            .as_ref()
            .map(|c| c.request_timeout_seconds)
            .unwrap_or(1800);
        *self.request_target.borrow_mut() = (name.to_owned(), url.clone());
        crate::events::send(crate::events::Event::RequestModel(name.to_owned()));
        crate::events::send(crate::events::Event::Request);
        anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
        let mut body = json!({"model":name,"messages":messages,"stream":true,"think":false,"options":{"num_ctx":self.context,"num_predict":self.output,"temperature":0.4}});
        if let Some(t) = tools {
            body["tools"] = t;
        }
        if let Some(schema) = format {
            body["format"] = schema;
            body["options"]["temperature"] = json!(0);
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
                serde_json::to_vec_pretty(&body)?,
            )?;
            Some(std::fs::File::create(
                path.join(format!("response-{sequence:03}.ndjson")),
            )?)
        } else {
            None
        };
        let request = self.client.post(url).json(&body);
        let request = if timeout == 0 {
            request
        } else {
            request.timeout(Duration::from_secs(timeout))
        };
        let response = request.send()?;
        let retry_after = crate::provider::retry_after(
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            SystemTime::now(),
        );
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let mut body = String::new();
            response.take(16384).read_to_string(&mut body)?;
            if let Some(provider) = crate::provider::classify(status, &body, retry_after) {
                return Err(provider.into());
            }
            bail!("Ollama HTTP {status}: request rejected");
        }
        let mut content = String::new();
        let mut thinking = String::new();
        let mut calls = Vec::new();
        let mut done = false;
        for line in BufReader::new(response).lines() {
            anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
            let line = line?;
            if let Some(file) = trace.as_mut() {
                writeln!(file, "{line}")?;
            }
            if line.is_empty() {
                continue;
            }
            let d: Value = serde_json::from_str(&line).context("Invalid Ollama stream JSON")?;
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
        anyhow::ensure!(done, "Ollama stream disconnected before completion");
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
     {"type":"function","function":{"name":"save_progress_note","description":"Replace a short persistent progress note carried across tasks, checkpoints and context handoffs. Record observed failure, attempted fix, constraints learned, and next action; cite files or check evidence. This is an advisory note, not proof of success. Maximum 1600 bytes; empty clears it.","parameters":{"type":"object","properties":{"note":{"type":"string"}},"required":["note"]}}},
     {"type":"function","function":{"name":"project_map","description":"Inspect project file paths plus optional Rust declarations and module reachability. For other formats use search and read_file to inspect content. The map is inventory, not proof of correctness.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"set_task","description":"Record or revise the current task. Plans and file lists are advisory; work already on disk is always retained.","parameters":{"type":"object","properties":{"title":{"type":"string"},"objective":{"type":"string"},"acceptance":{"type":"array","items":{"type":"string"}},"files":{"type":"array","items":{"type":"string"}},"out_of_scope":{"type":"array","items":{"type":"string"}}},"required":["title","objective"]}}},
     {"type":"function","function":{"name":"finish_task","description":"Report that the current task is complete. The harness saves a checkpoint and verifies configured checks. Unresolved failures remain available for repair.","parameters":{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}}},
     {"type":"function","function":{"name":"restore_checkpoint","description":"Explicitly restore project files from an ancestor checkpoint. First saves all current work in Git. Use only when inspection shows this is preferable to repairing current work. Explain why, then validate the result.","parameters":{"type":"object","properties":{"commit":{"type":"string"},"reason":{"type":"string"}},"required":["commit","reason"]}}},
     {"type":"function","function":{"name":"read_file","description":"Read numbered lines. Follow the continuation start_line to read the rest, rather than repeating the same request.","parameters":{"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer"},"line_count":{"type":"integer"}},"required":["path"]}}},
     {"type":"function","function":{"name":"edit_file","description":"Replace an exact, unique old_text occurrence with new_text. Supply file text without line-number prefixes.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}}},
     {"type":"function","function":{"name":"list_files","description":"List project paths.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"search","description":"Find literal text in project files; returns paths and line numbers.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}},
     {"type":"function","function":{"name":"write_file","description":"Create or replace a project file. Read existing files first. Use project-relative paths; preserve operator settings and Git metadata.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}},
     {"type":"function","function":{"name":"run_checks","description":"Execute the operator-configured validation commands and return results.","parameters":{"type":"object","properties":{}}}}
    ]);
    tools
        .as_array_mut()
        .unwrap()
        .extend(crate::dev_tools::schemas());
    tools.as_array_mut().unwrap().push(crate::symbols::schema());
    if crate::web_tools::enabled() {
        tools
            .as_array_mut()
            .unwrap()
            .extend(crate::web_tools::schemas());
    }
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
