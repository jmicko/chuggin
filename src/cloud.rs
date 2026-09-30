//! Free-only OpenAI-compatible cloud routes. Public metadata is briefly cached;
//! requests must also use Zen's anonymous credential or OpenRouter's zero-price
//! routing cap so a promotion ending cannot charge an account between refreshes.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

const ZEN_ENDPOINT: &str = "https://opencode.ai/zen/v1";
const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1";
const ZEN_METADATA: &str = "https://models.dev/api.json";
const CATALOG_TTL: Duration = Duration::from_secs(60);
const CATALOG_SIZE_LIMIT: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Provider {
    Zen,
    OpenRouter,
}

impl Provider {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Zen => "zen/",
            Self::OpenRouter => "openrouter/",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Zen => "OpenCode Zen",
            Self::OpenRouter => "OpenRouter",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Zen => ZEN_ENDPOINT,
            Self::OpenRouter => OPENROUTER_ENDPOINT,
        }
    }
}

pub fn provider(name: &str) -> Option<Provider> {
    [Provider::Zen, Provider::OpenRouter]
        .into_iter()
        .find(|provider| name.starts_with(provider.prefix()))
}

pub fn model_id(name: &str) -> Option<&str> {
    provider(name).and_then(|provider| name.strip_prefix(provider.prefix()))
}

pub fn url(ollama: &str, name: &str) -> String {
    provider(name).map_or_else(
        || crate::groq::url(ollama, name),
        |provider| format!("{}/chat/completions", provider.endpoint()),
    )
}

pub fn label<'a>(ollama: &'a str, name: &str) -> &'a str {
    match provider(name) {
        Some(provider) => provider.label(),
        None if crate::groq::model_id(name).is_some() => "Groq",
        None => ollama,
    }
}

pub fn key(name: &str) -> Result<Option<String>> {
    match provider(name) {
        // The official OpenCode loader uses this anonymous sentinel. Never
        // inherit an account key: a formerly free route must reject, not bill.
        Some(Provider::Zen) => Ok(Some("public".into())),
        Some(Provider::OpenRouter) => {
            let key = match fs::read_to_string(crate::groq::path("openrouter.key")?) {
                Ok(key) => key,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("Could not read the saved OpenRouter key"),
            };
            Ok((!key.trim().is_empty()).then(|| key.trim().into()))
        }
        None => Ok(None),
    }
}

type CatalogCache = HashMap<Provider, (Instant, Value)>;

fn cache() -> &'static Mutex<CatalogCache> {
    static CACHE: OnceLock<Mutex<CatalogCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_catalog(provider: Provider) -> Option<Value> {
    cache()
        .lock()
        .ok()?
        .get(&provider)
        .and_then(|(at, value)| (at.elapsed() < CATALOG_TTL).then(|| value.clone()))
}

fn store_catalog(provider: Provider, value: &Value) -> Result<()> {
    entries(value)?;
    cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("Cloud model catalog cache is unavailable"))?
        .insert(provider, (Instant::now(), value.clone()));
    Ok(())
}

