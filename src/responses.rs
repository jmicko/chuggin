//! Stateless Responses transport for official ChatGPT plan usage. Local tools
//! remain client-executed; no call is executable until the response completes.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const NAMESPACE: &str = "chuggin";

pub fn request(
    model: &str,
    messages: &[Value],
    tools: Option<&Value>,
    format: Option<&Value>,
) -> Result<Value> {
    let id = crate::chatgpt::model_id(model).context("Missing ChatGPT model ID")?;
    ensure!(!id.trim().is_empty(), "Missing ChatGPT model ID");
    let mut body = json!({
        "model":id,"input":input(messages, model)?,"store":false,"stream":true,
        "include":["reasoning.encrypted_content"]
    });
    if let Some(tools) = tools {
        let mut functions = Vec::new();
        for tool in tools.as_array().context("Invalid Responses tool list")? {
            ensure!(
                tool["type"] == "function",
                "Unsupported Responses tool type"
            );
            let function = &tool["function"];
            let name = function["name"]
                .as_str()
                .context("Missing Responses tool name")?;
            ensure!(!name.is_empty(), "Missing Responses tool name");
            ensure!(
                function["parameters"].is_object(),
                "Missing Responses tool parameters"
            );
            let mut out = json!({"type":"function","name":name,"parameters":function["parameters"],
                // Existing schemas deliberately use optional arguments. The
                // API must not normalize those into required strict fields.
                "strict":false});
            if let Some(description) = function["description"].as_str() {
                out["description"] = json!(description);
            }
            functions.push(out);
        }
        if !functions.is_empty() {
            body["tools"] = json!([{"type":"namespace","name":NAMESPACE,
                "description":"Tools for inspecting and working on the current project.","tools":functions}]);
        }
    }
    if format.is_some() {
        // The existing structured prompt carries the exact schema and its
        // repair logic; JSON mode avoids changing optional schema semantics.
        body["text"] = json!({"format":{"type":"json_object"}});
    }
    Ok(body)
}

fn input(messages: &[Value], model: &str) -> Result<Vec<Value>> {
    let messages = crate::vision::responses_messages(messages)?;
    let mut output = Vec::new();
    let mut pending = VecDeque::new();
    for (index, message) in messages.iter().enumerate() {
        let role = message["role"]
            .as_str()
            .context("Missing Responses message role")?;
        let native = &message["_chuggin_responses"];
        if role == "assistant" && native["model"] == model {
            let items = native["items"]
                .as_array()
                .context("Invalid saved Responses items")?;
            for item in items {
                match item["type"].as_str() {
                    Some("function_call") => {
                        validate_call(item)?;
                        pending.push_back(item["call_id"].as_str().unwrap().to_owned());
                    }
                    Some("message" | "reasoning") => {}
                    _ => bail!("Unsupported saved Responses output item"),
                }
                output.push(item.clone());
            }
            continue;
        }
        if role == "tool" {
            let matching = if let Some(id) = message["tool_call_id"].as_str() {
                pending
                    .iter()
                    .position(|pending| pending == id)
                    .context("Tool output has no matching Responses call ID")?
            } else {
                0
            };
            let id = pending
                .remove(matching)
                .context("Tool output has no matching Responses call")?;
            let content = &message["content"];
            output.push(json!({"type":"function_call_output","call_id":id,
                "output":content.as_str().map(str::to_owned).unwrap_or_else(||content.to_string())}));
            continue;
        }
        ensure!(
            matches!(role, "system" | "developer" | "user" | "assistant"),
            "Unsupported Responses message role"
        );
        let role = if role == "system" { "developer" } else { role };
        let content = content(&message["content"], role)?;
        if !content.is_empty() {
            output.push(json!({"type":"message","role":role,"content":content}));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            ensure!(
                role == "assistant",
                "Responses calls must belong to an assistant"
            );
            for (number, call) in calls.iter().enumerate() {
                let id = call["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{index}_{number}"));
                let arguments = &call["function"]["arguments"];
                let arguments = arguments
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| arguments.to_string());
                let item = json!({"type":"function_call","call_id":id,
                    "name":call["function"]["name"],"namespace":NAMESPACE,"arguments":arguments});
                validate_call(&item)?;
                pending.push_back(id);
                output.push(item);
            }
        }
    }
    ensure!(
        pending.is_empty(),
        "Cannot send Responses unresolved tool calls"
    );
    Ok(output)
}

