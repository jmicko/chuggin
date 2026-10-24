//! Provider-specific thinking controls. A default selection never adds an
//! override or requires metadata discovery before inference.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum Setting {
    #[default]
    Default,
    On,
    Off,
    Effort(String),
    TokenBudget(u32),
}
impl Setting {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
    pub fn label(&self) -> String {
        match self {
            Self::Default => "Model default".into(),
            Self::On => "Thinking on".into(),
            Self::Off => "Thinking off".into(),
            Self::Effort(level) => format!("{level} effort"),
            Self::TokenBudget(tokens) => format!("{tokens} reasoning tokens"),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if let Self::Effort(level) = self {
            ensure!(
                !level.is_empty()
                    && level.len() <= 128
                    && level.trim() == level
                    && !level.chars().any(char::is_control),
                "Thinking effort must be a short model-defined level without control characters"
            );
        }
        if let Self::TokenBudget(tokens) = self {
            ensure!(*tokens > 0, "Reasoning token budget must be positive");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub source: String,
    pub detail: String,
    pub options: Vec<Setting>,
    pub supports_token_budget: bool,
    pub default: Option<Value>,
}
impl Capabilities {
    fn unknown(source: &str, detail: &str) -> Self {
        Self {
            source: source.into(),
            detail: detail.into(),
            options: vec![Setting::Default],
            supports_token_budget: false,
            default: None,
        }
    }
    pub fn supports(&self, setting: &Setting) -> bool {
        setting.is_default()
            || self.options.contains(setting)
            || (self.supports_token_budget && matches!(setting, Setting::TokenBudget(_)))
    }
    pub fn validate_selection(&self, setting: &Setting) -> Result<()> {
        setting.validate()?;
        ensure!(
            self.supports(setting),
            "{} is not advertised for this model. {} Choose Model default or another supported setting. Source: {}",
            setting.label(),
            self.detail,
            self.source
        );
        Ok(())
    }
}

/// Metadata only: no chat requests, prompts, or inference probes are sent.
pub fn discover(ollama_url: &str, name: &str) -> Result<Capabilities> {
    if crate::chatgpt::model_id(name).is_some() {
        return Ok(chatgpt_capabilities(&crate::chatgpt::model_info(name)?));
    }
    if let Some(id) = crate::groq::model_id(name) {
        return Ok(groq_capabilities(id));
    }
    if let Some(provider) = crate::cloud::provider(name) {
        if provider == crate::cloud::Provider::Zen {
            return Ok(Capabilities::unknown(
                "OpenCode Zen metadata",
                "Zen does not publish accepted thinking controls. The model may think using its default behavior; adjustable levels are unverified.",
            ));
        }
        let catalog = crate::cloud::catalog(provider)?;
        let entry = catalog["data"]
            .as_array()
            .and_then(|models| {
                models
                    .iter()
                    .find(|model| model["id"].as_str() == crate::cloud::model_id(name))
            })
            .context("Selected model is missing from OpenRouter's metadata")?;
        return Ok(openrouter_capabilities(entry));
    }
    let base = ollama_url
        .trim_end_matches('/')
        .strip_suffix("/api/chat")
        .unwrap_or_else(|| ollama_url.trim_end_matches('/'));
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{base}/api/show"))
        .json(&json!({"model":name}))
        .timeout(Duration::from_secs(15))
        .send()
        .context("Could not discover this model's thinking controls from Ollama")?;
    ensure!(
        response.status().is_success(),
        "Ollama thinking discovery returned HTTP {}",
        response.status()
    );
    let metadata: Value = response
        .json()
        .context("Ollama thinking metadata is invalid")?;
    Ok(ollama_capabilities(&metadata))
}

pub fn chatgpt_capabilities(metadata: &Value) -> Capabilities {
    let mut capabilities = Capabilities::unknown(
        "Signed-in ChatGPT account model catalog",
        "Only thinking efforts advertised by this account model can be selected; Model default preserves the provider's normal behavior.",
    );
    let levels = metadata["supported_reasoning_efforts"]
        .as_array()
        .or_else(|| metadata["supported_reasoning_levels"].as_array());
    if let Some(levels) = levels {
        for entry in levels {
            let level = entry
                .as_str()
                .or_else(|| entry["reasoning_effort"].as_str())
                .or_else(|| entry["effort"].as_str());
            if let Some(level) = level {
                let setting = Setting::Effort(level.into());
                if setting.validate().is_ok() && !capabilities.options.contains(&setting) {
                    capabilities.options.push(setting);
                }
                if level == "none" && !capabilities.options.contains(&Setting::Off) {
                    capabilities.options.push(Setting::Off);
                }
            }
        }
    }
    capabilities.default = metadata
        .get("default_reasoning_effort")
        .filter(|value| value.is_string())
        .cloned();
    capabilities
}

pub fn ollama_capabilities(metadata: &Value) -> Capabilities {
    let mut capabilities = Capabilities::unknown(
        "Ollama /api/show",
        "The server does not advertise supported thinking values. Model default preserves its normal behavior; update Ollama to discover controls.",
    );
    let Some(values) = metadata["thinking"]["values"].as_array() else {
        return capabilities;
    };
    for value in values {
        let setting = match value {
            Value::Bool(true) => Some(Setting::On),
            Value::Bool(false) => Some(Setting::Off),
            Value::String(level) => Some(Setting::Effort(level.clone())),
            _ => None,
        };
        if let Some(setting) = setting
            && setting.validate().is_ok()
            && !capabilities.options.contains(&setting)
        {
            capabilities.options.push(setting);
        }
    }
    capabilities.default = metadata["thinking"].get("default").cloned();
    capabilities.detail = if values == &[json!(false)] {
        "Ollama reports that this model does not support thinking.".into()
    } else {
        "Options are the exact thinking values advertised by this Ollama model.".into()
    };
    capabilities
}

pub fn openrouter_capabilities(metadata: &Value) -> Capabilities {
    let mut capabilities = Capabilities::unknown(
        "OpenRouter /api/v1/models",
        "This model does not expose per-model thinking controls. Model default preserves its normal behavior.",
    );
    let Some(reasoning) = metadata.get("reasoning").filter(|v| v.is_object()) else {
        return capabilities;
    };
    let mandatory = reasoning["mandatory"] == true;
    capabilities.options.push(Setting::On);
    if !mandatory {
        capabilities.options.push(Setting::Off);
    }
    let efforts: Vec<String> = match reasoning.get("supported_efforts") {
        Some(Value::Array(levels)) => levels
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::Null) => ["max", "xhigh", "high", "medium", "low", "minimal", "none"]
            .map(str::to_owned)
            .to_vec(),
        _ => Vec::new(),
    };
    for level in efforts {
        if mandatory && level == "none" {
            continue;
        }
        let setting = Setting::Effort(level);
        if setting.validate().is_ok() && !capabilities.options.contains(&setting) {
            capabilities.options.push(setting);
        }
    }
    capabilities.supports_token_budget = reasoning["supports_max_tokens"] == true;
    capabilities.default = reasoning
        .get("default_effort")
        .cloned()
        .or_else(|| reasoning.get("default_enabled").cloned());
    capabilities.detail = if mandatory {
        "OpenRouter reports that thinking is mandatory for this model; it cannot be disabled."
            .into()
    } else {
        "Controls come from this model's OpenRouter reasoning metadata.".into()
    };
    capabilities
}

pub fn groq_capabilities(id: &str) -> Capabilities {
    let mut capabilities = Capabilities::unknown(
        "Groq documented model controls (https://console.groq.com/docs/reasoning)",
        "Groq's model-list API does not publish thinking options, and this model has no documented adjustment mapping in Chuggin.",
    );
    match id {
        "openai/gpt-oss-20b" | "openai/gpt-oss-120b" => {
            capabilities
                .options
                .extend(["low", "medium", "high"].map(|level| Setting::Effort(level.into())));
            capabilities.default = Some(json!("medium"));
            capabilities.detail = "Groq documents low, medium, and high effort for this model. Thinking cannot be disabled.".into();
        }
        "qwen/qwen3.8-27b" => {
            capabilities.options.extend([Setting::On, Setting::Off]);
            capabilities
                .options
                .extend(["low", "medium", "high"].map(|level| Setting::Effort(level.into())));
            capabilities.detail =
                "Groq documents on/off and low, medium, high effort for this model.".into();
        }
        "qwen/qwen3.6-27b" | "qwen/qwen3-32b" => {
            capabilities.options.extend([Setting::On, Setting::Off]);
            capabilities.detail =
                "Groq documents on/off controls for this model, without adjustable effort levels."
                    .into();
        }
        _ => {}
    }
    capabilities
}

/// Apply only a supported explicit override to the provider's existing request.
pub fn apply(
    body: &mut Value,
    name: &str,
    setting: &Setting,
    capabilities: Option<&Capabilities>,
) -> Result<()> {
    setting.validate()?;
    if setting.is_default() {
        return Ok(());
    }
    capabilities
        .context("Thinking capabilities must be discovered for an explicit setting")?
        .validate_selection(setting)?;
    if crate::chatgpt::model_id(name).is_some() {
        body["reasoning"] = match setting {
            Setting::Off => json!({"effort":"none"}),
            Setting::Effort(level) => json!({"effort":level}),
            _ => unreachable!("ChatGPT metadata rejects unadvertised thinking settings"),
        };
    } else if crate::groq::model_id(name).is_some() {
        body["reasoning_effort"] = match setting {
            Setting::On => json!("default"),
            Setting::Off => json!("none"),
            Setting::Effort(level) => json!(level),
            _ => unreachable!("Groq capabilities reject budgets and default was handled"),
        };
        if crate::groq::model_id(name).is_some_and(|id| id.starts_with("qwen/qwen3")) {
            // Qwen's raw reasoning output is incompatible with tool calling
            // and JSON mode; parsed keeps it in the separate reasoning field.
            body["reasoning_format"] = json!("parsed");
        }
    } else if crate::cloud::provider(name) == Some(crate::cloud::Provider::OpenRouter) {
        body["reasoning"] = match setting {
            Setting::On => json!({"enabled":true}),
            Setting::Off => json!({"enabled":false}),
            Setting::Effort(level) => json!({"effort":level}),
            Setting::TokenBudget(tokens) => json!({"max_tokens":tokens}),
            Setting::Default => unreachable!(),
        };
        body["provider"]["require_parameters"] = json!(true);
    } else {
        ensure!(
            crate::cloud::provider(name).is_none(),
            "This cloud route has no verified thinking override"
        );
        body["think"] = match setting {
            Setting::On => json!(true),
            Setting::Off => json!(false),
            Setting::Effort(level) => json!(level),
            _ => unreachable!("Ollama capabilities reject budgets and default was handled"),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_does_not_override_any_provider_or_require_discovery() {
        for name in [
            "local",
            "groq/openai/gpt-oss-120b",
            "openrouter/test:free",
            "zen/space-bunny-free",
            "chatgpt/account-model",
        ] {
            let mut body = json!({"model":name});
            let original = body.clone();
            apply(&mut body, name, &Setting::Default, None).unwrap();
            assert_eq!(body, original);
        }
    }
    #[test]
    fn chatgpt_only_uses_account_advertised_efforts_in_responses_shape() {
        let capabilities = chatgpt_capabilities(
            &json!({"supported_reasoning_efforts":["low",{"reasoning_effort":"ultra"},"none","\ninvalid"],"default_reasoning_effort":"ultra"}),
        );
        assert!(capabilities.supports(&Setting::Effort("ultra".into())));
        assert!(!capabilities.supports(&Setting::Effort("high".into())));
        assert!(!capabilities.supports(&Setting::On));
        assert!(!capabilities.supports(&Setting::TokenBudget(1024)));
        let mut body = json!({});
        apply(
            &mut body,
            "chatgpt/account-model",
            &Setting::Effort("ultra".into()),
            Some(&capabilities),
        )
        .unwrap();
        assert_eq!(body, json!({"reasoning":{"effort":"ultra"}}));
        apply(
            &mut body,
            "chatgpt/account-model",
            &Setting::Off,
            Some(&capabilities),
        )
        .unwrap();
        assert_eq!(body["reasoning"]["effort"], "none");
        let unknown = chatgpt_capabilities(&json!({}));
        assert_eq!(unknown.options, [Setting::Default]);
    }
    #[test]
    fn ollama_uses_exact_metadata_values_and_rejects_unsupported_names() {
        let caps = ollama_capabilities(
            &json!({"thinking":{"values":[true,false,"low","xhigh"],"default":"xhigh"}}),
        );
        let mut body = json!({});
        apply(
            &mut body,
            "custom-local",
            &Setting::Effort("xhigh".into()),
            Some(&caps),
        )
        .unwrap();
        assert_eq!(body["think"], "xhigh");
        assert!(
            apply(
                &mut body,
                "custom-local",
                &Setting::Effort("medium".into()),
                Some(&caps)
            )
            .is_err()
        );
        assert_eq!(caps.default, Some(json!("xhigh")));
        assert!(!ollama_capabilities(&json!({"capabilities":["thinking"]})).supports(&Setting::On));
    }
    #[test]
    fn mandatory_router_thinking_cannot_be_disabled_and_budgets_are_explicit() {
        let caps = openrouter_capabilities(
            &json!({"reasoning":{"mandatory":true,"supported_efforts":["high","low","none"],"supports_max_tokens":true}}),
        );
        assert!(!caps.supports(&Setting::Off));
        assert!(!caps.supports(&Setting::Effort("none".into())));
        assert!(!caps.supports(&Setting::Effort("medium".into())));
        let mut body = json!({"provider":{"max_price":{"prompt":0}}});
        apply(
            &mut body,
            "openrouter/test:free",
            &Setting::TokenBudget(2000),
            Some(&caps),
        )
        .unwrap();
        assert_eq!(body["reasoning"], json!({"max_tokens":2000}));
        assert_eq!(body["provider"]["max_price"]["prompt"], 0);
        assert_eq!(body["provider"]["require_parameters"], true);
        let unspecified = openrouter_capabilities(&json!({"reasoning":{"supported_efforts":null}}));
        assert!(unspecified.supports(&Setting::Effort("max".into())));
        let no_efforts = openrouter_capabilities(&json!({"reasoning":{"default_enabled":true}}));
        assert!(!no_efforts.supports(&Setting::Effort("high".into())));
    }
    #[test]
    fn groq_mapping_preserves_unsupported_and_unknown_models() {
        let mut body = json!({});
        let caps = groq_capabilities("qwen/qwen3.8-27b");
        apply(
            &mut body,
            "groq/qwen/qwen3.8-27b",
            &Setting::On,
            Some(&caps),
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "default");
        apply(
            &mut body,
            "groq/qwen/qwen3.8-27b",
            &Setting::Off,
            Some(&caps),
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "none");
        let caps = groq_capabilities("openai/gpt-oss-120b");
        assert!(
            apply(
                &mut body,
                "groq/openai/gpt-oss-120b",
                &Setting::Off,
                Some(&caps)
            )
            .is_err()
        );
        let caps = groq_capabilities("future-model");
        assert!(!caps.supports(&Setting::Effort("high".into())));
    }
    #[test]
    fn settings_round_trip_and_validate() {
        for setting in [
            Setting::Default,
            Setting::On,
            Setting::Off,
            Setting::Effort("high".into()),
            Setting::TokenBudget(2048),
        ] {
            let encoded = serde_json::to_value(&setting).unwrap();
            assert_eq!(serde_json::from_value::<Setting>(encoded).unwrap(), setting);
            setting.validate().unwrap();
        }
        assert!(Setting::TokenBudget(0).validate().is_err());
        assert!(Setting::Effort("invalid\nlevel".into()).validate().is_err());
    }
}