/// Get current public metadata. Expired metadata never substitutes for a failed
/// refresh. No credentials or project content are sent by catalog requests.
pub fn catalog(provider: Provider) -> Result<Value> {
    if let Some(value) = cached_catalog(provider) {
        return Ok(value);
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("chuggin/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let available = fetch_json(&client, &format!("{}/models", provider.endpoint()))?;
    let value = if provider == Provider::Zen {
        let metadata = fetch_json(&client, ZEN_METADATA)?;
        merge_zen_catalog(&available, &metadata)?
    } else {
        available
    };
    store_catalog(provider, &value)?;
    Ok(value)
}

fn fetch_json(client: &reqwest::blocking::Client, url: &str) -> Result<Value> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(|_| crate::provider::Unavailable {
            reason: "Public cloud model metadata is temporarily unavailable",
            retry_after: None,
        })?;
    if !response.status().is_success() {
        let retry_after = crate::provider::retry_after(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            SystemTime::now(),
        );
        if let Some(error) = crate::provider::classify(response.status().as_u16(), "", retry_after)
        {
            return Err(error.into());
        }
    }
    ensure!(
        response.status().is_success(),
        "Cloud model catalog returned HTTP {}",
        response.status()
    );
    let mut body = Vec::new();
    response
        .take(CATALOG_SIZE_LIMIT + 1)
        .read_to_end(&mut body)
        .map_err(|_| crate::provider::Unavailable {
            reason: "Public cloud model metadata was interrupted",
            retry_after: None,
        })?;
    ensure!(
        body.len() as u64 <= CATALOG_SIZE_LIMIT,
        "Cloud model catalog exceeds its size limit"
    );
    serde_json::from_slice(&body).context("Could not read public cloud model metadata")
}

fn entries(value: &Value) -> Result<&Vec<Value>> {
    value["data"]
        .as_array()
        .context("Cloud model catalog is missing its model list")
}

// Zen's /models endpoint publishes availability only. OpenCode's own models.dev
// database supplies pricing, tool support, and the per-model SDK/endpoint type.
fn merge_zen_catalog(available: &Value, metadata: &Value) -> Result<Value> {
    let provider = &metadata["opencode"];
    let models = provider["models"]
        .as_object()
        .context("OpenCode model pricing metadata is unavailable")?;
    let default_sdk = provider["npm"]
        .as_str()
        .context("OpenCode protocol metadata is unavailable")?;
    let mut merged = Vec::new();
    for entry in entries(available)? {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        let Some(model) = models.get(id) else {
            continue;
        };
        let mut model = model.clone();
        ensure!(
            model["id"] == id,
            "OpenCode model metadata ID does not match availability"
        );
        if model["provider"]["npm"].as_str().is_none() {
            model["provider"] = json!({"npm":default_sdk});
        }
        merged.push(model);
    }
    Ok(json!({"data":merged}))
}

pub fn models(provider: Provider) -> Result<Vec<String>> {
    models_from_catalog(provider, &catalog(provider)?)
}

/// Pure catalog filtering, also used by discovery and request validation.
pub fn models_from_catalog(provider: Provider, catalog: &Value) -> Result<Vec<String>> {
    let entries = entries(catalog)?;
    let mut counts = BTreeMap::new();
    for entry in entries {
        if let Some(id) = entry["id"].as_str() {
            *counts.entry(id).or_insert(0usize) += 1;
        }
    }
    let mut names: Vec<String> = entries
        .iter()
        .filter(|entry| free_chat_model(provider, entry))
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| counts.get(id) == Some(&1))
        .map(|id| format!("{}{id}", provider.prefix()))
        .collect();
    names.sort();
    ensure!(
        !names.is_empty(),
        "No free tool-capable Chat Completions models are currently available from {}",
        provider.label()
    );
    Ok(names)
}

pub fn validate(name: &str) -> Result<()> {
    let Some(provider) = provider(name) else {
        return Ok(());
    };
    validate_catalog(provider, &catalog(provider)?, name)
}

/// Only enable provider-enforced JSON output when it is explicitly advertised.
/// Other models can still follow the schema prompt and existing JSON parser.
pub fn supports_json(name: &str) -> Result<bool> {
    if provider(name) != Some(Provider::OpenRouter) {
        return Ok(false);
    }
    let catalog = catalog(Provider::OpenRouter)?;
    validate_catalog(Provider::OpenRouter, &catalog, name)?;
    Ok(entries(&catalog)?.iter().any(|model| {
        model["id"].as_str() == model_id(name)
            && contains(&model["supported_parameters"], "response_format")
    }))
}

