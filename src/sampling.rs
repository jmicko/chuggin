//! Sampling is applied once to the final provider request. Existing projects keep
//! Legacy behavior; ProviderDefault deliberately sends no sampling overrides.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const PARAMETERS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "repeat_penalty",
    "presence_penalty",
    "frequency_penalty",
    "seed",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Values {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}
impl Values {
    pub fn validate(&self) -> Result<()> {
        for (name, value, minimum, maximum) in [
            ("temperature", self.temperature, 0.0, 2.0),
            ("top_p", self.top_p, 0.0, 1.0),
            ("min_p", self.min_p, 0.0, 1.0),
            ("repeat_penalty", self.repeat_penalty, 0.0, 2.0),
            ("presence_penalty", self.presence_penalty, -2.0, 2.0),
            ("frequency_penalty", self.frequency_penalty, -2.0, 2.0),
        ] {
            if let Some(value) = value {
                ensure!(
                    value.is_finite() && (minimum..=maximum).contains(&value),
                    "{name} must be a finite number between {minimum} and {maximum}"
                );
            }
        }
        if let Some(top_k) = self.top_k {
            ensure!(top_k <= 1_000_000, "top_k must be between 0 and 1000000");
        }
        Ok(())
    }
    pub fn is_empty(&self) -> bool {
        self.parameters().is_empty()
    }
    pub fn parameters(&self) -> Map<String, Value> {
        serde_json::to_value(self)
            .expect("Sampling values are serializable")
            .as_object()
            .unwrap()
            .clone()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum Settings {
    #[default]
    Legacy,
    ProviderDefault,
    Focused,
    Exploratory,
    Wildcard,
    Custom(Values),
}
impl Settings {
    pub fn label(&self) -> String {
        match self {
            Self::Legacy => "Legacy (preserve previous behavior)".into(),
            Self::ProviderDefault => "Provider default".into(),
            Self::Focused => "Focused · temperature 0.4".into(),
            Self::Exploratory => "Exploratory · temperature 0.9".into(),
            Self::Wildcard => "Wildcard · temperature 1.1".into(),
            Self::Custom(_) => "Custom sampling".into(),
        }
    }
    pub fn custom_values(&self) -> Values {
        match self {
            Self::Focused => Values {
                temperature: Some(0.4),
                ..Values::default()
            },
            Self::Exploratory => Values {
                temperature: Some(0.9),
                ..Values::default()
            },
            Self::Wildcard => Values {
                temperature: Some(1.1),
                ..Values::default()
            },
            Self::Custom(values) => values.clone(),
            Self::Legacy | Self::ProviderDefault => Values::default(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.custom_values().validate()
    }
    pub fn has_override(&self) -> bool {
        !matches!(self, Self::Legacy | Self::ProviderDefault) && !self.custom_values().is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub parameters: Vec<String>,
    pub verified: bool,
    pub source: String,
    pub detail: String,
}
impl Capabilities {
    fn known(parameters: &[&str], source: &str, detail: &str) -> Self {
        Self {
            parameters: parameters.iter().map(|name| (*name).into()).collect(),
            verified: true,
            source: source.into(),
            detail: detail.into(),
        }
    }
    pub fn supports_parameter(&self, name: &str) -> bool {
        self.parameters.iter().any(|parameter| parameter == name)
    }
    pub fn supports(&self, setting: &Settings) -> bool {
        setting.validate().is_ok()
            && (!setting.has_override()
                || (self.verified
                    && setting
                        .custom_values()
                        .parameters()
                        .keys()
                        .all(|name| self.supports_parameter(name))))
    }
    pub fn validate_selection(&self, setting: &Settings) -> Result<()> {
        setting.validate()?;
        let unsupported: Vec<_> = setting
            .custom_values()
            .parameters()
            .keys()
            .filter(|name| !self.supports_parameter(name))
            .cloned()
            .collect();
        ensure!(
            self.supports(setting),
            "{} cannot be applied to this model: {}. {} Choose Provider default or supported custom values. Source: {}",
            setting.label(),
            unsupported.join(", "),
            self.detail,
            self.source
        );
        Ok(())
    }
}

/// Metadata/documented capabilities only. No generation requests are sent, and
/// a provider-default or Legacy request does not need this discovery step.
pub fn discover(_ollama_url: &str, name: &str) -> Result<Capabilities> {
    if crate::chatgpt::model_id(name).is_some() {
        return Ok(Capabilities { parameters: vec![], verified: true, source: "Signed-in ChatGPT Responses transport".into(), detail: "Sampling overrides are not supported by this ChatGPT plan connection. Provider default preserves its normal model behavior.".into() });
    }
    if crate::groq::model_id(name).is_some() {
        return Ok(Capabilities::known(
            &["temperature", "top_p", "seed"],
            "https://console.groq.com/docs/api-reference",
            "Groq documents temperature, top_p and seed. Its API currently states presence/frequency penalties are unsupported; top_k, min_p and repeat_penalty are not advertised.",
        ));
    }
    match crate::cloud::provider(name) {
        Some(crate::cloud::Provider::Zen) => Ok(Capabilities { parameters: vec![], verified: false, source: "OpenCode Zen model metadata".into(), detail: "Zen does not advertise accepted sampling controls. Explicit overrides cannot be verified and will not be silently dropped.".into() }),
        Some(crate::cloud::Provider::OpenRouter) => {
            let catalog = crate::cloud::catalog(crate::cloud::Provider::OpenRouter)?;
            let entry = catalog["data"].as_array().and_then(|models| models.iter().find(|model| model["id"].as_str() == crate::cloud::model_id(name))).context("Selected model is missing from OpenRouter's sampling metadata")?;
            Ok(openrouter_capabilities(entry))
        }
        None => Ok(Capabilities::known(PARAMETERS, "https://github.com/ollama/ollama/blob/main/api/types.go", "Sampling overrides are passed as documented Ollama options; unset fields keep the server/model defaults.")),
    }
}

pub fn openrouter_capabilities(metadata: &Value) -> Capabilities {
    let values = metadata["supported_parameters"].as_array();
    let parameters = PARAMETERS
        .iter()
        .filter(|name| {
            let wire = if **name == "repeat_penalty" {
                "repetition_penalty"
            } else {
                name
            };
            values.is_some_and(|values| values.iter().any(|value| value == wire))
        })
        .map(|name| (*name).to_owned())
        .collect();
    Capabilities { parameters, verified: values.is_some(), source: "OpenRouter /api/v1/models supported_parameters".into(), detail: "Only parameters advertised for this OpenRouter model can be selected. Routes are required to support the explicit parameters.".into() }
}

/// Apply sampling after transport conversion so JSON mode cannot override an
/// explicit user selection. Returns the exact overrides for secret-free traces.
pub fn apply(
    body: &mut Value,
    name: &str,
    setting: &Settings,
    capabilities: Option<&Capabilities>,
) -> Result<Value> {
    setting.validate()?;
    if setting.has_override() {
        capabilities
            .context("Sampling capabilities must be verified for explicit overrides")?
            .validate_selection(setting)?;
    }
    let ollama = crate::groq::model_id(name).is_none()
        && crate::cloud::provider(name).is_none()
        && crate::chatgpt::model_id(name).is_none();
    let formatted = body.get("format").is_some()
        || body.get("response_format").is_some()
        || body["text"].get("format").is_some();
    for key in PARAMETERS.iter().copied().chain(["repetition_penalty"]) {
        if let Some(object) = body.as_object_mut() {
            object.remove(key);
        }
        if let Some(options) = body.get_mut("options").and_then(Value::as_object_mut) {
            options.remove(key);
        }
    }
    let values = if matches!(setting, Settings::Legacy)
        && (ollama || crate::groq::model_id(name).is_some())
    {
        Values {
            temperature: Some(if formatted { 0.0 } else { 0.4 }),
            ..Values::default()
        }
    } else {
        setting.custom_values()
    };
    let mut parameters = values.parameters();
    if crate::cloud::provider(name) == Some(crate::cloud::Provider::OpenRouter) {
        if let Some(penalty) = parameters.remove("repeat_penalty") {
            parameters.insert("repetition_penalty".into(), penalty);
        }
        if !parameters.is_empty() {
            body["provider"]["require_parameters"] = json!(true);
        }
    }
    if !parameters.is_empty() {
        let target = if ollama { &mut body["options"] } else { body };
        if target.is_null() {
            *target = json!({});
        }
        target
            .as_object_mut()
            .context("Sampling target must be an object")?
            .extend(parameters.clone());
    }
    Ok(
        json!({"selection":setting,"overrides":parameters,"location":if ollama {"options"} else {"request"},"source":if matches!(setting, Settings::Legacy) {"previous transport behavior"} else if setting.has_override() {"explicit user selection"} else {"provider/model defaults; no sampling override sent"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_modes_and_validation_cover_custom_bounds() {
        assert_eq!(
            serde_json::to_value(Settings::default()).unwrap(),
            json!({"mode":"legacy"})
        );
        let custom = Settings::Custom(Values {
            temperature: Some(1.1),
            top_p: Some(0.95),
            top_k: Some(40),
            seed: Some(-1),
            ..Values::default()
        });
        assert_eq!(
            serde_json::from_value::<Settings>(serde_json::to_value(&custom).unwrap()).unwrap(),
            custom
        );
        for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
            assert!(
                Settings::Custom(Values {
                    temperature: Some(temperature),
                    ..Values::default()
                })
                .validate()
                .is_err()
            );
        }
        assert!(
            Settings::Custom(Values {
                top_p: Some(1.1),
                ..Values::default()
            })
            .validate()
            .is_err()
        );
        assert!(
            Settings::Custom(Values {
                presence_penalty: Some(-2.0),
                frequency_penalty: Some(2.0),
                ..Values::default()
            })
            .validate()
            .is_ok()
        );
        assert!(
            serde_json::from_value::<Settings>(json!({"mode":"custom","value":{"temprature":0.8}}))
                .is_err()
        );
    }
    #[test]
    fn provider_default_removes_all_old_overrides_even_in_json_mode() {
        for (name, mut body) in [
            (
                "local",
                json!({"options":{"temperature":0,"top_p":0.9,"num_ctx":8192},"format":"json"}),
            ),
            (
                "groq/fixture",
                json!({"temperature":0,"top_p":0.9,"response_format":{"type":"json_object"}}),
            ),
        ] {
            let applied = apply(&mut body, name, &Settings::ProviderDefault, None).unwrap();
            assert_eq!(applied["overrides"], json!({}));
            assert!(body.get("temperature").is_none());
            assert!(body["options"].get("temperature").is_none());
            assert!(body["options"].get("top_p").is_none());
            if name == "local" {
                assert_eq!(body["options"]["num_ctx"], 8192);
            }
        }
    }
    #[test]
    fn explicit_temperature_survives_json_and_openrouter_aliases_require_support() {
        let supported = discover("unused", "local").unwrap();
        let mut body = json!({"options":{"temperature":0,"num_predict":100},"format":"json"});
        apply(&mut body, "local", &Settings::Exploratory, Some(&supported)).unwrap();
        assert_eq!(body["options"]["temperature"], 0.9);
        let capabilities = openrouter_capabilities(
            &json!({"supported_parameters":["temperature","repetition_penalty"]}),
        );
        let mut body = json!({"provider":{"max_price":{"prompt":0}}});
        apply(
            &mut body,
            "openrouter/fixture:free",
            &Settings::Custom(Values {
                temperature: Some(1.1),
                repeat_penalty: Some(1.2),
                ..Values::default()
            }),
            Some(&capabilities),
        )
        .unwrap();
        assert_eq!(body["temperature"], 1.1);
        assert_eq!(body["repetition_penalty"], 1.2);
        assert!(body.get("repeat_penalty").is_none());
        assert_eq!(body["provider"]["require_parameters"], true);
        assert_eq!(body["provider"]["max_price"]["prompt"], 0);
        assert!(!capabilities.supports(&Settings::Custom(Values {
            min_p: Some(0.1),
            ..Values::default()
        })));
    }
    #[test]
    fn unsupported_cloud_controls_are_rejected_instead_of_silently_dropped() {
        for name in ["chatgpt/fixture", "zen/fixture"] {
            let capabilities = discover("unused", name).unwrap();
            assert!(!capabilities.supports(&Settings::Focused));
            assert!(capabilities.supports(&Settings::ProviderDefault));
            assert!(capabilities.supports(&Settings::Legacy));
            assert!(
                apply(
                    &mut json!({}),
                    name,
                    &Settings::Wildcard,
                    Some(&capabilities)
                )
                .is_err()
            );
        }
        let groq = discover("unused", "groq/fixture").unwrap();
        assert!(groq.supports(&Settings::Wildcard));
        assert!(!groq.supports(&Settings::Custom(Values {
            presence_penalty: Some(0.5),
            ..Values::default()
        })));
    }
    #[test]
    fn legacy_exactly_preserves_old_transport_policy() {
        for name in ["local", "groq/fixture"] {
            let mut body = if name == "local" {
                json!({"options":{}})
            } else {
                json!({})
            };
            let resolved = apply(&mut body, name, &Settings::Legacy, None).unwrap();
            assert_eq!(resolved["overrides"]["temperature"], 0.4);
            let mut json_body = if name == "local" {
                json!({"format":"json"})
            } else {
                json!({"response_format":{"type":"json_object"}})
            };
            assert_eq!(
                apply(&mut json_body, name, &Settings::Legacy, None).unwrap()["overrides"]["temperature"],
                0.0
            );
        }
        for name in ["chatgpt/fixture", "openrouter/fixture:free", "zen/fixture"] {
            assert_eq!(
                apply(&mut json!({}), name, &Settings::Legacy, None).unwrap()["overrides"],
                json!({})
            );
        }
    }
}
