//! Shared Chat Completions wire protocol for cloud inference providers.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Normalize internal history to OpenAI tool-call IDs and string arguments.
pub fn messages(input: &[Value]) -> Result<Vec<Value>> {
    let mut pending = std::collections::VecDeque::new();
    let mut result = Vec::new();
    let multimodal = crate::vision::openai_messages(input)?;
    for (i, m) in multimodal.iter().enumerate() {
        let mut out = json!({"role":m["role"],"content":m["content"].clone()});
        if let Some(calls) = m["tool_calls"].as_array().filter(|c| !c.is_empty()) {
            let mut translated = Vec::new();
            for (n, c) in calls.iter().enumerate() {
                let id = c["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{i}_{n}"));
                pending.push_back(id.clone());
                let a = &c["function"]["arguments"];
                let a = if let Some(s) = a.as_str() {
                    s.to_owned()
                } else {
                    a.to_string()
                };
                translated.push(json!({"id":id,"type":"function","function":{"name":c["function"]["name"],"arguments":a}}));
            }
            out["tool_calls"] = json!(translated);
        }
        if m["role"] == "tool" {
            let index = if let Some(id) = m["tool_call_id"].as_str() {
                pending
                    .iter()
                    .position(|p| p == id)
                    .context("Tool result has no matching OpenAI-compatible call ID")?
            } else {
                0
            };
            let id = pending
                .remove(index)
                .context("Tool result has no matching OpenAI-compatible call")?;
            out["tool_call_id"] = json!(id);
        }
        if m["role"] == "assistant" {
            for key in ["reasoning_content", "reasoning_details"] {
                if let Some(value) = m.get(key) {
                    out[key] = value.clone();
                }
            }
        }
        result.push(out);
    }
    ensure!(
        pending.is_empty(),
        "Cannot send OpenAI-compatible unresolved tool calls"
    );
    Ok(result)
}

