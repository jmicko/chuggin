use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
        })
    }
    pub fn use_project_settings(&mut self, path: &Path) {
        self.settings_path = Some(path.to_owned());
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
        for attempt in 0..2 {
            let result = self.chat_format_once(messages, tools.clone(), format.clone());
            if let Err(error) = &result {
                crate::events::send(crate::events::Event::RequestFinished);
                let transient = error.chain().any(|e| {
                    e.downcast_ref::<reqwest::Error>()
                        .is_some_and(|e| e.is_timeout() || e.is_connect())
                });
                if let Some(path) = self.trace.borrow().as_ref() {
                    std::fs::write(
                        path.join(format!(
                            "request-failure-{:03}.json",
                            self.sequence.get().saturating_sub(1)
                        )),
                        serde_json::to_vec_pretty(
                            &json!({"error":format!("{error:#}"),"retry_same_stage":transient && attempt == 0}),
                        )?,
                    )?;
                }
                if transient && attempt == 0 && !self.stop.load(Ordering::SeqCst) {
                    crate::events::log("Model request timed out or failed to connect; retrying the same stage once with current settings. Completed edits and checks are retained.".into());
                    continue;
                }
            }
            return result;
        }
        unreachable!()
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
        crate::events::send(crate::events::Event::RequestModel(name.to_owned()));
        crate::events::send(crate::events::Event::Request);
        anyhow::ensure!(!self.stop.load(Ordering::SeqCst), "Stopped by operator");
        // Conservative byte budget; token counts vary by model/tokenizer.
        let input_bytes = serde_json::to_vec(messages)?.len()
            + tools.as_ref().map(|t| t.to_string().len()).unwrap_or(0);
        let budget = self.context.saturating_sub(self.output + 1024) as usize;
        anyhow::ensure!(
            input_bytes <= budget.min(64000),
            "Stage input exceeds conservative context budget ({input_bytes} bytes > {}); reduce task/context payload",
            budget.min(64000)
        );
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
        let response = request.send()?.error_for_status()?;
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
            anyhow::ensure!(
                content.len() + thinking.len() < 64_000,
                "Response size exceeded; recover with a smaller task"
            );
            anyhow::ensure!(
                !repetitive(&thinking) && !repetitive(&content),
                "Repeated model output; discarded response"
            );
            if d["done"].as_bool() == Some(true) {
                crate::events::send(crate::events::Event::Metrics {
                    prompt: d["prompt_eval_count"].as_u64().unwrap_or(0),
                    generated: d["eval_count"].as_u64().unwrap_or(0),
                    seconds: d["eval_duration"].as_f64().unwrap_or(0.) / 1_000_000_000.,
                });
                anyhow::ensure!(
                    d["done_reason"] != "length",
                    "Model hit output/context limit; response discarded"
                );
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
        let m = self.chat_format(
            &[
                json!({"role":"system","content":system}),
                json!({"role":"user","content":input.to_string()}),
            ],
            None,
            Some(schema.clone()),
        )?;
        match parse_reply(m["content"].as_str().unwrap_or("")) {
            Ok(value) => Ok(value),
            Err(_) => {
                let repaired = self.chat_format(&[
                    json!({"role":"system","content":format!("{system}\nReturn ONLY valid JSON. Repair the formatting of this prior answer, preserving its meaning. JSON keys must be double-quoted. Do not add commentary or Markdown.")}),
                    json!({"role":"user","content":crate::project::excerpt(m["content"].as_str().unwrap_or(""),12000)}),
                ], None, Some(schema))?;
                parse_reply(repaired["content"].as_str().unwrap_or(""))
                    .context("Could not read the model's reply after a formatting retry")
            }
        }
    }

    /// Read-only investigation with bounded observations; no history survives this stage.
    pub fn investigate<T: serde::de::DeserializeOwned + schemars::JsonSchema>(
        &self,
        system: &str,
        input: Value,
        tools: Value,
        mut execute: impl FnMut(&str, &Value) -> Result<String>,
    ) -> Result<T> {
        let mut messages = vec![
            json!({"role":"system","content":system}),
            json!({"role":"user","content":input.to_string()}),
        ];
        let mut observations = Vec::new();
        for _ in 0..6 {
            if serde_json::to_vec(&messages)?.len() + tools.to_string().len()
                > self.context.saturating_sub(self.output + 2048).min(48000) as usize
            {
                break;
            }
            let response = self.chat(&messages, Some(tools.clone()), false)?;
            let calls = response["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                if let Ok(value) = parse_reply(response["content"].as_str().unwrap_or("")) {
                    return Ok(value);
                }
                observations.push(json!({"draft":crate::project::excerpt(response["content"].as_str().unwrap_or(""),4000)}));
                break;
            }
            anyhow::ensure!(
                calls.len() <= 8,
                "Too many inspection calls in one response"
            );
            messages.push(response);
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args = &call["function"]["arguments"];
                let result = match execute(name, args) {
                    Ok(value) => json!({"ok":true,"result":crate::project::excerpt(&value,6000)}),
                    Err(e) => json!({"ok":false,"error":e.to_string()}),
                };
                observations.push(json!({"tool":name,"arguments":args,"result":result}));
                let mut reply =
                    json!({"role":"tool","tool_name":name,"content":result.to_string()});
                if let Some(id) = call.get("id") {
                    reply["tool_call_id"] = id.clone();
                }
                messages.push(reply);
            }
        }
        self.structured(system,json!({"project":input,"inspection":crate::project::excerpt(&serde_json::to_string(&observations)?,8000),"instruction":"Finish the discovery decision from the actual inspected evidence."}))
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
// Only scan the last few thousand characters; long source-code outputs may legitimately repeat.
// Four identical nontrivial lines/paragraphs near the tail are enough to abandon this response.
fn repetitive(s: &str) -> bool {
    let tail: String = s
        .chars()
        .rev()
        .take(6000)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let mut seen = std::collections::HashMap::new();
    for line in tail.split(['\n', '.']) {
        let line = line.trim();
        // Structured task lists and source strings often repeat valid fields.
        // Output-size/token limits still bound those responses.
        if line.len() < 70 || line.starts_with('"') {
            continue;
        }
        let n = seen.entry(line).or_insert(0);
        *n += 1;
        if *n >= 4 {
            return true;
        }
    }
    false
}
pub fn tools() -> Value {
    let mut tools = json!([
     {"type":"function","function":{"name":"save_progress_note","description":"Replace a short task-local handoff note preserved across fresh contexts and same-task repairs. Record observed failure, attempted fix, constraints learned, and next action; cite files or check evidence. This is an advisory note, not proof of success. Maximum 1600 bytes; empty clears it.","parameters":{"type":"object","properties":{"note":{"type":"string"}},"required":["note"]}}},
     {"type":"function","function":{"name":"project_map","description":"Inspect project file paths plus optional Rust declarations and module reachability. For other formats use search and read_file to inspect content. The map is inventory, not proof of correctness.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"request_file_access","description":"Request access to an existing supporting file omitted from task.files, in any language or text format. Explain the minimal supporting change. The harness grants safe paths and records the exception for review. Preserve existing behavior and validation.","parameters":{"type":"object","properties":{"path":{"type":"string"},"reason":{"type":"string"}},"required":["path","reason"]}}},
     {"type":"function","function":{"name":"read_file","description":"Read numbered lines. Follow the continuation start_line to read the rest, rather than repeating the same request.","parameters":{"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer"},"line_count":{"type":"integer"}},"required":["path"]}}},
     {"type":"function","function":{"name":"edit_file","description":"Replace an exact, unique old_text occurrence with new_text. Supply file text without line-number prefixes.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}}},
     {"type":"function","function":{"name":"list_files","description":"List project paths.","parameters":{"type":"object","properties":{}}}},
     {"type":"function","function":{"name":"search","description":"Find literal text in project files; returns paths and line numbers.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}},
     {"type":"function","function":{"name":"write_file","description":"Create or replace a project file. Read existing files first. Only files in the task specification may be changed.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}},
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
pub fn tools_for(root: &std::path::Path) -> Value {
    let mut available = tools();
    let rust = crate::project::inventory(root)
        .unwrap_or_default()
        .iter()
        .any(|p| p.ends_with(".rs"));
    available
        .as_array_mut()
        .unwrap()
        .retain(|t| match t["function"]["name"].as_str() {
            Some("compiler_diagnostics") => root.join("Cargo.toml").is_file(),
            Some("lookup_symbol") => rust,
            _ => true,
        });
    available
}

pub fn inspection_tools(root: &std::path::Path) -> Value {
    json!(
        tools_for(root)
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| matches!(
                t["function"]["name"].as_str(),
                Some(
                    "project_map"
                        | "lookup_symbol"
                        | "read_file"
                        | "list_files"
                        | "search"
                        | "web_search"
                        | "read_web_page"
                )
            ))
            .cloned()
            .collect::<Vec<_>>()
    )
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
