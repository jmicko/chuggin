//! Restore measured main-model input usage without estimating transcript tokens.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const TAIL_BYTES: u64 = 1024 * 1024;
const REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const LEGACY_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const METADATA_BYTES: u64 = 128 * 1024;
const LEGACY_CYCLES: usize = 32;
const LEGACY_REQUESTS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Usage {
    pub prompt: u64,
    pub model: String,
    pub context_tokens: u32,
    pub recorded_at: Option<String>,
}

impl Usage {
    fn matches(&self, model: &str) -> bool {
        self.prompt > 0 && self.context_tokens > 0 && self.model == model
    }
}

enum Durable {
    Missing,
    Present(Option<Usage>),
}

/// New conversations put the durable field last, allowing a bounded tail read
/// even when their message history is large. Explicit unknown/mismatched usage
/// never causes repeated archive scans or restores another model's measurement.
pub fn load(state: &Path, model: &str, context_tokens: u32) -> Option<Usage> {
    match durable(&state.join("conversation.json"), model) {
        Durable::Present(usage) => usage,
        Durable::Missing => legacy(state, model, context_tokens),
    }
}

fn durable(path: &Path, model: &str) -> Durable {
    let (tail, truncated) = match tail(path, TAIL_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Durable::Missing,
        Err(_) => return Durable::Present(None),
    };
    let value = if !truncated {
        match serde_json::from_slice::<Value>(&tail) {
            Ok(value) => value,
            Err(_) => return Durable::Present(None),
        }
    } else {
        // Conversation uses pretty serialization with this final top-level
        // field. Requiring its two-space indentation excludes nested tool data.
        let text = String::from_utf8_lossy(&tail);
        let Some(index) = text.rfind("\n  \"last_context_usage\"") else {
            return Durable::Missing;
        };
        let suffix = format!("{{{}", &text[index + 3..]);
        match serde_json::from_str::<Value>(&suffix) {
            Ok(value) => value,
            Err(_) => return Durable::Present(None),
        }
    };
    let Some(value) = value.get("last_context_usage") else {
        return Durable::Missing;
    };
    Durable::Present(
        serde_json::from_value::<Usage>(value.clone())
            .ok()
            .filter(|usage| usage.matches(model)),
    )
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Request {
    model: String,
    tools: Vec<Tool>,
    options: Options,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Tool {
    function: Function,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    tools: Vec<Tool>,
}

impl Tool {
    fn main_request(&self) -> bool {
        matches!(self.function.name.as_str(), "set_task" | "finish_task")
            || (self.kind == "function" && matches!(self.name.as_str(), "set_task" | "finish_task"))
            || (self.kind == "namespace" && self.tools.iter().any(Tool::main_request))
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Function {
    name: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Options {
    num_ctx: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Metadata {
    model: String,
    context_tokens: Option<u32>,
}

fn read_metadata(path: &Path) -> Option<Metadata> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > METADATA_BYTES {
        return None;
    }
    serde_json::from_reader(BufReader::new(file)).ok()
}

fn legacy(state: &Path, model: &str, context_tokens: u32) -> Option<Usage> {
    let mut cycles: Vec<(u64, PathBuf)> = fs::read_dir(state)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let cycle = entry
                .file_name()
                .to_str()?
                .strip_prefix("cycle-")?
                .parse()
                .ok()?;
            entry
                .file_type()
                .ok()?
                .is_dir()
                .then(|| (cycle, entry.path()))
        })
        .collect();
    cycles.sort_unstable_by_key(|cycle| std::cmp::Reverse(cycle.0));
    let wire_model = crate::cloud::model_id(model)
        .or_else(|| crate::groq::model_id(model))
        .unwrap_or(model);
    let mut remaining_request_bytes = LEGACY_SCAN_BYTES;
    for (_, cycle) in cycles.into_iter().take(LEGACY_CYCLES) {
        let configuration = read_metadata(&cycle.join("configuration.json"));
        let mut requests: Vec<(u64, PathBuf)> = fs::read_dir(&cycle)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let sequence = name.strip_prefix("request-")?.strip_suffix(".json")?;
                if sequence.is_empty() || !sequence.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Some((sequence.parse().ok()?, entry.path()))
            })
            .collect();
        requests.sort_unstable_by_key(|request| std::cmp::Reverse(request.0));
        for (sequence, path) in requests.into_iter().take(LEGACY_REQUESTS) {
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if metadata.len() > REQUEST_BYTES {
                continue;
            }
            remaining_request_bytes = remaining_request_bytes.checked_sub(metadata.len())?;
            // Unknown fields (especially messages) are streamed and discarded,
            // rather than allocating or inspecting an entire conversation.
            let Ok(request) = serde_json::from_reader::<_, Request>(BufReader::new(file)) else {
                continue;
            };
            if (request.model != model && request.model != wire_model)
                || !request.tools.iter().any(Tool::main_request)
            {
                continue;
            }
            let connection = read_metadata(&cycle.join(format!("connection-{sequence:03}.json")));
            let recorded_model = connection
                .as_ref()
                .filter(|metadata| !metadata.model.is_empty())
                .or_else(|| {
                    configuration
                        .as_ref()
                        .filter(|metadata| !metadata.model.is_empty())
                });
            if recorded_model.is_some_and(|metadata| metadata.model != model) {
                continue;
            }
            let limit = request
                .options
                .num_ctx
                .or_else(|| {
                    connection
                        .as_ref()
                        .and_then(|metadata| metadata.context_tokens)
                })
                .or_else(|| {
                    configuration
                        .as_ref()
                        .and_then(|metadata| metadata.context_tokens)
                })
                .unwrap_or(context_tokens);
            if limit == 0 {
                continue;
            }
            let response = cycle.join(format!("response-{sequence:03}.ndjson"));
            let Ok((bytes, truncated)) = tail(&response, TAIL_BYTES) else {
                continue;
            };
            let bytes = if truncated {
                // Never interpret a JSON fragment cut from the start of a line
                // as a complete provider frame.
                bytes
                    .splitn(2, |byte| *byte == b'\n')
                    .nth(1)
                    .unwrap_or_default()
            } else {
                &bytes
            };
            let Some(prompt) = measured_prompt(bytes) else {
                continue;
            };
            let recorded_at = fs::metadata(&response)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339());
            return Some(Usage {
                prompt,
                model: model.into(),
                context_tokens: limit,
                recorded_at,
            });
        }
    }
    None
}