fn content(value: &Value, role: &str) -> Result<Vec<Value>> {
    // Reconstructed history uses EasyInputMessage content. Original native
    // assistant output items, with output_text/id/status/phase, replay above.
    let text_type = "input_text";
    if let Some(text) = value.as_str() {
        return Ok((!text.is_empty())
            .then(|| json!({"type":text_type,"text":text}))
            .into_iter()
            .collect());
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    for part in value
        .as_array()
        .context("Invalid Responses message content")?
    {
        match part["type"].as_str() {
            Some("text") => output.push(json!({"type":text_type,"text":part["text"].as_str().context("Invalid Responses text")?})),
            Some("image_url") => {
                ensure!(role == "user", "Responses image input must belong to a user");
                let url = part["image_url"]["url"].as_str().context("Invalid Responses image URL")?;
                output.push(json!({"type":"input_image","image_url":url}));
            }
            _ => bail!("Unsupported Responses content part"),
        }
    }
    Ok(output)
}

fn call_name(item: &Value) -> Result<&str> {
    let name = item["name"]
        .as_str()
        .context("Missing Responses function name")?;
    let namespace = item["namespace"].as_str();
    ensure!(
        namespace.is_none() || namespace == Some(NAMESPACE),
        "Unexpected Responses tool namespace"
    );
    let name = name.strip_prefix("chuggin.").unwrap_or(name);
    ensure!(
        !name.is_empty() && !name.contains('.'),
        "Invalid Responses function name"
    );
    Ok(name)
}

fn validate_call(item: &Value) -> Result<Value> {
    ensure!(
        item["type"] == "function_call",
        "Invalid Responses function item"
    );
    ensure!(
        !item["call_id"].as_str().unwrap_or("").is_empty(),
        "Missing Responses call ID"
    );
    ensure!(
        item["status"].is_null() || item["status"] == "completed",
        "Incomplete Responses function call"
    );
    let name = call_name(item)?;
    let arguments: Value = serde_json::from_str(
        item["arguments"]
            .as_str()
            .context("Missing Responses function arguments")?,
    )
    .context("Incomplete Responses function arguments")?;
    ensure!(
        arguments.is_object(),
        "Responses function arguments must be an object"
    );
    Ok(
        json!({"id":item["call_id"],"type":"function","function":{"name":name,"arguments":arguments}}),
    )
}

/// Map documented plan-usage failures to the shared durable backoff mechanism.
/// Never print a provider body that could contain private input or credentials.
pub fn error(value: &Value) -> anyhow::Error {
    let error = value.get("error").unwrap_or(value);
    match error["code"].as_str().unwrap_or("") {
        "subscription_sharing_usage_limit_exceeded" | "rate_limit_exceeded" => crate::provider::Unavailable {
            reason:"ChatGPT plan usage limit reached; check ChatGPT settings → Usage or choose another provider",
            retry_after:None,
        }.into(),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" | "server_error" => crate::provider::Unavailable {
            reason:"ChatGPT plan usage is temporarily unavailable",retry_after:None,
        }.into(),
        "subscription_sharing_invalid_user" => crate::provider::Unavailable {
            reason:"ChatGPT authentication needs attention; sign in again in Global settings",retry_after:None,
        }.into(),
        "subscription_sharing_user_not_eligible" => anyhow::anyhow!("ChatGPT plan usage is unavailable for this account or workspace policy"),
        "subscription_sharing_unsupported_capability" => anyhow::anyhow!("ChatGPT plan usage rejected an unsupported request capability"),
        "subscription_sharing_route_not_supported" | "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => anyhow::anyhow!("ChatGPT sign-in permission does not authorize this inference route; review the connection in Global settings"),
        _ => crate::provider::classify(200, &error.to_string(), None)
            .map(anyhow::Error::from)
            .unwrap_or_else(||anyhow::anyhow!("ChatGPT Responses inference failed")),
    }
}

