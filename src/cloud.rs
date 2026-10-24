//! Free-only OpenAI-compatible cloud routes. Public metadata is briefly cached;
//! requests must also use Zen's anonymous credential or OpenRouter's zero-price
//! routing cap so a promotion ending cannot charge an account between refreshes.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceStatus {
    Free,
    Paid,
    Unknown,
}

/// Live provider metadata, retaining unavailable/unknown entries so a selector
/// can explain why an old model is no longer a valid free choice.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelEntry {
    /// Stored model identity, including Chuggin's provider prefix.
    pub id: String,
    pub name: String,
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub tools: Option<bool>,
    pub pricing: PriceStatus,
    pub compatible: bool,
    pub unavailable_reason: Option<String>,
}

impl ModelEntry {
    pub fn selectable(&self) -> bool {
        self.compatible && self.pricing == PriceStatus::Free
    }

    pub fn detail(&self) -> &str {
        self.unavailable_reason.as_deref().unwrap_or(
            "Currently listed at zero cost; provider access, capacity and rate limits can change.",
        )
    }
}

pub fn provider(name: &str) -> Option<Provider> {
    [Provider::Zen, Provider::OpenRouter]
        .into_iter()
        .find(|provider| name.starts_with(provider.prefix()))
}

pub fn model_id(name: &str) -> Option<&str> {
    crate::chatgpt::model_id(name)
        .or_else(|| provider(name).and_then(|provider| name.strip_prefix(provider.prefix())))
}

pub fn url(ollama: &str, name: &str) -> String {
    if crate::chatgpt::model_id(name).is_some() {
        return "https://api.openai.com/v1/responses".into();
    }
    provider(name).map_or_else(
        || crate::groq::url(ollama, name),
        |provider| format!("{}/chat/completions", provider.endpoint()),
    )
}

pub fn label<'a>(ollama: &'a str, name: &str) -> &'a str {
    match provider(name) {
        Some(provider) => provider.label(),
        None if crate::chatgpt::model_id(name).is_some() => "OpenAI · ChatGPT plan",
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

fn cached_catalog(provider: Provider, cache: &Mutex<CatalogCache>) -> Option<Value> {
    cache
        .lock()
        .ok()?
        .get(&provider)
        .and_then(|(at, value)| (at.elapsed() < CATALOG_TTL).then(|| value.clone()))
}

fn store_catalog(provider: Provider, value: &Value, cache: &Mutex<CatalogCache>) -> Result<()> {
    entries(value)?;
    cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Cloud model catalog cache is unavailable"))?
        .insert(provider, (Instant::now(), value.clone()));
    Ok(())
}

/// Get current public metadata. Expired metadata never substitutes for a failed
/// refresh. No credentials or project content are sent by catalog requests.
pub fn catalog(provider: Provider) -> Result<Value> {
    resolve_catalog(provider, false, cache(), || fetch_catalog(provider))
}

/// Explicit user refresh always contacts the public APIs, including inside the
/// normal TTL. A failed refresh retires the prior cache instead of showing stale
/// free pricing as current availability.
pub fn refresh_catalog(provider: Provider) -> Result<Value> {
    resolve_catalog(provider, true, cache(), || fetch_catalog(provider))
}

fn resolve_catalog(
    provider: Provider,
    refresh: bool,
    cache: &Mutex<CatalogCache>,
    fetch: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    if refresh {
        cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Cloud model catalog cache is unavailable"))?
            .remove(&provider);
    } else if let Some(value) = cached_catalog(provider, cache) {
        return Ok(value);
    }
    let value = fetch()?;
    store_catalog(provider, &value, cache)?;
    Ok(value)
}

fn fetch_catalog(provider: Provider) -> Result<Value> {
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
    Ok(value)
}

fn fetch_json(client: &reqwest::blocking::Client, url: &str) -> Result<Value> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
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
        let mut model = models
            .get(id)
            .filter(|model| model["id"] == id)
            .cloned()
            .unwrap_or_else(|| entry.clone());
        // Preserve newly available IDs even if the descriptive database has
        // not caught up: their pricing/capabilities stay unknown. If Zen starts
        // returning richer authoritative metadata, prefer it over that database.
        if let (Some(model), Some(entry)) = (model.as_object_mut(), entry.as_object()) {
            model.extend(entry.clone());
            // An authoritative pricing payload must not be ignored in favor
            // of older database costs. Unrecognized fields fail closed below.
            if entry.contains_key("pricing") && !entry.contains_key("cost") {
                model.insert("cost".into(), entry["pricing"].clone());
            }
        }
        if model["provider"]["npm"].as_str().is_none() {
            model["provider"] = json!({"npm":default_sdk});
        }
        merged.push(model);
    }
    Ok(json!({"data":merged}))
}

