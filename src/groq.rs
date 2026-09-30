//! Groq protocol and persistent, cross-project quota admission. No secrets in artifacts.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub const ENDPOINT: &str = "https://api.groq.com/openai/v1";
pub fn model_id(name: &str) -> Option<&str> {
    name.strip_prefix("groq/")
}
pub fn url(ollama: &str, name: &str) -> String {
    if model_id(name).is_some() {
        format!("{ENDPOINT}/chat/completions")
    } else {
        format!("{}/api/chat", ollama.trim_end_matches('/'))
    }
}
pub fn path(name: &str) -> Result<PathBuf> {
    Ok(crate::setup::settings_path()?.with_file_name(name))
}
pub fn key() -> Result<String> {
    let s = fs::read_to_string(path("groq.key")?)
        .context("Add your Groq key in Shared settings → Groq connection and limits")?;
    ensure!(!s.trim().is_empty(), "Groq key is empty");
    Ok(s.trim().into())
}
pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
pub fn models() -> Result<Vec<String>> {
    let r = client()?
        .get(format!("{ENDPOINT}/models"))
        .bearer_auth(key()?)
        .timeout(Duration::from_secs(20))
        .send()?;
    ensure!(
        r.status().is_success(),
        "Groq model list returned HTTP {}",
        r.status()
    );
    let b: Value = r.json()?;
    let mut names: Vec<String> = b["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .filter(|id| {
            !["whisper", "orpheus", "guard", "allam"]
                .iter()
                .any(|s| id.contains(s))
        })
        .map(|id| format!("groq/{id}"))
        .collect();
    names.sort();
    ensure!(!names.is_empty(), "No supported Groq chat models found");
    Ok(names)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_response_tokens: u32,
    pub requests_per_minute: u64,
    pub requests_per_day: u64,
    pub tokens_per_minute: u64,
    pub tokens_per_day: u64,
    pub input_tokens_per_minute: u64,
    pub output_tokens_per_minute: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_response_tokens: 1024,
            requests_per_minute: 24,
            requests_per_day: 900,
            tokens_per_minute: 7200,
            tokens_per_day: 180000,
            input_tokens_per_minute: 0,
            output_tokens_per_minute: 0,
        }
    }
}
impl Limits {
    pub fn load() -> Result<Self> {
        let p = path("groq-limits.json")?;
        let s: Self = if p.exists() {
            serde_json::from_slice(&fs::read(p)?)?
        } else {
            Self::default()
        };
        ensure!(
            (128..=32768).contains(&s.max_response_tokens)
                && s.requests_per_minute > 0
                && s.requests_per_day > 0
                && s.tokens_per_minute > 0
                && s.tokens_per_day > 0,
            "Groq combined request/token limits must be positive"
        );
        Ok(s)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    id: u64,
    at: u64,
    input: u64,
    output: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Remaining {
    tokens: u64,
    #[serde(default)]
    tokens_at: u64,
    tokens_until: u64,
    requests: u64,
    requests_until: u64,
    token_limit: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    next: u64,
    #[serde(default)]
    cooldown_until: u64,
    entries: Vec<Entry>,
    headers: BTreeMap<String, Remaining>,
    #[serde(default)]
    prompt_ratio: f64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn ledger<T>(f: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
    let p = path("groq-budget.json")?;
    fs::create_dir_all(p.parent().unwrap())?;
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path("groq-budget.lock")?)?;
    lock.lock()?;
    let mut s: Ledger = if p.exists() {
        serde_json::from_slice(&fs::read(&p)?)
            .context("Groq budget ledger needs repair; refusing to reset usage")?
    } else {
        Ledger::default()
    };
    s.entries.retain(|e| e.at.saturating_add(86400) > now());
    let r = f(&mut s)?;
    crate::setup::save(&p, &s)?;
    Ok(r)
}
fn estimate(messages: &[Value], tools: Option<&Value>, ratio: f64) -> u64 {
    // Include framing and tools; calibrate upward using actual prompt usage. Never assume cache hits.
    let bytes =
        serde_json::to_vec(messages).unwrap().len() + tools.map_or(0, |t| t.to_string().len());
    (bytes as f64 / 4.0 * ratio.max(1.2)).ceil() as u64 + 128
}
fn delay(s: &Ledger, l: &Limits, model: &str, input: u64, output: u64, at: u64) -> Result<u64> {
    let total = input.saturating_add(output);
    ensure!(
        total <= l.tokens_per_minute && total <= l.tokens_per_day,
        "Groq request needs about {total} tokens including output reservation, exceeding its allowance. Reduce context/response size or configure your account limits; waiting cannot fit this request."
    );
    ensure!(
        l.input_tokens_per_minute == 0 || input <= l.input_tokens_per_minute,
        "Groq request exceeds input token allowance"
    );
    ensure!(
        l.output_tokens_per_minute == 0 || output <= l.output_tokens_per_minute,
        "Groq response exceeds output token allowance"
    );
    let mut until = at.max(s.cooldown_until);
    // Request counts and daily tokens remain rolling windows. Minute tokens
    // replenish continuously, like Groq's reported token bucket. Reconstruct from
    // the durable charges so restarts and actual-usage settlement cannot reset it.
    for (window, requests, tokens) in [
        (60, l.requests_per_minute, u64::MAX),
        (86400, l.requests_per_day, l.tokens_per_day),
    ] {
        let entries: Vec<_> = s
            .entries
            .iter()
            .filter(|e| e.at.saturating_add(window) > at)
            .collect();
        let mut count = entries.len() as u64;
        let mut used = entries
            .iter()
            .map(|e| e.input.saturating_add(e.output))
            .sum::<u64>();
        for e in entries {
            if count < requests && used.saturating_add(total) <= tokens {
                break;
            }
            until = until.max(e.at.saturating_add(window).saturating_add(1));
            count -= 1;
            used = used.saturating_sub(e.input.saturating_add(e.output));
        }
    }
    for (capacity, requested, kind) in [
        (l.tokens_per_minute, total, 0),
        (l.input_tokens_per_minute, input, 1),
        (l.output_tokens_per_minute, output, 2),
    ] {
        if capacity > 0 {
            let available = available_tokens(&s.entries, capacity, kind, at);
            until = until.max(at.saturating_add(refill_wait(available, requested, capacity)));
        }
    }
    if let Some(h) = s.headers.get(model) {
        ensure!(
            h.token_limit == 0 || total <= h.token_limit,
            "Groq request exceeds server tokens/minute; reduce context/response size"
        );
        if h.tokens_at > 0 && h.token_limit > 0 {
            until =
                until.max(at.saturating_add(refill_wait(h.available(at), total, h.token_limit)));
        } else if h.tokens_until > at && h.tokens < total {
            // Old ledgers lack a snapshot timestamp: keep their conservative wait
            // until a fresh response establishes continuous refill timing.
            until = until.max(h.tokens_until);
        }
        if h.requests_until > at && h.requests == 0 {
            until = until.max(h.requests_until);
        }
    }
    Ok(until.saturating_sub(at))
}
pub struct Reservation {
    id: u64,
    model: String,
    estimated: u64,
}
pub enum Admission {
    Ready(Reservation),
    Wait(u64),
}
pub fn admit(
    model: &str,
    messages: &[Value],
    tools: Option<&Value>,
    output: u64,
) -> Result<Admission> {
    let l = Limits::load()?;
    ledger(|s| {
        let input = estimate(messages, tools, s.prompt_ratio);
        let seconds = delay(s, &l, model, input, output, now())?;
        if seconds > 0 {
            return Ok(Admission::Wait(seconds));
        }
        s.next += 1;
        s.entries.push(Entry {
            id: s.next,
            at: now(),
            input,
            output,
        });
        if let Some(h) = s.headers.get_mut(model) {
            if h.tokens_at > 0 && h.token_limit > 0 {
                h.tokens = h.available(now()).floor() as u64;
                h.tokens_at = now();
            }
            h.tokens = h.tokens.saturating_sub(input + output);
            h.requests = h.requests.saturating_sub(1);
        }
        Ok(Admission::Ready(Reservation {
            id: s.next,
            model: model.into(),
            estimated: input,
        }))
    })
}
impl Reservation {
    pub fn defer(&self, seconds: u64) -> Result<()> {
        ledger(|s| {
            s.cooldown_until = s.cooldown_until.max(now().saturating_add(seconds));
            Ok(())
        })
    }
    pub fn headers(&self, headers: &reqwest::header::HeaderMap) -> Result<()> {
        let get = |k: &str| headers.get(k).and_then(|s| s.to_str().ok());
        ledger(|s| {
            let h = s.headers.entry(self.model.clone()).or_default();
            if let (Some(n), Some(r)) = (
                get("x-ratelimit-remaining-tokens").and_then(|n| n.parse().ok()),
                get("x-ratelimit-reset-tokens").and_then(duration_seconds),
            ) {
                h.tokens = n;
                h.tokens_at = now();
                h.tokens_until = now() + r + 1;
            }
            if let (Some(n), Some(r)) = (
                get("x-ratelimit-remaining-requests").and_then(|n| n.parse().ok()),
                get("x-ratelimit-reset-requests").and_then(duration_seconds),
            ) {
                h.requests = n;
                h.requests_until = now() + r + 1;
            }
            if let Some(n) = get("x-ratelimit-limit-tokens").and_then(|n| n.parse().ok()) {
                h.token_limit = n;
            }
            Ok(())
        })
    }
    pub fn settle(&self, input: u64, output: u64) -> Result<()> {
        ledger(|s| {
            if let Some(e) = s.entries.iter_mut().find(|e| e.id == self.id) {
                let ratio = input as f64 / self.estimated.max(1) as f64;
                if ratio > 1.0 {
                    s.prompt_ratio = s.prompt_ratio.max(1.2) * ratio * 1.1;
                }
                e.input = input;
                e.output = output;
            }
            Ok(())
        })
    }
}
fn duration_seconds(s: &str) -> Option<u64> {
    let mut number = String::new();
    let mut seconds = 0.0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let n = number.parse::<f64>().ok()?;
        number.clear();
        seconds += n * match c {
            'd' => 86400.0,
            'h' => 3600.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                0.001
            }
            'm' => 60.0,
            's' => 1.0,
            _ => return None,
        };
    }
    if !number.is_empty() {
        seconds += number.parse::<f64>().ok()?;
    }
    (seconds.is_finite() && seconds >= 0.0).then(|| seconds.ceil() as u64)
}
/// Normalize internal history to OpenAI tool-call IDs and string arguments.
pub fn messages(input: &[Value]) -> Result<Vec<Value>> {
    let mut pending = std::collections::VecDeque::new();
    let mut result = Vec::new();
    for (i, m) in input.iter().enumerate() {
        let mut out = json!({"role":m["role"],"content":m["content"].as_str().unwrap_or("")});
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
            let fallback = pending
                .pop_front()
                .context("Tool result has no matching Groq call")?;
            out["tool_call_id"] = json!(m["tool_call_id"].as_str().unwrap_or(&fallback));
        }
        result.push(out);
    }
    ensure!(pending.is_empty(), "Cannot send Groq unresolved tool calls");
    Ok(result)
}
#[derive(Default)]
pub struct Stream {
    calls: BTreeMap<u64, Value>,
    pub usage: Option<(u64, u64)>,
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
        if let Some(calls) = d["tool_calls"].as_array() {
            for c in calls {
                let index = c["index"].as_u64().context("Missing Groq tool index")?;
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
        Ok(
            json!({"message":{"content":d["content"],"thinking":d["reasoning"]},"done_reason":c["finish_reason"]}),
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
                    "Incomplete Groq tool call"
                );
                v["function"]["arguments"] =
                    serde_json::from_str(v["function"]["arguments"].as_str().unwrap_or(""))
                        .context("Incomplete Groq arguments")?;
                ensure!(
                    v["function"]["arguments"].is_object(),
                    "Groq arguments must be an object"
                );
                Ok(v)
            })
            .collect()
    }
}