pub fn validate_catalog(provider: Provider, catalog: &Value, name: &str) -> Result<()> {
    ensure!(
        self::provider(name) == Some(provider),
        "Cloud model provider does not match its catalog"
    );
    let id = model_id(name).context("Cloud model ID is missing")?;
    ensure!(!id.is_empty(), "Cloud model ID is missing");
    let matches: Vec<&Value> = entries(catalog)?
        .iter()
        .filter(|entry| entry["id"] == id)
        .collect();
    if matches.len() != 1 || !free_chat_model(provider, matches[0]) {
        // A preview ending is a provider condition, not a model reasoning
        // failure. Retain work and wait for availability or a human selection.
        return Err(crate::provider::Unavailable {
            reason: "Selected free cloud model is unavailable, no longer free, or incompatible; choose another listed free model",
            retry_after: None,
        }.into());
    }
    Ok(())
}

fn contains(values: &Value, value: &str) -> bool {
    values
        .as_array()
        .is_some_and(|values| values.iter().any(|entry| entry == value))
}

fn free_chat_model(provider: Provider, model: &Value) -> bool {
    let Some(id) = model["id"].as_str() else {
        return false;
    };
    if id.is_empty() || id.chars().any(char::is_whitespace) || model["status"] == "deprecated" {
        return false;
    }
    match provider {
        Provider::Zen => {
            (id.ends_with("-free") || id == "big-pickle")
                && model["tool_call"] == true
                && contains(&model["modalities"]["input"], "text")
                && contains(&model["modalities"]["output"], "text")
                && model["provider"]["npm"] == "@ai-sdk/openai-compatible"
                && free_prices(&model["cost"], "input", "output")
        }
        Provider::OpenRouter => {
            !id.starts_with("openrouter/")
                && contains(&model["supported_parameters"], "tools")
                && contains(&model["architecture"]["input_modalities"], "text")
                && contains(&model["architecture"]["output_modalities"], "text")
                && free_prices(&model["pricing"], "prompt", "completion")
        }
    }
}

fn free_prices(prices: &Value, input: &str, output: &str) -> bool {
    let Some(prices) = prices.as_object() else {
        return false;
    };
    prices.get(input).is_some_and(zero_price)
        && prices.get(output).is_some_and(zero_price)
        && prices.iter().all(|(name, value)| {
            if name == "overrides" {
                value.as_array().is_some_and(|overrides| {
                    overrides.iter().all(|tier| {
                        tier.as_object().is_some_and(|tier| {
                            tier.iter().all(|(name, value)| {
                                matches!(name.as_str(), "min_prompt_tokens" | "max_prompt_tokens")
                                    || zero_price(value)
                            })
                        })
                    })
                })
            } else {
                zero_price(value)
            }
        })
}

fn zero_price(value: &Value) -> bool {
    if let Some(number) = value.as_number() {
        // serde_json can already round an unquoted 1e-999 to floating zero.
        // Integer zero is exact; ambiguous floating zero fails closed. Zen's
        // official costs use integer zero and OpenRouter uses decimal strings.
        return number.as_u64() == Some(0) || number.as_i64() == Some(0);
    }
    let Some(value) = value.as_str() else {
        return false;
    };
    // Do not use floating-point parsing: a small nonzero decimal such as
    // "1e-999" can underflow to zero and must never qualify as free.
    let value = value.trim();
    let value = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value, None), |(m, e)| (m, Some(e)));
    if exponent.is_some_and(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return false;
    }
    let mut digits = 0;
    let mut points = 0;
    for byte in mantissa.bytes() {
        match byte {
            b'0' => digits += 1,
            b'.' => points += 1,
            _ => return false,
        }
    }
    digits > 0 && points <= 1
}