/// Echo reasoning only to the exact cloud model that produced it. Interleaved
/// tool reasoning is protocol state, not a note to feed to unrelated models.
pub fn messages_for(input: &[Value], name: &str) -> Result<Vec<Value>> {
    let mut prepared = input.to_vec();
    for message in &mut prepared {
        if message["role"] != "assistant" {
            continue;
        }
        let record = message["_chuggin_reasoning"].clone();
        if record["model"] != name {
            continue;
        }
        if name.starts_with("openrouter/")
            && record["details"].as_array().is_some_and(|d| !d.is_empty())
        {
            message["reasoning_details"] = record["details"].clone();
        } else if let Some(text) = record["content"].as_str() {
            message["reasoning_content"] = json!(text);
        }
    }
    messages(&prepared)
}
#[derive(Default)]
pub struct Stream {
    calls: BTreeMap<u64, Value>,
    pub usage: Option<(u64, u64)>,
    pub reasoning_details: Vec<Value>,
}
impl Stream {
    pub fn frame(&mut self, data: &str) -> Result<Value> {
        let v: Value = serde_json::from_str(data)?;
        if v.get("error").is_some() {
            return Ok(v);
        }
        let u = v
            .get("usage")
            .filter(|v| v.is_object())
            .or_else(|| v["x_groq"].get("usage"));
        if let Some(u) = u {
            self.usage = Some((
                u["prompt_tokens"].as_u64().unwrap_or(0),
                u["completion_tokens"].as_u64().unwrap_or(0),
            ));
        }
        let c = &v["choices"][0];
        let d = &c["delta"];
        if let Some(details) = d["reasoning_details"].as_array() {
            // The provider requires the original block sequence unchanged.
            self.reasoning_details.extend(details.iter().cloned());
        }
        if let Some(calls) = d["tool_calls"].as_array() {
            for c in calls {
                let index = c["index"]
                    .as_u64()
                    .context("Missing OpenAI-compatible tool index")?;
                let out = self.calls.entry(index).or_insert_with(
                    || json!({"id":"","type":"function","function":{"name":"","arguments":""}}),
                );
                if let Some(id) = c["id"].as_str() {
                    out["id"] = json!(format!("{}{id}", out["id"].as_str().unwrap_or("")));
                }
                for k in ["name", "arguments"] {
                    if let Some(s) = c["function"][k].as_str() {
                        out["function"][k] =
                            json!(format!("{}{s}", out["function"][k].as_str().unwrap_or("")));
                    }
                }
            }
        }
        let reasoning = d
            .get("reasoning_content")
            .filter(|v| v.is_string())
            .or_else(|| d.get("reasoning").filter(|v| v.is_string()));
        Ok(
            json!({"message":{"content":d["content"],"thinking":reasoning},"done_reason":c["finish_reason"]}),
        )
    }
    pub fn calls(&self) -> Result<Vec<Value>> {
        self.calls
            .values()
            .map(|v| {
                let mut v = v.clone();
                ensure!(
                    !v["id"].as_str().unwrap_or("").is_empty()
                        && !v["function"]["name"].as_str().unwrap_or("").is_empty(),
                    "Incomplete OpenAI-compatible tool call"
                );
                v["function"]["arguments"] =
                    serde_json::from_str(v["function"]["arguments"].as_str().unwrap_or(""))
                        .context("Incomplete OpenAI-compatible arguments")?;
                ensure!(
                    v["function"]["arguments"].is_object(),
                    "OpenAI-compatible arguments must be an object"
                );
                Ok(v)
            })
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_reasoning_is_scoped_to_its_model_and_excluded_from_local_wire() {
        let input = vec![
            json!({"role":"assistant","content":"Inspecting","tool_calls":[{"id":"a","function":{"name":"read_file","arguments":{"path":"a.txt"}}}],"_chuggin_reasoning":{"model":"zen/space-bunny-free","content":"Private reasoning","details":[]}}),
            json!({"role":"tool","tool_call_id":"a","content":"Evidence"}),
        ];
        let zen = messages_for(&input, "zen/space-bunny-free").unwrap();
        assert_eq!(zen[0]["reasoning_content"], "Private reasoning");
        assert_eq!(zen[0]["tool_calls"][0]["id"], zen[1]["tool_call_id"]);
        for wire in [
            messages(&input).unwrap(),
            messages_for(&input, "openrouter/stealth/space-bunny-alpha").unwrap(),
            crate::vision::ollama_messages(&input).unwrap(),
        ] {
            let wire = serde_json::to_string(&wire).unwrap();
            assert!(!wire.contains("Private reasoning"));
            assert!(!wire.contains("_chuggin_reasoning"));
        }
    }

    #[test]
    fn streaming_reasoning_fields_and_original_blocks_are_preserved() {
        let details = vec![
            json!({"type":"reasoning.text","text":"Start ","index":0,"signature":null}),
            json!({"type":"reasoning.text","text":"end","index":0,"signature":"sig"}),
            json!({"type":"reasoning.encrypted","data":"opaque","index":1}),
        ];
        let mut stream = Stream::default();
        let first = stream.frame(&json!({"choices":[{"delta":{"reasoning_content":"Thinking","reasoning_details":[details[0]]}}]}).to_string()).unwrap();
        assert_eq!(first["message"]["thinking"], "Thinking");
        let second = stream.frame(&json!({"choices":[{"delta":{"reasoning":" more","reasoning_details":[details[1],details[2]]}}]}).to_string()).unwrap();
        assert_eq!(second["message"]["thinking"], " more");
        assert_eq!(stream.reasoning_details, details);
        let name = "openrouter/stealth/space-bunny-alpha";
        let input = vec![
            json!({"role":"assistant","content":"OK","_chuggin_reasoning":{"model":name,"content":"Thinking more","details":stream.reasoning_details}}),
        ];
        let wire = messages_for(&input, name).unwrap();
        assert_eq!(wire[0]["reasoning_details"], json!(details));
        assert!(wire[0].get("reasoning_content").is_none());
    }

    #[test]
    fn empty_interleaved_reasoning_and_usage_only_frames_are_valid() {
        let input = vec![
            json!({"role":"assistant","content":"OK","_chuggin_reasoning":{"model":"zen/space-bunny-free","content":"","details":[]}}),
        ];
        assert_eq!(
            messages_for(&input, "zen/space-bunny-free").unwrap()[0]["reasoning_content"],
            ""
        );
        let mut stream = Stream::default();
        let frame = stream
            .frame(
                &json!({"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":5}})
                    .to_string(),
            )
            .unwrap();
        assert_eq!(stream.usage, Some((40, 5)));
        assert!(frame["done_reason"].is_null());
    }
}