/// Keep a bounded working view, preserving system instructions, the goal, and complete
/// recent exchanges. The original conversation remains in the caller's disk artifacts.
pub fn prepare(
    name: &str,
    input: &[Value],
    tools: Option<&Value>,
    output: u64,
) -> Result<Vec<Value>> {
    let limits = Limits::load()?;
    let (ratio, cap) = ledger(|s| {
        Ok((
            s.prompt_ratio,
            s.headers.get(name).map_or(limits.tokens_per_minute, |h| {
                if h.token_limit == 0 {
                    limits.tokens_per_minute
                } else {
                    limits.tokens_per_minute.min(h.token_limit)
                }
            }),
        ))
    })?;
    fit(input, tools, output, ratio, cap.min(limits.tokens_per_day))
}
fn fit(
    input: &[Value],
    tools: Option<&Value>,
    output: u64,
    ratio: f64,
    cap: u64,
) -> Result<Vec<Value>> {
    let mut result = input.to_vec();
    let fits = |m: &[Value]| {
        estimate(m, tools, ratio)
            .saturating_add(output)
            .saturating_add(100)
            <= cap
    };
    let mut changed = false;
    while !fits(&result) {
        // Drop complete older turns only; never leave an orphan tool result.
        let end = (3..result.len())
            .find(|&i| result[i]["role"] == "assistant" || result[i]["role"] == "user");
        if let Some(end) = end.filter(|&i| result[i..].iter().any(|m| m["role"] == "assistant")) {
            result.drain(2..end);
            changed = true;
        } else {
            break;
        }
    }
    if !fits(&result) {
        for m in &mut result {
            if m["role"] == "tool"
                && let Some(s) = m["content"].as_str().filter(|s| s.len() > 1600)
            {
                m["content"] = json!(format!(
                    "{}\n[Result shortened for Groq's request budget. Inspect narrower file/output pages if details are needed; completed actions remain in place.]",
                    crate::project::excerpt(s, 1600)
                ));
                changed = true;
            }
        }
    }
    ensure!(
        fits(&result),
        "Groq's per-request budget cannot fit the goal, tools and latest exchange. Increase token limits only if your account supports it, or reduce the goal/tool output. No request was sent."
    );
    if changed {
        result.insert(2.min(result.len()), json!({"role":"user","content":"Earlier exchanges were omitted to fit Groq’s request allowance. The main goal and completed work remain. Use saved progress notes and inspect current files to recover details instead of repeating completed actions."}));
        crate::events::log("Groq request budget: using a smaller working conversation; completed actions and full request artifacts remain saved. Re-read details through tools as needed.".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_headers() {
        assert_eq!(duration_seconds("2m59.56s"), Some(180));
        assert_eq!(duration_seconds("1h2m3s"), Some(3723));
        assert_eq!(duration_seconds("1ms"), Some(1));
        assert_eq!(duration_seconds("bad"), None);
    }
    #[test]
    fn limits_cover_minutes_days_and_split_tokens() {
        let l = Limits {
            requests_per_minute: 2,
            requests_per_day: 3,
            tokens_per_minute: 1000,
            tokens_per_day: 2000,
            ..Limits::default()
        };
        let mut s = Ledger::default();
        s.entries.push(Entry {
            id: 1,
            at: 100,
            input: 400,
            output: 100,
        });
        assert_eq!(delay(&s, &l, "a", 100, 100, 101).unwrap(), 0);
        assert!((6..=7).contains(&delay(&s, &l, "a", 500, 100, 101).unwrap()));
        s.entries.push(Entry {
            id: 2,
            at: 102,
            input: 1,
            output: 1,
        });
        assert_eq!(delay(&s, &l, "b", 1, 1, 103).unwrap(), 58); // also applies across models
        s.entries.push(Entry {
            id: 3,
            at: 200,
            input: 1,
            output: 1,
        });
        assert_eq!(delay(&s, &l, "b", 1, 1, 201).unwrap(), 86300);
        assert!(delay(&s, &l, "b", 1001, 0, 90000).is_err());
        let split = Limits {
            input_tokens_per_minute: 20,
            ..l
        };
        assert!(delay(&s, &split, "a", 21, 1, 90000).is_err());
    }
    #[test]
    fn server_headers_only_tighten_budget() {
        let mut s = Ledger::default();
        s.headers.insert(
            "a".into(),
            Remaining {
                tokens: 5,
                tokens_at: 0,
                tokens_until: 160,
                requests: 0,
                requests_until: 200,
                token_limit: 1000,
            },
        );
        assert_eq!(delay(&s, &Limits::default(), "a", 10, 10, 101).unwrap(), 99);
        assert_eq!(delay(&s, &Limits::default(), "a", 10, 10, 201).unwrap(), 0);
        assert!(delay(&s, &Limits::default(), "a", 1001, 1, 201).is_err());
    }
    #[test]
    fn tool_history_has_ids_and_string_arguments() {
        let input = vec![
            json!({"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a"}}}]}),
            json!({"role":"tool","tool_name":"read_file","content":"ok"}),
        ];
        let m = messages(&input).unwrap();
        assert_eq!(
            m[0]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(m[0]["tool_calls"][0]["id"], m[1]["tool_call_id"]);
        assert!(m[1].get("tool_name").is_none());
        assert!(messages(&input[..1]).is_err());
    }
    #[test]
    fn fragmented_parallel_tool_stream_and_usage() {
        let mut s = Stream::default();
        for v in [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_","arguments":"{\"pa"}},{"index":1,"id":"call_b","function":{"name":"list_files","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"x\"}"}}]},"finish_reason":"tool_calls"}],"x_groq":{"usage":{"prompt_tokens":100,"completion_tokens":20}}}),
        ] {
            s.frame(&v.to_string()).unwrap();
        }
        let c = s.calls().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["function"]["name"], "read_file");
        assert_eq!(c[0]["function"]["arguments"], json!({"path":"x"}));
        assert_eq!(s.usage, Some((100, 20)));
    }
    #[test]
    fn incomplete_arguments_never_execute() {
        let mut s = Stream::default();
        s.frame(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"edit_file","arguments":"{"}}]}}]}).to_string()).unwrap();
        assert!(s.calls().is_err());
    }
    #[test]
    fn report_base_request_size() {
        let m = vec![
            json!({"role":"system","content":crate::prompts::WORK}),
            json!({"role":"user","content":"Build a word processor"}),
        ];
        println!(
            "Loop base estimate: {}; Chat tools estimate: {}",
            estimate(&m, Some(&crate::model::tools()), 1.2),
            estimate(&m, Some(&crate::operator::schemas()), 1.2)
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    #[test]
    #[ignore = "One live request with the full loop tool schema; no tool execution"]
    fn live_groq_full_tool_schema() {
        let model = crate::model::Model::new(
            "http://unused",
            "groq/openai/gpt-oss-120b",
            8192,
            1024,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let m = vec![
            json!({"role":"system","content":crate::prompts::WORK}),
            json!({"role":"user","content":"Protocol test: call read_file for Cargo.toml once. Do not set a task or make changes. The caller will validate your tool request without executing it."}),
        ];
        let response = model.watchdog_chat(&m, crate::model::tools()).unwrap();
        assert!(
            response["tool_calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["function"]["name"] == "read_file")
        );
    }
    #[test]
    #[ignore = "Uses a configured Groq key for two small paid/free-account requests"]
    fn live_groq_tool_continuation() {
        let model = crate::model::Model::new(
            "http://unused",
            "groq/openai/gpt-oss-120b",
            8192,
            512,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let tools = json!([{"type":"function","function":{"name":"lookup_test_value","description":"Return the value of a harmless test fixture.","parameters":{"type":"object","properties":{},"required":[]}}}]);
        let mut m = vec![
            json!({"role":"system","content":"This is a bounded protocol test. Call lookup_test_value once, then repeat its result to the user in one sentence. Do not invent the value."}),
            json!({"role":"user","content":"What is the fixture value?"}),
        ];
        let response = model.watchdog_chat(&m, tools.clone()).unwrap();
        assert_eq!(response["tool_calls"].as_array().unwrap().len(), 1);
        let call = response["tool_calls"][0].clone();
        assert_eq!(call["function"]["name"], "lookup_test_value");
        m.push(response);
        m.push(json!({"role":"tool","tool_call_id":call["id"],"tool_name":"lookup_test_value","content":"orchid-42"}));
        let answer = model.watchdog_chat(&m, tools).unwrap();
        assert!(answer["content"].as_str().unwrap().contains("orchid-42"));
        println!("Groq streamed tool call and paired tool-result continuation passed.");
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use std::process::Command;
    #[test]
    #[ignore = "Subprocess fixture; launched by ledger_is_shared_and_survives_restart"]
    fn quota_worker() {
        assert!(std::env::var_os("CHUGGIN_QUOTA_FIXTURE").is_some());
        let m = vec![json!({"role":"user","content":"fixture"})];
        let ready = matches!(
            admit("groq/fixture", &m, None, 100).unwrap(),
            Admission::Ready(_)
        );
        fs::write(
            path(&format!("result-{}", std::process::id())).unwrap(),
            if ready { "ready" } else { "wait" },
        )
        .unwrap();
    }
    #[test]
    fn ledger_is_shared_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("chuggin");
        fs::create_dir_all(&base).unwrap();
        let limits = Limits {
            requests_per_minute: 1,
            ..Limits::default()
        };
        fs::write(
            base.join("groq-limits.json"),
            serde_json::to_vec(&limits).unwrap(),
        )
        .unwrap();
        let spawn = || {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "groq::persistence_tests::quota_worker",
                    "--ignored",
                ])
                .env("XDG_CONFIG_HOME", dir.path())
                .env("CHUGGIN_QUOTA_FIXTURE", "1")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut children = (0..4).map(|_| spawn()).collect::<Vec<_>>();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        // A later process cannot reset the reserved request by restarting.
        assert!(spawn().wait().unwrap().success());
        let values: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("result-"))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert_eq!(values.len(), 5);
        assert_eq!(values.iter().filter(|v| *v == "ready").count(), 1);
        let ledger: Ledger =
            serde_json::from_slice(&fs::read(base.join("groq-budget.json")).unwrap()).unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].output, 100);
    }
    #[test]
    fn context_fitting_preserves_goal_and_latest_complete_tools() {
        let mut input = vec![
            json!({"role":"system","content":"Rules"}),
            json!({"role":"user","content":"The main goal"}),
        ];
        for i in 0..12 {
            input.push(json!({"role":"assistant","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read_file","arguments":{"path":"file"}}}]}));
            input.push(
                json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(1000)}),
            );
        }
        let result = fit(&input, None, 100, 1.2, 1200).unwrap();
        assert_eq!(result[..2], input[..2]);
        assert_eq!(result[result.len() - 1], input[input.len() - 1]);
        assert!(result.len() < input.len());
        assert!(messages(&result).is_ok());
    }
    #[test]
    fn latest_human_instruction_is_not_discarded() {
        let input = vec![
            json!({"role":"system","content":"Rules"}),
            json!({"role":"user","content":"The main goal"}),
            json!({"role":"user","content":"x".repeat(10000)}),
        ];
        assert!(fit(&input, None, 100, 1.2, 1200).is_err());
    }
    #[test]
    fn oversized_tool_result_is_explicitly_shortened() {
        let input = vec![
            json!({"role":"system","content":"Rules"}),
            json!({"role":"user","content":"The main goal"}),
            json!({"role":"assistant","tool_calls":[{"id":"a","function":{"name":"read_file","arguments":{}}}]}),
            json!({"role":"tool","tool_call_id":"a","content":"x".repeat(10000)}),
        ];
        let result = fit(&input, None, 100, 1.2, 1200).unwrap();
        assert!(
            result.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("Result shortened")
        );
        assert!(messages(&result).is_ok());
    }
}