#[cfg(test)]
pub(crate) fn seed_catalog(provider: Provider, catalog: &Value) -> Result<()> {
    store_catalog(provider, catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn router(id: &str, pricing: Value) -> Value {
        json!({"id":id,"pricing":pricing,
            "supported_parameters":["tools","tool_choice","response_format"],
            "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}})
    }

    fn zen(id: &str, input: Value, output: Value) -> Value {
        json!({"id":id,"cost":{"input":input,"output":output,"cache_read":0},
            "tool_call":true,"modalities":{"input":["text"],"output":["text"]}})
    }

    #[test]
    fn cloud_routes_do_not_use_the_local_endpoint_or_groq_credentials() {
        let local = "http://private-host:11434/";
        assert_eq!(
            url(local, "zen/space-bunny-free"),
            format!("{ZEN_ENDPOINT}/chat/completions")
        );
        assert_eq!(
            url(local, "openrouter/stealth/space-bunny-alpha"),
            format!("{OPENROUTER_ENDPOINT}/chat/completions")
        );
        assert_eq!(
            url(local, "groq/fixture"),
            format!("{}/chat/completions", crate::groq::ENDPOINT)
        );
        assert_eq!(
            url(local, "local-model"),
            "http://private-host:11434/api/chat"
        );
        assert_eq!(
            model_id("openrouter/stealth/space-bunny-alpha"),
            Some("stealth/space-bunny-alpha")
        );
        assert_eq!(model_id("local-model"), None);
        assert_eq!(
            key("zen/space-bunny-free").unwrap().as_deref(),
            Some("public")
        );
        assert_eq!(key("local-model").unwrap(), None);
        assert_eq!(label(local, "zen/space-bunny-free"), "OpenCode Zen");
        assert_eq!(label(local, "local-model"), local);
        assert!(!supports_json("zen/space-bunny-free").unwrap());
    }

    #[test]
    fn openrouter_catalog_requires_zero_costs_tools_and_text() {
        let free = router(
            "stealth/space-bunny-alpha",
            json!({"prompt":"0","completion":"0"}),
        );
        let mut text_only = router("lab/no-tools:free", json!({"prompt":"0","completion":"0"}));
        text_only["supported_parameters"] = json!(["max_tokens"]);
        let mut image_only = router("lab/image:free", json!({"prompt":"0","completion":"0"}));
        image_only["architecture"]["output_modalities"] = json!(["image"]);
        let catalog = json!({"data":[free,text_only,image_only,
            router("lab/paid:free",json!({"prompt":"0.0000001","completion":"0"})),
            router("lab/cache-charge",json!({"prompt":"0","completion":"0","input_cache_read":"0.001"})),
            router("lab/request-charge",json!({"prompt":"0","completion":"0","request":"0.1"})),
            router("lab/unknown-price",json!({"prompt":"0"})),
            router("openrouter/free",json!({"prompt":"0","completion":"0"}))
        ]});
        assert_eq!(
            models_from_catalog(Provider::OpenRouter, &catalog).unwrap(),
            vec!["openrouter/stealth/space-bunny-alpha"]
        );
        assert!(
            validate_catalog(
                Provider::OpenRouter,
                &catalog,
                "openrouter/stealth/space-bunny-alpha"
            )
            .is_ok()
        );
        assert!(
            validate_catalog(Provider::OpenRouter, &catalog, "openrouter/lab/paid:free").is_err()
        );
        assert!(
            validate_catalog(
                Provider::Zen,
                &catalog,
                "openrouter/stealth/space-bunny-alpha"
            )
            .is_err()
        );
    }

    #[test]
    fn zen_catalog_intersects_live_ids_pricing_and_chat_protocol() {
        let mut responses = zen("muse-spark-1.3-contributor-free", json!(0), json!(0));
        responses["provider"] = json!({"npm":"@ai-sdk/openai"});
        let mut messages = zen("qwen3.6-plus-free", json!(0), json!(0));
        messages["provider"] = json!({"npm":"@ai-sdk/anthropic"});
        let mut decision = zen("jev-free", json!(0), json!(0));
        decision["tool_call"] = json!(false);
        let metadata = json!({"opencode":{"npm":"@ai-sdk/openai-compatible","models":{
            "space-bunny-free":zen("space-bunny-free",json!(0),json!(0)),
            "big-pickle":zen("big-pickle",json!(0),json!(0)),
            "old-model-free":zen("old-model-free",json!(0),json!(0)),
            "formerly-free":zen("formerly-free",json!(0.01),json!(0)),
            "muse-spark-1.3-contributor-free":responses,
            "qwen3.6-plus-free":messages,
            "jev-free":decision
        }}});
        let available = json!({"data":[{"id":"space-bunny-free"},{"id":"big-pickle"},{"id":"formerly-free"},{"id":"muse-spark-1.3-contributor-free"},{"id":"qwen3.6-plus-free"},{"id":"jev-free"},{"id":"unknown-free"}]});
        let catalog = merge_zen_catalog(&available, &metadata).unwrap();
        assert_eq!(
            models_from_catalog(Provider::Zen, &catalog).unwrap(),
            vec!["zen/big-pickle", "zen/space-bunny-free"]
        );
        assert!(validate_catalog(Provider::Zen, &catalog, "zen/space-bunny-free").is_ok());
        assert!(validate_catalog(Provider::Zen, &catalog, "zen/old-model-free").is_err());
        assert!(validate_catalog(Provider::Zen, &catalog, "zen/formerly-free").is_err());
        assert!(validate_catalog(Provider::Zen, &available, "zen/space-bunny-free").is_err());
    }

    #[test]
    fn unknown_ambiguous_and_newly_paid_prices_fail_closed() {
        let free = router("lab/fixture", json!({"prompt":"0","completion":"0"}));
        let mut catalog = json!({"data":[free.clone()]});
        assert!(validate_catalog(Provider::OpenRouter, &catalog, "openrouter/lab/fixture").is_ok());
        catalog["data"][0]["pricing"]["completion"] = json!("0.001");
        assert!(
            validate_catalog(Provider::OpenRouter, &catalog, "openrouter/lab/fixture").is_err()
        );
        assert!(
            validate_catalog(
                Provider::OpenRouter,
                &json!({"data":[free.clone(),free]}),
                "openrouter/lab/fixture"
            )
            .is_err()
        );
        assert!(
            validate_catalog(
                Provider::OpenRouter,
                &json!({"data":[]}),
                "openrouter/lab/fixture"
            )
            .is_err()
        );
        assert!(
            validate_catalog(Provider::OpenRouter, &json!({}), "openrouter/lab/fixture").is_err()
        );
        assert!(validate_catalog(Provider::OpenRouter, &catalog, "openrouter/").is_err());
        assert!(!free_prices(
            &json!({"prompt":"0","completion":"0","overrides":[{"min_prompt_tokens":1000,"completion":"0.1"}]}),
            "prompt",
            "completion"
        ));
        assert!(free_prices(
            &json!({"prompt":"0","completion":"0","overrides":[{"min_prompt_tokens":1000,"prompt":"0","completion":"0"}]}),
            "prompt",
            "completion"
        ));
    }

    #[test]
    fn exact_zero_prices_do_not_accept_underflow_or_unknown_notation() {
        for value in [
            json!(0),
            json!("0"),
            json!("0.000"),
            json!("-0"),
            json!("0e-1000"),
        ] {
            assert!(zero_price(&value), "{value}");
        }
        for value in [
            json!(0.0),
            json!("1e-999"),
            json!("NaN"),
            json!("free"),
            json!(null),
            json!("--0"),
            json!("0e--1"),
            json!("0.0.0"),
            json!(""),
            json!(-1),
            serde_json::from_str::<Value>("1e-999").unwrap(),
        ] {
            assert!(!zero_price(&value), "{value}");
        }
    }

    #[test]
    fn public_catalog_rate_limits_and_disconnections_use_provider_waits() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for status in ["429 Too Many Requests", "503 Service Unavailable"] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    request.read_line(&mut line).unwrap();
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(socket,"HTTP/1.1 {status}\r\nRetry-After: 37\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        for _ in 0..2 {
            let error = fetch_json(&client, &url).unwrap_err();
            let unavailable = error
                .downcast_ref::<crate::provider::Unavailable>()
                .unwrap();
            assert_eq!(unavailable.retry_after, Some(Duration::from_secs(37)));
        }
        server.join().unwrap();
        assert!(
            fetch_json(&client, &url)
                .unwrap_err()
                .downcast_ref::<crate::provider::Unavailable>()
                .is_some()
        );
    }
}