pub fn model_entries(provider: Provider, refresh: bool) -> Result<Vec<ModelEntry>> {
    let catalog = if refresh {
        refresh_catalog(provider)?
    } else {
        catalog(provider)?
    };
    model_entries_from_catalog(provider, &catalog)
}

pub fn model_entries_from_catalog(provider: Provider, catalog: &Value) -> Result<Vec<ModelEntry>> {
    let entries = entries(catalog)?;
    let mut counts = BTreeMap::new();
    for entry in entries {
        if let Some(id) = entry["id"].as_str() {
            *counts.entry(id).or_insert(0usize) += 1;
        }
    }
    let mut models: Vec<_> = entries.iter().filter_map(|model| {
        let id = model["id"].as_str()?;
        if !valid_id(id) { return None; }
        let prices = match provider {
            Provider::Zen => &model["cost"],
            Provider::OpenRouter => &model["pricing"],
        };
        let (input, output) = match provider {
            Provider::Zen => ("input", "output"),
            Provider::OpenRouter => ("prompt", "completion"),
        };
        let pricing = if free_prices(prices, input, output) {
            PriceStatus::Free
        } else if known_charge(prices) {
            PriceStatus::Paid
        } else {
            PriceStatus::Unknown
        };
        let tools = match provider {
            Provider::Zen => model["tool_call"].as_bool(),
            Provider::OpenRouter => model["supported_parameters"].as_array().map(|_|contains(&model["supported_parameters"], "tools")),
        };
        let duplicate = counts.get(id) != Some(&1);
        let compatible = !duplicate && chat_model(provider, model);
        let unavailable_reason = if duplicate {
            Some("Provider returned conflicting duplicate entries; refresh before choosing this model.".into())
        } else if model["status"] == "deprecated" {
            Some("Provider metadata marks this model as deprecated.".into())
        } else if pricing == PriceStatus::Paid {
            Some("Provider metadata includes a charge; Chuggin's free connection does not select paid models.".into())
        } else if pricing == PriceStatus::Unknown {
            Some("Complete zero-cost pricing is not published or cannot be verified.".into())
        } else if tools != Some(true) {
            Some("Tool calling is not advertised by this provider.".into())
        } else if !compatible {
            Some("This model does not advertise the text Chat Completions protocol supported by this connection.".into())
        } else {
            None
        };
        let context = match provider {
            Provider::Zen => model["limit"]["context"].as_u64(),
            Provider::OpenRouter => model["context_length"].as_u64().or_else(||model["top_provider"]["context_length"].as_u64()),
        };
        let output = match provider {
            Provider::Zen => model["limit"]["output"].as_u64(),
            Provider::OpenRouter => model["top_provider"]["max_completion_tokens"].as_u64(),
        };
        let name: String = model["name"].as_str().unwrap_or(id).chars().filter(|character| !character.is_control()).collect();
        Some(ModelEntry {
            id:format!("{}{id}",provider.prefix()),
            name:if name.trim().is_empty() { id.into() } else {name.trim().into()},
            context_tokens:context.and_then(|value|u32::try_from(value).ok()).filter(|value|*value>0),
            output_tokens:output.and_then(|value|u32::try_from(value).ok()).filter(|value|*value>0),
            tools,pricing,compatible,unavailable_reason,
        })
    }).collect();
    models.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(models)
}

/// Pure catalog filtering, also used by discovery and request validation.
#[cfg(test)]
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
    chat_model(provider, model)
        && match provider {
            Provider::Zen => free_prices(&model["cost"], "input", "output"),
            Provider::OpenRouter => free_prices(&model["pricing"], "prompt", "completion"),
        }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