impl Remaining {
    fn available(&self, at: u64) -> f64 {
        (self.tokens as f64
            + at.saturating_sub(self.tokens_at) as f64 * self.token_limit as f64 / 60.0)
            .min(self.token_limit as f64)
    }
}
fn refill_wait(available: f64, requested: u64, capacity: u64) -> u64 {
    if available >= requested as f64 {
        return 0;
    }
    (((requested as f64 - available) * 60.0 / capacity as f64).ceil() as u64).saturating_add(1)
}
fn available_tokens(entries: &[Entry], capacity: u64, kind: u8, at: u64) -> f64 {
    let mut available = capacity as f64;
    let mut previous = entries.first().map_or(at, |e| e.at);
    for e in entries {
        available = (available + e.at.saturating_sub(previous) as f64 * capacity as f64 / 60.0)
            .min(capacity as f64);
        available -= match kind {
            1 => e.input,
            2 => e.output,
            _ => e.input.saturating_add(e.output),
        } as f64;
        previous = previous.max(e.at);
    }
    (available + at.saturating_sub(previous) as f64 * capacity as f64 / 60.0).min(capacity as f64)
}

#[cfg(test)]
mod refill_tests {
    use super::*;
    #[test]
    fn partial_refill_is_available_before_the_minute_rolls_over() {
        let entries = vec![Entry {
            id: 1,
            at: 100,
            input: 4800,
            output: 0,
        }];
        assert_eq!(available_tokens(&entries, 7200, 0, 100), 2400.0);
        assert_eq!(available_tokens(&entries, 7200, 0, 120), 4800.0);
        assert_eq!(available_tokens(&entries, 7200, 0, 140), 7200.0);
        assert_eq!(refill_wait(4800.0, 6000, 7200), 11);
        let roundtrip: Vec<Entry> =
            serde_json::from_str(&serde_json::to_string(&entries).unwrap()).unwrap();
        assert_eq!(available_tokens(&roundtrip, 7200, 0, 120), 4800.0);
    }
    #[test]
    fn repeated_requests_cannot_borrow_unlimited_refill_capacity() {
        let entries = vec![
            Entry {
                id: 1,
                at: 100,
                input: 6000,
                output: 0,
            },
            Entry {
                id: 2,
                at: 140,
                input: 6000,
                output: 0,
            },
        ];
        assert_eq!(available_tokens(&entries, 7200, 0, 140), 0.0);
        assert_eq!(refill_wait(0.0, 6000, 7200), 51);
        assert_eq!(available_tokens(&entries, 7200, 0, 1000), 7200.0);
    }
    #[test]
    fn server_snapshot_refills_without_waiting_for_a_full_bucket() {
        let mut s = Ledger::default();
        s.headers.insert(
            "a".into(),
            Remaining {
                tokens: 2000,
                tokens_at: 100,
                tokens_until: 146,
                token_limit: 8000,
                requests: 50,
                requests_until: 200,
            },
        );
        let wait = delay(&s, &Limits::default(), "a", 5500, 500, 110).unwrap();
        assert!((20..=22).contains(&wait));
        s.cooldown_until = 150;
        assert_eq!(
            delay(&s, &Limits::default(), "a", 5500, 500, 110).unwrap(),
            40
        );
    }
}