fn tail(path: &Path, limit: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(length.min(limit) as usize);
    file.take(limit).read_to_end(&mut bytes)?;
    Ok((bytes, start > 0))
}

fn frame_prompt(text: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(text).ok()?;
    if matches!(
        value["type"].as_str(),
        Some("response.completed" | "response.failed" | "response.incomplete")
    ) {
        return value["response"]["usage"]["input_tokens"]
            .as_u64()
            .filter(|prompt| *prompt > 0);
    }
    if value["done"] == true {
        return value["prompt_eval_count"]
            .as_u64()
            .filter(|prompt| *prompt > 0);
    }
    value
        .get("usage")
        .or_else(|| value["x_groq"].get("usage"))?
        .get("prompt_tokens")?
        .as_u64()
        .filter(|prompt| *prompt > 0)
}

fn measured_prompt(bytes: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(bytes);
    let mut last = None;
    let mut event_data = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            if let Some(prompt) = frame_prompt(&event_data) {
                last = Some(prompt);
            }
            event_data.clear();
        } else if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if let Some(prompt) = frame_prompt(data) {
                last = Some(prompt);
            }
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(data);
        } else if let Some(prompt) = frame_prompt(line) {
            last = Some(prompt);
        }
    }
    frame_prompt(&event_data).or(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(cycle: &Path, sequence: u64, model: &str, main: bool, context: Option<u32>) {
        fs::create_dir_all(cycle).unwrap();
        let mut value = json!({
            "model":model,
            "messages":[{"role":"user","content":"The words set_task and finish_task in this message do not identify a main request."}],
            "tools":[{"function":{"name":if main {"set_task"} else {"report_diagnosis"}}}]
        });
        if let Some(context) = context {
            value["options"] = json!({"num_ctx":context});
        }
        fs::write(
            cycle.join(format!("request-{sequence:03}.json")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    fn response(cycle: &Path, sequence: u64, text: &str) {
        fs::write(cycle.join(format!("response-{sequence:03}.ndjson")), text).unwrap();
    }

    fn native_request(cycle: &Path, sequence: u64, model: &str, main: bool) {
        fs::create_dir_all(cycle).unwrap();
        // Use the real adapter so restoration follows the actual namespace
        // wire format without reading credentials or sending any request.
        let tools = json!([{"type":"function","function":{
            "name":if main {"set_task"} else {"report_investigation"},
            "parameters":{"type":"object","properties":{}}
        }}]);
        let value = crate::responses::request(
            model,
            &[json!({"role":"user","content":"set_task mentioned in history is not a main tool."})],
            Some(&tools),
            None,
        )
        .unwrap();
        fs::write(
            cycle.join(format!("request-{sequence:03}.json")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn ollama_restores_measured_input_and_original_request_window() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000003");
        request(&cycle, 0, "local:9b", true, Some(65536));
        fs::write(
            cycle.join("configuration.json"),
            r#"{"model":"local:9b","context_tokens":4096}"#,
        )
        .unwrap();
        response(
            &cycle,
            0,
            "{\"done\":false,\"message\":{\"content\":\"working\"}}\n{\"done\":true,\"prompt_eval_count\":12345,\"eval_count\":900}\n",
        );
        let usage = load(state.path(), "local:9b", 8192).unwrap();
        assert_eq!(
            usage.prompt, 12345,
            "Output tokens are not added to measured input"
        );
        assert_eq!(usage.context_tokens, 65536);
        assert_eq!(usage.model, "local:9b");
        assert!(usage.recorded_at.is_some());
    }

    #[test]
    fn cloud_usage_skips_observers_and_accepts_complete_usage_before_truncated_output() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000009");
        request(&cycle, 0, "free-model", true, None);
        fs::write(
            cycle.join("configuration.json"),
            r#"{"model":"zen/free-model","context_tokens":1000000}"#,
        )
        .unwrap();
        response(
            &cycle,
            0,
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":87234,\"completion_tokens\":100}}\n\ndata: {\"choices\":[\n",
        );
        request(&cycle, 1, "free-model", true, None);
        response(
            &cycle,
            1,
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: {\"usage\":",
        );
        request(&cycle, 2, "free-model", false, None);
        response(
            &cycle,
            2,
            "data: {\"usage\":{\"prompt_tokens\":999999}}\n\n",
        );
        let child = cycle.join("watchdog-10/helper");
        request(&child, 99, "free-model", true, None);
        response(
            &child,
            99,
            "data: {\"usage\":{\"prompt_tokens\":888888}}\n\n",
        );
        let usage = load(state.path(), "zen/free-model", 8192).unwrap();
        assert_eq!(usage.prompt, 87234);
        assert_eq!(usage.context_tokens, 1000000);
        assert_eq!(usage.model, "zen/free-model");
    }

    #[test]
    fn groq_and_multiline_sse_use_authoritative_counts() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000001");
        request(&cycle, 0, "groq-model", true, None);
        response(
            &cycle,
            0,
            "data: {\"choices\":[],\n data: \"x_groq\":{\"usage\":{\"prompt_tokens\":3456}}}\n\ndata: [DONE]\n\n",
        );
        let usage = load(state.path(), "groq/groq-model", 32768).unwrap();
        assert_eq!(usage.prompt, 3456);
        assert_eq!(usage.context_tokens, 32768);
    }

    #[test]
    fn native_responses_restore_main_usage_and_ignore_auxiliary_calls() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000009");
        native_request(&cycle, 0, "chatgpt/account-model", true);
        fs::write(
            cycle.join("configuration.json"),
            r#"{"model":"chatgpt/account-model","context_tokens":128000}"#,
        )
        .unwrap();
        response(
            &cycle,
            0,
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":16357,\"output_tokens\":9999}}}\n\ndata: [DONE]\n\n",
        );
        native_request(&cycle, 1, "chatgpt/account-model", false);
        response(
            &cycle,
            1,
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":999999}}}\n\n",
        );
        let child = cycle.join("helpers/investigation");
        native_request(&child, 99, "chatgpt/account-model", true);
        response(
            &child,
            99,
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":888888}}}\n\n",
        );
        let usage = load(state.path(), "chatgpt/account-model", 8192).unwrap();
        assert_eq!(usage.prompt, 16357);
        assert_eq!(usage.context_tokens, 128000);
        assert_eq!(usage.model, "chatgpt/account-model");
        assert!(load(state.path(), "chatgpt/different-model", 8192).is_none());
        assert!(load(state.path(), "zen/account-model", 8192).is_none());
    }

    #[test]
    fn native_terminal_usage_survives_failure_or_torn_archive() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000001");
        native_request(&cycle, 0, "chatgpt/account-model", true);
        for terminal in [
            "response.completed",
            "response.failed",
            "response.incomplete",
        ] {
            response(
                &cycle,
                0,
                &format!(
                    "data: {{\"type\":\"{terminal}\",\"response\":{{\"usage\":{{\"input_tokens\":456,\"output_tokens\":777}}}}}}\n\ndata: {{\"type\":\"response.created\",\"response\":{{\"usage\":{{\"input_tokens\":999}}}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":\n"
                ),
            );
            assert_eq!(
                load(state.path(), "chatgpt/account-model", 128000)
                    .unwrap()
                    .prompt,
                456,
                "{terminal} has authoritative input usage; a later partial frame does not"
            );
        }
        for frame in [
            json!({"type":"response.created","response":{"usage":{"input_tokens":999}}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":0,"output_tokens":999}}}),
            json!({"type":"response.failed","response":{"usage":{"output_tokens":999}}}),
            json!({"type":"response.incomplete","response":{"usage":{"input_tokens":"999"}}}),
        ] {
            response(&cycle, 0, &format!("data: {frame}\n\n"));
            assert!(load(state.path(), "chatgpt/account-model", 128000).is_none());
        }
    }

    #[test]
    fn unknown_incomplete_and_corrupt_usage_remain_unknown() {
        let state = tempfile::tempdir().unwrap();
        assert!(load(state.path(), "local:9b", 8192).is_none());
        let cycle = state.path().join("cycle-000001");
        request(&cycle, 0, "local:9b", true, Some(8192));
        response(
            &cycle,
            0,
            "{\"done\":false,\"prompt_eval_count\":999}\n{\"done\":true,\"prompt_eval_count\":0}\n{\"done\":true,\"prompt_eval_count\":",
        );
        assert!(load(state.path(), "local:9b", 8192).is_none());
        response(&cycle, 0, "{\"done\":true,\"prompt_eval_count\":321}\n");
        fs::write(cycle.join("request-000.json"), "{broken request").unwrap();
        assert!(load(state.path(), "local:9b", 8192).is_none());
        request(&cycle, 0, "local:9b", true, Some(8192));
        fs::write(
            state.path().join("conversation.json"),
            "{broken conversation",
        )
        .unwrap();
        assert!(load(state.path(), "local:9b", 8192).is_none());
    }

    #[test]
    fn durable_unknown_or_changed_model_never_reuses_old_archives() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000001");
        request(&cycle, 0, "local:9b", true, Some(8192));
        response(&cycle, 0, "{\"done\":true,\"prompt_eval_count\":321}\n");
        let path = state.path().join("conversation.json");
        for usage in [
            Value::Null,
            json!({"prompt":0,"model":"local:9b","context_tokens":8192}),
            json!({"prompt":999,"model":"different-model","context_tokens":8192}),
            json!({"prompt":999,"model":"local:9b","context_tokens":0}),
            json!({"prompt":"not a measured count","model":"local:9b","context_tokens":8192}),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&json!({"messages":[],"last_context_usage":usage})).unwrap(),
            )
            .unwrap();
            assert!(load(state.path(), "local:9b", 8192).is_none());
        }
        fs::write(&path, r#"{"messages":[],"last_context_usage":{"prompt":777,"model":"local:9b","context_tokens":65536}}"#).unwrap();
        let usage = load(state.path(), "local:9b", 8192).unwrap();
        assert_eq!(usage.prompt, 777);
        assert_eq!(usage.context_tokens, 65536);
    }

    #[test]
    fn large_conversation_and_response_are_recovered_from_bounded_tails() {
        #[derive(Serialize)]
        struct Conversation {
            messages: Vec<Value>,
            last_context_usage: Option<Usage>,
        }
        let state = tempfile::tempdir().unwrap();
        let value = Conversation {
            messages: vec![
                json!({"role":"assistant","content":"x".repeat(TAIL_BYTES as usize + 1000)}),
            ],
            last_context_usage: Some(Usage {
                prompt: 777,
                model: "local:9b".into(),
                context_tokens: 65536,
                recorded_at: None,
            }),
        };
        fs::write(
            state.path().join("conversation.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(load(state.path(), "local:9b", 8192).unwrap().prompt, 777);
        fs::remove_file(state.path().join("conversation.json")).unwrap();
        let cycle = state.path().join("cycle-000001");
        request(&cycle, 0, "local:9b", true, Some(8192));
        response(
            &cycle,
            0,
            &format!(
                "{}\n{{\"done\":true,\"prompt_eval_count\":54321}}\n",
                "x".repeat(TAIL_BYTES as usize + 1000)
            ),
        );
        assert_eq!(load(state.path(), "local:9b", 8192).unwrap().prompt, 54321);
    }

    #[test]
    fn connection_model_distinguishes_providers_and_live_model_changes() {
        let state = tempfile::tempdir().unwrap();
        let cycle = state.path().join("cycle-000001");
        request(&cycle, 0, "shared-model", true, None);
        response(&cycle, 0, "data: {\"usage\":{\"prompt_tokens\":1111}}\n\n");
        fs::write(
            cycle.join("configuration.json"),
            r#"{"model":"groq/shared-model","context_tokens":32768}"#,
        )
        .unwrap();
        assert!(load(state.path(), "zen/shared-model", 8192).is_none());
        fs::write(
            cycle.join("connection-000.json"),
            r#"{"model":"zen/shared-model","context_tokens":65536}"#,
        )
        .unwrap();
        let usage = load(state.path(), "zen/shared-model", 8192).unwrap();
        assert_eq!(usage.prompt, 1111);
        assert_eq!(usage.context_tokens, 65536);
        assert!(load(state.path(), "zen/different-model", 8192).is_none());
    }
}