fn chat_model(provider: Provider, model: &Value) -> bool {
    let Some(id) = model["id"].as_str() else {
        return false;
    };
    if !valid_id(id) || model["status"] == "deprecated" {
        return false;
    }
    match provider {
        Provider::Zen => {
            model["tool_call"] == true
                && contains(&model["modalities"]["input"], "text")
                && contains(&model["modalities"]["output"], "text")
                && model["provider"]["npm"] == "@ai-sdk/openai-compatible"
        }
        Provider::OpenRouter => {
            // Router aliases do not identify the concrete model or its limits.
            // Offer the individual live models instead.
            !id.starts_with("openrouter/")
                && contains(&model["supported_parameters"], "tools")
                && contains(&model["architecture"]["input_modalities"], "text")
                && contains(&model["architecture"]["output_modalities"], "text")
        }
    }
}

fn known_charge(prices: &Value) -> bool {
    let Some(prices) = prices.as_object() else {
        return false;
    };
    prices.iter().any(|(name, value)| {
        if name == "overrides" {
            value.as_array().is_some_and(|tiers| {
                tiers.iter().any(|tier| {
                    tier.as_object().is_some_and(|tier| {
                        tier.iter().any(|(name, value)| {
                            !matches!(name.as_str(), "min_prompt_tokens" | "max_prompt_tokens")
                                && positive_price(value)
                        })
                    })
                })
            })
        } else {
            positive_price(value)
        }
    })
}

fn positive_price(value: &Value) -> bool {
    value.as_f64().is_some_and(|number| number > 0.0)
        || value
            .as_str()
            .and_then(decimal_parts)
            .is_some_and(|(negative, nonzero)| !negative && nonzero)
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
    value
        .as_str()
        .and_then(decimal_parts)
        .is_some_and(|(_, nonzero)| !nonzero)
}

fn decimal_parts(value: &str) -> Option<(bool, bool)> {
    // Do not use floating-point parsing: a small nonzero decimal such as
    // "1e-999" can underflow to zero and must never qualify as free.
    let value = value.trim();
    let negative = value.starts_with('-');
    let value = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value, None), |(m, e)| (m, Some(e)));
    if exponent.is_some_and(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let mut digits = 0;
    let mut points = 0;
    let mut nonzero = false;
    for byte in mantissa.bytes() {
        match byte {
            b'0' => digits += 1,
            b'1'..=b'9' => {
                digits += 1;
                nonzero = true;
            }
            b'.' => points += 1,
            _ => return None,
        }
    }
    (digits > 0 && points <= 1).then_some((negative, nonzero))
}