#[derive(Default)]
pub struct Stream {
    items: BTreeMap<u64, Value>,
    finished_items: BTreeSet<u64>,
    pub usage: Option<(u64, u64)>,
    pub completed: bool,
    text: String,
}
impl Stream {
    pub fn frame(&mut self, data: &str) -> Result<Value> {
        let event: Value = serde_json::from_str(data).context("Invalid Responses stream JSON")?;
        let event_type = event["type"]
            .as_str()
            .context("Missing Responses stream event type")?;
        if let Some(usage) = event["response"]["usage"].as_object()
            && let (Some(input), Some(output)) = (
                usage.get("input_tokens").and_then(Value::as_u64),
                usage.get("output_tokens").and_then(Value::as_u64),
            )
        {
            self.usage = Some((input, output));
        }
        match event_type {
            "response.output_item.added" | "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .context("Missing Responses output index")?;
                ensure!(event["item"].is_object(), "Missing Responses output item");
                self.items.insert(index, event["item"].clone());
                if event_type.ends_with(".done") {
                    self.finished_items.insert(index);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .context("Missing Responses function index")?;
                let item = self
                    .items
                    .get_mut(&index)
                    .context("Responses arguments have no function item")?;
                ensure!(
                    item["type"] == "function_call",
                    "Responses arguments do not match a function"
                );
                if let Some(id) = event["item_id"].as_str() {
                    ensure!(
                        item["id"] == id,
                        "Responses arguments reference a different function"
                    );
                }
                if event_type.ends_with(".done") {
                    item["arguments"] = json!(
                        event["arguments"]
                            .as_str()
                            .context("Missing complete Responses arguments")?
                    );
                } else {
                    let delta = event["delta"]
                        .as_str()
                        .context("Missing Responses arguments delta")?;
                    let previous = item["arguments"]
                        .as_str()
                        .context("Invalid Responses arguments buffer")?;
                    item["arguments"] = json!(format!("{previous}{delta}"));
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .context("Missing Responses text delta")?;
                self.text.push_str(delta);
                return Ok(json!({"message":{"content":delta}}));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                return Ok(
                    json!({"message":{"thinking":event["delta"].as_str().context("Missing Responses reasoning delta")?}}),
                );
            }
            "response.failed" => return Err(error(&event["response"])),
            "error" => return Err(error(&event)),
            "response.incomplete" => {
                let reason = event["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("");
                ensure!(
                    reason == "max_output_tokens",
                    "ChatGPT Responses inference ended incomplete"
                );
                return Ok(json!({"done":true,"done_reason":"length",
                    "prompt_eval_count":self.usage.map(|u|u.0),"eval_count":self.usage.map(|u|u.1)}));
            }
            "response.completed" => {
                ensure!(
                    event["response"]["status"].is_null()
                        || event["response"]["status"] == "completed",
                    "Responses terminal status is not completed"
                );
                if let Some(items) = event["response"]["output"].as_array() {
                    self.items = items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| (i as u64, item.clone()))
                        .collect();
                    self.finished_items = self.items.keys().copied().collect();
                }
                ensure!(
                    self.items
                        .keys()
                        .all(|index| self.finished_items.contains(index)),
                    "Responses completed with unfinished output items"
                );
                for item in self.items.values() {
                    if item["type"] == "function_call" {
                        validate_call(item)?;
                    } else {
                        ensure!(
                            matches!(item["type"].as_str(), Some("message" | "reasoning")),
                            "Unsupported Responses output item"
                        );
                    }
                }
                let full_text: String = self
                    .items
                    .values()
                    .filter(|item| item["type"] == "message")
                    .filter_map(|item| item["content"].as_array())
                    .flatten()
                    .filter_map(|part| match part["type"].as_str() {
                        Some("output_text") => part["text"].as_str(),
                        Some("refusal") => part["refusal"].as_str(),
                        _ => None,
                    })
                    .collect();
                let remaining = if full_text.is_empty() {
                    ""
                } else {
                    full_text
                        .strip_prefix(&self.text)
                        .context("Responses completed text differs from its stream")?
                };
                self.completed = true;
                return Ok(json!({"message":{"content":remaining},"done":true,
                    "prompt_eval_count":self.usage.map(|u|u.0),"eval_count":self.usage.map(|u|u.1)}));
            }
            _ => {}
        }
        Ok(json!({}))
    }
    pub fn calls(&self) -> Result<Vec<Value>> {
        ensure!(
            self.completed,
            "Responses stream has not completed; tool calls were not executed"
        );
        let mut ids = BTreeSet::new();
        self.items
            .values()
            .filter(|item| item["type"] == "function_call")
            .map(|item| {
                ensure!(
                    ids.insert(item["call_id"].as_str().unwrap_or("")),
                    "Duplicate Responses call ID"
                );
                validate_call(item)
            })
            .collect()
    }
    pub fn items(&self) -> Result<Vec<Value>> {
        ensure!(self.completed, "Responses stream has not completed");
        Ok(self.items.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(stream: &mut Stream, event: Value) -> Value {
        stream.frame(&event.to_string()).unwrap()
    }
    fn function(arguments: &str) -> Value {
        json!({"type":"function_call","id":"fc_1","call_id":"call_1","namespace":"chuggin","name":"read_file","arguments":arguments,"status":"completed"})
    }
    #[test]
    fn plan_request_preserves_history_namespaced_tools_and_stateless_fields() {
        let messages = vec![
            json!({"role":"system","content":"Project instructions"}),
            json!({"role":"user","content":"Read evidence"}),
            json!({"role":"assistant","content":"Checking","tool_calls":[{"id":"call_a","function":{"name":"read_file","arguments":{"path":"a.txt"}}}]}),
            json!({"role":"tool","tool_call_id":"call_a","content":"Evidence"}),
        ];
        let tools = json!([{"type":"function","function":{"name":"read_file","description":"Read project evidence","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}]);
        let body = request("chatgpt/account-model", &messages, Some(&tools), None).unwrap();
        assert_eq!(body["model"], "account-model");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][3]["type"], "function_call");
        assert_eq!(body["input"][2]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][3]["call_id"], body["input"][4]["call_id"]);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["tools"][0]["strict"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        for field in [
            "messages",
            "previous_response_id",
            "max_output_tokens",
            "temperature",
            "metadata",
            "conversation",
        ] {
            assert!(body.get(field).is_none(), "{field}");
        }
        assert!(request("chatgpt/account-model", &messages[..3], Some(&tools), None).is_err());
    }
    #[test]
    fn streamed_calls_remain_unusable_until_completed_and_valid() {
        let mut stream = Stream::default();
        let mut added = function("");
        added["status"] = json!("in_progress");
        frame(
            &mut stream,
            json!({"type":"response.output_item.added","output_index":0,"item":added}),
        );
        frame(
            &mut stream,
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"path\":"}),
        );
        assert!(stream.calls().is_err());
        frame(
            &mut stream,
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"\"a.txt\"}"}),
        );
        frame(
            &mut stream,
            json!({"type":"response.output_item.done","output_index":0,"item":function("{\"path\":\"a.txt\"}")}),
        );
        assert!(stream.calls().is_err());
        frame(
            &mut stream,
            json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":512,"output_tokens":20}}}),
        );
        let calls = stream.calls().unwrap();
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["arguments"]["path"], "a.txt");
        assert_eq!(stream.usage, Some((512, 20)));
        let mut broken = Stream::default();
        assert!(broken.frame(&json!({"type":"response.completed","response":{"output":[function("{\"path\":")]}}).to_string()).is_err());
        assert!(broken.calls().is_err());
    }
    #[test]
    fn same_model_replays_original_encrypted_reasoning_and_calls() {
        let items = json!([{"id":"rs_1","type":"reasoning","summary":[],"encrypted_content":"opaque"},function("{\"path\":\"a.txt\"}")]);
        let messages = vec![
            json!({"role":"assistant","content":"","tool_calls":[{"id":"call_1","function":{"name":"read_file","arguments":{"path":"a.txt"}}}],
            "_chuggin_responses":{"model":"chatgpt/a","items":items}}),
            json!({"role":"tool","tool_call_id":"call_1","content":"Evidence"}),
        ];
        let same = request("chatgpt/a", &messages, None, None).unwrap();
        assert_eq!(same["input"][0], items[0]);
        assert_eq!(same["input"][1], items[1]);
        let other = request("chatgpt/b", &messages, None, None).unwrap();
        assert!(!other.to_string().contains("opaque"));
        assert_eq!(other["input"][0]["type"], "function_call");
        assert_eq!(other["input"][0]["call_id"], other["input"][1]["call_id"]);
    }
    #[test]
    fn terminal_failures_keep_usage_but_never_release_tool_calls() {
        for code in [
            "subscription_sharing_usage_limit_exceeded",
            "subscription_sharing_usage_unavailable",
        ] {
            let mut stream = Stream::default();
            let error = stream.frame(&json!({"type":"response.failed","response":{"error":{"code":code},"usage":{"input_tokens":99,"output_tokens":1}}}).to_string()).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::provider::Unavailable>()
                    .is_some()
            );
            assert_eq!(stream.usage, Some((99, 1)));
            assert!(stream.calls().is_err());
        }
        let mut stream = Stream::default();
        let frame = frame(
            &mut stream,
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":99,"output_tokens":1}}}),
        );
        assert_eq!(frame["done_reason"], "length");
        assert!(stream.calls().is_err());
        assert!(
            error(&json!({"code":"subscription_sharing_user_not_eligible"}))
                .downcast_ref::<crate::provider::Unavailable>()
                .is_none()
        );
    }
    #[test]
    fn text_reasoning_and_terminal_output_do_not_duplicate_deltas() {
        let mut stream = Stream::default();
        assert_eq!(
            frame(
                &mut stream,
                json!({"type":"response.output_text.delta","delta":"Hello"})
            )["message"]["content"],
            "Hello"
        );
        assert_eq!(
            frame(
                &mut stream,
                json!({"type":"response.reasoning_summary_text.delta","delta":"Considering"})
            )["message"]["thinking"],
            "Considering"
        );
        let frame = frame(
            &mut stream,
            json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello world"}]}]}}),
        );
        assert_eq!(frame["message"]["content"], " world");
        assert!(stream.calls().unwrap().is_empty());
    }
    #[test]
    fn user_images_convert_to_native_input_images() {
        let messages = [
            json!({"role":"user","content":[{"type":"text","text":"Inspect"},{"type":"image_url","image_url":{"url":"data:image/png;base64,fixture"}}]}),
        ];
        let body = request("chatgpt/account-model", &messages, None, None).unwrap();
        assert_eq!(
            body["input"][0]["content"][1],
            json!({"type":"input_image","image_url":"data:image/png;base64,fixture"})
        );
    }
    #[test]
    fn new_tool_images_preserve_prior_native_reasoning_and_assistant_phase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        image::RgbImage::new(2, 2).save(&path).unwrap();
        let item = json!({"type":"reasoning","id":"rs_prior","summary":[],"encrypted_content":"opaque-prior"});
        let assistant = json!({"type":"message","id":"msg_prior","role":"assistant","phase":"commentary","status":"completed","content":[{"type":"output_text","text":"Inspecting","annotations":[]}]});
        let call = json!({"type":"function_call","id":"fc_image","call_id":"image","namespace":"chuggin","name":"view_image","arguments":"{\"path\":\"image.png\"}","status":"completed"});
        let messages = [
            json!({"role":"assistant","content":"Inspecting","tool_calls":[{"id":"image","function":{"name":"view_image","arguments":{"path":"image.png"}}}],
            "_chuggin_responses":{"model":"chatgpt/account-model","items":[item,assistant,call]}}),
            crate::vision::tool_reply(
                "view_image",
                &json!({"_chuggin_images":[{"path":path,"mime_type":"image/png","width":2,"height":2}]}),
                Some(&json!("image")),
            ),
        ];
        let body = request("chatgpt/account-model", &messages, None, None).unwrap();
        assert_eq!(body["input"][0], item);
        assert_eq!(body["input"][1], assistant);
        assert_eq!(body["input"][2], call);
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["input"][4]["content"][1]["type"], "input_image");
        let other = request("chatgpt/other-model", &messages, None, None).unwrap();
        assert!(!other.to_string().contains("opaque-prior"));
        assert!(
            crate::vision::ollama_messages(&messages)
                .unwrap()
                .iter()
                .all(|message| message.get("_chuggin_responses").is_none())
        );
    }
}