#[cfg(test)]
pub(crate) fn seed_catalog(provider: Provider, catalog: &Value) -> Result<()> {
    store_catalog(provider, catalog, cache())
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
    fn zen_discovery_uses_live_prices_not_free_names_or_stale_descriptors() {
        let mut current = zen("new-preview", json!(0), json!(0));
        current["name"] = json!("New Preview");
        current["limit"] = json!({"context":1048576,"output":65536});
        let metadata = json!({"opencode":{"npm":"@ai-sdk/openai-compatible","models":{
            "new-preview":current,
            "old-free":zen("old-free",json!(0),json!(0)),
            "new-pricing":zen("new-pricing",json!(0),json!(0))
        }}});
        let available = json!({"data":[
            {"id":"new-preview"},
            {"id":"old-free","cost":{"input":"0.1","output":0}},
            {"id":"new-pricing","pricing":{"prompt":"0.1","completion":"0"}},
            {"id":"unpublished-free"}
        ]});
        let catalog = merge_zen_catalog(&available, &metadata).unwrap();
        assert_eq!(
            models_from_catalog(Provider::Zen, &catalog).unwrap(),
            ["zen/new-preview"]
        );
        let entries = model_entries_from_catalog(Provider::Zen, &catalog).unwrap();
        let preview = entries
            .iter()
            .find(|entry| entry.id == "zen/new-preview")
            .unwrap();
        assert_eq!(preview.name, "New Preview");
        assert_eq!(preview.context_tokens, Some(1048576));
        assert_eq!(preview.output_tokens, Some(65536));
        assert!(preview.selectable());
        assert!(preview.detail().contains("rate limits can change"));
        for id in ["zen/old-free", "zen/new-pricing"] {
            let entry = entries.iter().find(|entry| entry.id == id).unwrap();
            assert_eq!(entry.pricing, PriceStatus::Paid);
            assert!(!entry.selectable());
            assert!(entry.detail().contains("charge"));
        }
        let unknown = entries
            .iter()
            .find(|entry| entry.id == "zen/unpublished-free")
            .unwrap();
        assert_eq!(unknown.pricing, PriceStatus::Unknown);
        assert_eq!(unknown.tools, None);
        assert!(!unknown.selectable());
        assert!(unknown.detail().contains("cannot be verified"));
    }

    #[test]
    fn typed_catalog_preserves_capability_and_pricing_uncertainty() {
        let mut supported = router("lab/current", json!({"prompt":"0","completion":"0"}));
        supported["name"] = json!("Current\u{1b} Model");
        supported["context_length"] = json!(200000);
        supported["top_provider"] = json!({"max_completion_tokens":32768});
        let mut missing_tools = router("lab/unknown-tools", json!({"prompt":"0","completion":"0"}));
        missing_tools
            .as_object_mut()
            .unwrap()
            .remove("supported_parameters");
        let mut no_tools = router("lab/no-tools", json!({"prompt":"0","completion":"0"}));
        no_tools["supported_parameters"] = json!([]);
        let mut image = router("lab/image", json!({"prompt":"0","completion":"0"}));
        image["architecture"]["output_modalities"] = json!(["image"]);
        let duplicate = router("lab/duplicate", json!({"prompt":"0","completion":"0"}));
        let entries = model_entries_from_catalog(
            Provider::OpenRouter,
            &json!({"data":[
                supported,missing_tools,no_tools,image,duplicate.clone(),duplicate,
                router("lab/unknown-price",json!({"prompt":"0","completion":"free"})),
                router("lab/tiny-charge",json!({"prompt":"1e-999","completion":"0"})),
                router("lab/invalid\nidentity",json!({"prompt":"0","completion":"0"}))
            ]}),
        )
        .unwrap();
        let selected: Vec<_> = entries.iter().filter(|entry| entry.selectable()).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "openrouter/lab/current");
        assert_eq!(selected[0].name, "Current Model");
        assert_eq!(selected[0].context_tokens, Some(200000));
        assert_eq!(selected[0].output_tokens, Some(32768));
        assert!(entries.iter().all(|entry| !entry.id.contains('\n')));
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id.ends_with("unknown-tools"))
                .unwrap()
                .tools,
            None
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id.ends_with("no-tools"))
                .unwrap()
                .tools,
            Some(false)
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id.ends_with("unknown-price"))
                .unwrap()
                .pricing,
            PriceStatus::Unknown
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id.ends_with("tiny-charge"))
                .unwrap()
                .pricing,
            PriceStatus::Paid
        );
        for entry in entries
            .iter()
            .filter(|entry| entry.id.ends_with("duplicate"))
        {
            assert!(!entry.selectable());
            assert!(entry.detail().contains("duplicate"));
        }
        assert!(!known_charge(
            &json!({"prompt":"0","completion":"0","overrides":[{"min_prompt_tokens":1000,"prompt":"0","completion":"0"}]})
        ));
        assert!(known_charge(
            &json!({"prompt":"0","completion":"0","overrides":[{"min_prompt_tokens":1000,"completion":"0.1"}]})
        ));
        assert!(!known_charge(&json!({"prompt":"-1","completion":"NaN"})));
    }

    #[test]
    fn explicit_refresh_bypasses_and_retires_cached_free_metadata() {
        let cache = Mutex::new(CatalogCache::new());
        let free = json!({"data":[router("lab/current",json!({"prompt":"0","completion":"0"}))]});
        let paid = json!({"data":[router("lab/current",json!({"prompt":"0.1","completion":"0"}))]});
        assert_eq!(
            resolve_catalog(Provider::OpenRouter, false, &cache, || Ok(free.clone())).unwrap(),
            free
        );
        assert_eq!(
            resolve_catalog(Provider::OpenRouter, false, &cache, || panic!(
                "TTL cache should avoid fetch"
            ))
            .unwrap(),
            free
        );
        assert_eq!(
            resolve_catalog(Provider::OpenRouter, true, &cache, || Ok(paid.clone())).unwrap(),
            paid
        );
        assert!(
            validate_catalog(
                Provider::OpenRouter,
                &cached_catalog(Provider::OpenRouter, &cache).unwrap(),
                "openrouter/lab/current"
            )
            .is_err()
        );
        assert!(
            resolve_catalog(Provider::OpenRouter, true, &cache, || anyhow::bail!(
                "temporary metadata failure"
            ))
            .is_err()
        );
        assert!(cached_catalog(Provider::OpenRouter, &cache).is_none());
        assert_eq!(
            resolve_catalog(Provider::OpenRouter, false, &cache, || Ok(free.clone())).unwrap(),
            free
        );
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
