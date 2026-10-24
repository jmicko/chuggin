//! Provider-neutral discovery and connection editing. Stored model identifiers
//! remain compatible with existing projects; choosing a model never changes a
//! global default or a project's endpoint until the caller accepts the result.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Groq,
    Ollama,
    OpenAI,
    Zen,
    OpenRouter,
}
pub const PROVIDERS: [Provider; 5] = [
    Provider::Groq,
    Provider::Ollama,
    Provider::OpenAI,
    Provider::Zen,
    Provider::OpenRouter,
];
impl Provider {
    pub fn for_model(name: &str) -> Option<Self> {
        if name.is_empty() {
            None
        } else if crate::chatgpt::model_id(name).is_some() {
            Some(Self::OpenAI)
        } else if crate::groq::model_id(name).is_some() {
            Some(Self::Groq)
        } else {
            Some(match crate::cloud::provider(name) {
                Some(crate::cloud::Provider::Zen) => Self::Zen,
                Some(crate::cloud::Provider::OpenRouter) => Self::OpenRouter,
                None => Self::Ollama,
            })
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Groq => "Groq",
            Self::Ollama => "Ollama",
            Self::OpenAI => "OpenAI · ChatGPT plan",
            Self::Zen => "OpenCode Zen",
            Self::OpenRouter => "OpenRouter",
        }
    }
    fn endpoint(self, local_url: &str) -> &str {
        match self {
            Self::Groq => crate::groq::ENDPOINT,
            Self::Ollama => local_url,
            Self::OpenAI => "https://api.openai.com/v1",
            Self::Zen => "https://opencode.ai/zen/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }
    fn credential_ready(self) -> Result<bool> {
        Ok(match self {
            Self::Groq => crate::groq::key().is_ok(),
            Self::OpenRouter => crate::cloud::key("openrouter/connection-check")?.is_some(),
            Self::OpenAI => crate::chatgpt::accounts()?
                .iter()
                .any(|a| a.active && a.connected && a.plan_enabled),
            Self::Ollama | Self::Zen => true,
        })
    }
    fn status(self, local_url: &str) -> Result<String> {
        Ok(match self {
            Self::Ollama => {
                if local_url.trim().is_empty() {
                    "Set up connection".into()
                } else {
                    local_url.into()
                }
            }
            Self::Zen => "Anonymous · live free catalog".into(),
            Self::OpenAI => crate::chatgpt::account_status()?,
            _ if self.credential_ready()? => "API key configured".into(),
            _ => "Add API key".into(),
        })
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Ollama => {
                "Models installed on your selected server. Local inference has no API token allowance; Ollama cloud models may have account limits."
            }
            Self::Groq => {
                "Models available to your Groq account. The model list does not report your account's free or paid allowance. Shared quota pacing remains enabled."
            }
            Self::OpenAI => {
                "Use your authorized ChatGPT plan. Requests share your existing plan allowance. Account-specific model availability is refreshed from OpenAI; usage and app limits are managed in ChatGPT Settings → Usage. OpenAI controls response length; this connection does not enforce Chuggin’s response-token setting."
            }
            Self::Zen => {
                "The catalog checks current availability, pricing and tool support. Free pricing does not mean unlimited access: capacity, rate limits and previews can change. Chuggin uses anonymous access and does not switch to paid models."
            }
            Self::OpenRouter => {
                "The catalog checks current zero-cost models with tool support. An API key is needed for inference. Free access has provider and account limits; paid fallback remains disabled."
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub badge: String,
    pub context: Option<u32>,
    pub output: Option<u32>,
    pub selectable: bool,
    pub explanation: String,
}
impl Entry {
    pub fn row(&self, current: &str) -> String {
        format!(
            "{} · {}{}",
            self.name,
            self.badge,
            if self.id == current {
                " · current"
            } else {
                ""
            }
        )
    }
    pub fn details(&self) -> String {
        let mut detail = format!("{}\nModel ID: {}\n{}", self.name, self.id, self.badge);
        if let Some(context) = self.context {
            detail.push_str(&format!(" · context: {context} tokens"));
        }
        if let Some(output) = self.output {
            detail.push_str(&format!(" · max output: {output} tokens"));
        }
        detail.push_str(&format!("\n{}", self.explanation));
        detail
    }
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub model: String,
    pub local_url: Option<String>,
}
impl Selection {
    pub fn patch(&self, field: &str) -> Value {
        let mut value = json!({field:&self.model});
        if let Some(url) = &self.local_url {
            value["ollama_url"] = json!(url);
        }
        value
    }
}

pub enum Action {
    Select(usize),
    Refresh,
    Connection,
    Provider,
    Cancel,
}

fn local_entries(local_url: &str) -> Result<Vec<Entry>> {
    validate_url(local_url)?;
    let value: Value = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("{}/api/tags", local_url.trim_end_matches('/')))
        .send()
        .context("Could not connect to this Ollama server")?
        .error_for_status()?
        .json()?;
    let models = value["models"]
        .as_array()
        .context("Ollama did not return a model list")?;
    let mut entries: Vec<_> = models.iter().filter_map(|model| {
        let id = model["name"].as_str().or_else(|| model["model"].as_str())?;
        if id.is_empty() || id.chars().any(char::is_whitespace) || Provider::for_model(id) != Some(Provider::Ollama) { return None; }
        let embedding = model["capabilities"].as_array().is_some_and(|caps| caps.contains(&json!("embedding")) && !caps.contains(&json!("completion")));
        Some(Entry { id:id.into(), name:id.into(), badge:if id.ends_with(":cloud") { "Ollama cloud · account limits" } else { "Local server" }.into(), context:None, output:None, selectable:!embedding, explanation:if embedding { "Embedding-only models cannot run the project loop." } else { "Installed on this connection. Selects this model without pulling or generating anything." }.into() })
    }).collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    Ok(entries)
}

fn load(provider: Provider, local_url: &str) -> Result<Vec<Entry>> {
    match provider {
        Provider::Ollama => local_entries(local_url),
        Provider::Groq => Ok(crate::groq::models()?.into_iter().map(|id| Entry { name:crate::groq::model_id(&id).unwrap_or(&id).into(), id, badge:"Account quota".into(), context:None, output:None, selectable:true, explanation:"Groq lists this chat model. Free or paid allowance depends on your account; the catalog does not report remaining usage.".into() }).collect()),
        Provider::OpenAI => {
            let catalog = crate::chatgpt::models()?;
            let models = catalog["models"].as_array().context("OpenAI did not return an account model catalog")?;
            Ok(models.iter().filter(|model| model["visibility"] == "list").filter_map(|model| {
                let id = model["slug"].as_str()?;
                if id.is_empty() || id.chars().any(char::is_whitespace) { return None; }
                Some(Entry { id:format!("chatgpt/{id}"), name:model["display_name"].as_str().unwrap_or(id).into(), badge:"ChatGPT plan".into(), context:model["context_window"].as_u64().and_then(|n| u32::try_from(n).ok()), output:model["max_output_tokens"].as_u64().and_then(|n| u32::try_from(n).ok()), selectable:true, explanation:"Listed for the selected ChatGPT account. Requests consume its existing plan allowance; availability is verified by a completed response.".into() })
            }).collect())
        }
        Provider::Zen | Provider::OpenRouter => {
            let cloud = if provider == Provider::Zen { crate::cloud::Provider::Zen } else { crate::cloud::Provider::OpenRouter };
            Ok(crate::cloud::model_entries(cloud, true)?.into_iter().map(|entry| {
                let selectable = entry.selectable();
                let explanation = entry.detail().to_owned();
                Entry { id:entry.id, name:entry.name, context:entry.context_tokens, output:entry.output_tokens, selectable, explanation, badge:match entry.pricing { crate::cloud::PriceStatus::Free => "Free · access limits apply", crate::cloud::PriceStatus::Paid => "Paid · unavailable here", crate::cloud::PriceStatus::Unknown => "Pricing unconfirmed" }.into() }
            }).collect())
        }
    }
}

fn fetch(provider: Provider, url: &str) -> Result<Vec<Entry>> {
    let url = url.to_owned();
    crate::ui::busy(
        &format!("Refreshing {} model catalog", provider.label()),
        move || load(provider, &url),
    )
}

/// Both back levels are ordinary navigation. A refresh never commits a selection.
pub fn choose(current: &str, local_url: &str, title: &str) -> Result<Option<Selection>> {
    let mut url = local_url.to_owned();
    let mut provider_index = Provider::for_model(current)
        .and_then(|provider| PROVIDERS.iter().position(|p| *p == provider))
        .unwrap_or(0);
    loop {
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "{title}\nCurrent model: {}\nChoose a provider, then browse its live model catalog. Connections are shared; this selection only changes the requested model role.",
            if current.is_empty() {
                "Not selected"
            } else {
                current
            }
        ));
        let mut rows = PROVIDERS
            .iter()
            .map(|p| Ok(format!("{} · {}", p.label(), p.status(&url)?)))
            .collect::<Result<Vec<_>>>()?;
        rows.extend([
            "Manage inference providers…".into(),
            "Cancel selection".into(),
        ]);
        let Some(index) = crate::menu::select("Inference provider", &rows, provider_index)? else {
            return Ok(None);
        };
        if index == PROVIDERS.len() {
            let old_url = crate::setup::settings()?.ollama_url;
            connections_menu()?;
            let new_url = crate::setup::settings()?.ollama_url;
            if old_url != new_url {
                url = new_url;
            }
            continue;
        }
        let Some(provider) = PROVIDERS.get(index).copied() else {
            return Ok(None);
        };
        provider_index = index;
        let mut selected = current.to_owned();
        loop {
            if matches!(provider, Provider::OpenAI | Provider::Groq)
                && !provider.credential_ready()?
            {
                crate::ui::clear_notes();
                crate::ui::notice(format!(
                    "Connect {} to retrieve its account model catalog. Signing in or adding a key does not change your project model.\n{}",
                    provider.label(),
                    provider.description()
                ));
                match crate::menu::select(
                    "Connect provider",
                    &[
                        "Connection settings…".into(),
                        "Choose another provider".into(),
                        "Cancel selection".into(),
                    ],
                    0,
                )? {
                    Some(0) => {
                        connection_menu(provider)?;
                        continue;
                    }
                    Some(2) => return Ok(None),
                    _ => break,
                }
            }
            let entries = match fetch(provider, &url) {
                Ok(entries) => entries,
                Err(error) => {
                    crate::ui::clear_notes();
                    crate::ui::notice(format!(
                        "Could not refresh {} models.\n{error:#}\nYour selected model has not changed.",
                        provider.label()
                    ));
                    match crate::menu::select(
                        "Model catalog unavailable",
                        &[
                            "Retry catalog refresh".into(),
                            "Connection settings…".into(),
                            "Choose another provider".into(),
                            "Cancel selection".into(),
                        ],
                        0,
                    )? {
                        Some(0) => continue,
                        Some(1) => {
                            edit_connection(provider, &mut url)?;
                            continue;
                        }
                        Some(3) => return Ok(None),
                        _ => break,
                    }
                }
            };
            let checked = chrono::Local::now().format("%H:%M:%S").to_string();
            match crate::ui::select_model(title, provider, &entries, &selected, &checked)? {
                Action::Select(index) => {
                    let entry = &entries[index];
                    selected = entry.id.clone();
                    if !provider.credential_ready()? {
                        crate::ui::clear_notes();
                        crate::ui::notice(format!(
                            "{} is listed, but {} needs a connection before it can run. Your project settings have not changed.",
                            entry.name,
                            provider.label()
                        ));
                        if crate::menu::select(
                            "Connect provider",
                            &["Connection settings…".into(), "Back to models".into()],
                            0,
                        )? == Some(0)
                        {
                            connection_menu(provider)?;
                        }
                        continue;
                    }
                    return Ok(Some(Selection {
                        model: entry.id.clone(),
                        local_url: (provider == Provider::Ollama).then(|| url.clone()),
                    }));
                }
                Action::Refresh => continue,
                Action::Connection => {
                    edit_connection(provider, &mut url)?;
                }
                Action::Provider => break,
                Action::Cancel => return Ok(None),
            }
        }
    }
}

fn edit_connection(provider: Provider, url: &mut String) -> Result<()> {
    let old_url = crate::setup::settings()?.ollama_url;
    connection_menu(provider)?;
    let new_url = crate::setup::settings()?.ollama_url;
    if provider == Provider::Ollama && (old_url != new_url || url.is_empty()) {
        *url = new_url;
    }
    Ok(())
}

pub fn connections_menu() -> Result<()> {
    loop {
        let settings = crate::setup::settings()?;
        crate::ui::clear_notes();
        crate::ui::notice("Manage inference providers in one place. Connections are global; each project and role keeps its own selected model. Catalog tests do not generate responses.".into());
        let mut rows = PROVIDERS
            .iter()
            .map(|p| {
                Ok(format!(
                    "{} · {}",
                    p.label(),
                    p.status(&settings.ollama_url)?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        rows.push("Back".into());
        let selected = Provider::for_model(&settings.model)
            .and_then(|p| PROVIDERS.iter().position(|v| *v == p))
            .unwrap_or(0);
        let Some(index) =
            crate::menu::select("Inference providers · shared connections", &rows, selected)?
        else {
            return Ok(());
        };
        let Some(provider) = PROVIDERS.get(index) else {
            return Ok(());
        };
        connection_menu(*provider)?;
    }
}

fn validate_url(url: &str) -> Result<()> {
    let url = reqwest::Url::parse(url)
        .context("Enter a server URL, for example http://localhost:11434")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Server URL must use HTTP or HTTPS"
    );
    Ok(())
}

pub fn connection_menu(provider: Provider) -> Result<()> {
    loop {
        let settings = crate::setup::settings()?;
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "{}\nShared connection: changes apply wherever this connection is used. Existing project model choices are preserved.",
            provider.description()
        ));
        let endpoint = provider.endpoint(&settings.ollama_url);
        let mut rows = vec![format!(
            "{} · {endpoint}",
            if provider == Provider::Ollama {
                "Server URL"
            } else {
                "API endpoint"
            }
        )];
        rows.push(match provider {
            Provider::Ollama => "Authentication · not required".into(),
            Provider::Zen => "Authentication · anonymous free access".into(),
            Provider::OpenAI => format!("Account · {}", crate::chatgpt::account_status()?),
            _ => format!(
                "API key · {}",
                if provider.credential_ready()? {
                    "Configured"
                } else {
                    "Not configured"
                }
            ),
        });
        rows.push("Test catalog connection".into());
        if provider == Provider::Groq {
            rows.push("Quota and pacing…".into());
        }
        rows.push("Back".into());
        match crate::menu::select(
            &format!("{} · shared connection", provider.label()),
            &rows,
            0,
        )? {
            Some(0) if provider == Provider::Ollama => {
                crate::ui::clear_notes();
                crate::ui::notice("This server address is shared by new projects. Existing project endpoint overrides remain unchanged. Selecting a model from this connection saves the endpoint with that model.".into());
                let url = crate::setup::ask("Ollama server URL", &settings.ollama_url)?;
                validate_url(&url)?;
                let mut fresh = crate::setup::settings()?;
                fresh.ollama_url = url.trim_end_matches('/').into();
                crate::setup::save(&crate::setup::settings_path()?, &fresh)?;
            }
            Some(0 | 1) if matches!(provider, Provider::Ollama | Provider::Zen) => {
                crate::ui::show("Connection details", provider.description())?;
            }
            Some(0) => {
                crate::ui::show(
                    "API endpoint",
                    &format!("{endpoint}\n\n{}", provider.description()),
                )?;
            }
            Some(1) if provider == Provider::OpenAI => accounts_menu()?,
            Some(1) => edit_key(provider)?,
            Some(2) => match fetch(provider, &settings.ollama_url) {
                Ok(entries) => crate::ui::show(
                    "Catalog connection works",
                    &format!(
                        "{}\n{} selectable models currently listed. This checks the catalog only; it does not generate a response or verify remaining allowance.\n\n{}",
                        provider.label(),
                        entries.iter().filter(|e| e.selectable).count(),
                        entries
                            .iter()
                            .map(|e| format!("{} · {}", e.name, e.badge))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                )?,
                Err(error) => crate::ui::show(
                    "Catalog connection failed",
                    &format!("{error:#}\nUpdate the connection or retry the catalog check."),
                )?,
            },
            Some(3) if provider == Provider::Groq => crate::setup::groq_limits_menu()?,
            _ => return Ok(()),
        }
    }
}

fn edit_key(provider: Provider) -> Result<()> {
    let file = match provider {
        Provider::Groq => "groq.key",
        Provider::OpenRouter => "openrouter.key",
        _ => return Ok(()),
    };
    let title = format!("{} API key", provider.label());
    match crate::menu::select(
        &title,
        &[
            "Enter or replace key".into(),
            "Remove key".into(),
            "Back".into(),
        ],
        0,
    )? {
        Some(0) => {
            crate::ui::clear_notes();
            crate::ui::notice("The key is stored privately in your user configuration and shared across projects. It is never added to project files or model prompts.".into());
            let key = if crate::ui::active() {
                crate::ui::ask_secret(&format!("{title} · input is masked"))?
            } else {
                dialoguer::Password::new().with_prompt(&title).interact()?
            };
            crate::web_tools::save_key(&crate::groq::path(file)?, &key)?;
        }
        Some(1) => {
            let path = crate::groq::path(file)?;
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn accounts_menu() -> Result<()> {
    loop {
        let accounts = crate::chatgpt::accounts()?;
        crate::ui::clear_notes();
        crate::ui::notice("Continue with ChatGPT opens your browser to authorize Chuggin. Your eligible plan's usage is shared with your other apps. You can cap or revoke Chuggin in ChatGPT Settings → Usage. Signing in does not change any project's model.".into());
        let mut rows: Vec<_> = accounts
            .iter()
            .map(|a| {
                format!(
                    "{}{}{}",
                    a.label,
                    if a.active { " · active" } else { "" },
                    if !a.connected {
                        " · reconnect"
                    } else if !a.plan_enabled {
                        " · enable plan access"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        rows.extend([
            "Continue with ChatGPT · add account".into(),
            "Disconnect active account".into(),
            "Manage usage in ChatGPT".into(),
            "Back".into(),
        ]);
        let Some(index) = crate::menu::select(
            "OpenAI · ChatGPT accounts",
            &rows,
            accounts
                .iter()
                .position(|a| a.active)
                .unwrap_or(accounts.len()),
        )?
        else {
            return Ok(());
        };
        if let Some(account) = accounts.get(index) {
            if account.connected && account.plan_enabled {
                crate::chatgpt::select_account(&account.id)?;
            } else {
                let id = account.id.clone();
                crate::ui::busy("Waiting for ChatGPT browser sign-in", move || {
                    crate::chatgpt::sign_in(Some(&id))
                })?;
            }
        } else {
            match index - accounts.len() {
                0 => {
                    crate::ui::busy("Waiting for ChatGPT browser sign-in", || {
                        crate::chatgpt::sign_in(None)
                    })?;
                }
                1 => {
                    let message = crate::chatgpt::disconnect_active()?;
                    crate::ui::show("ChatGPT account disconnected", &message)?;
                }
                2 => {
                    crate::ui::show(
                        "Manage ChatGPT plan usage",
                        "Open https://chatgpt.com and go to Settings → Usage to set Chuggin's share of your plan. Disconnect access in Settings → Security and login → Sign in with ChatGPT.",
                    )?;
                }
                _ => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_reserved_prefixes_select_remote_providers() {
        assert_eq!(
            Provider::for_model("hf.co/community/model:Q4_K_M"),
            Some(Provider::Ollama)
        );
        assert_eq!(
            Provider::for_model("zen/new-free-model"),
            Some(Provider::Zen)
        );
        assert_eq!(
            Provider::for_model("chatgpt/account-model"),
            Some(Provider::OpenAI)
        );
        assert_eq!(
            Provider::for_model("groq/account-model"),
            Some(Provider::Groq)
        );
        assert_eq!(Provider::for_model(""), None);
    }
    #[test]
    fn model_and_effective_local_connection_are_one_selection() {
        let selection = Selection {
            model: "model:latest".into(),
            local_url: Some("http://127.0.0.1:23456".into()),
        };
        assert_eq!(
            selection.patch("model"),
            json!({"model":"model:latest","ollama_url":"http://127.0.0.1:23456"})
        );
        let cloud = Selection {
            model: "zen/new-model".into(),
            local_url: None,
        };
        assert_eq!(
            cloud.patch("chat_model"),
            json!({"chat_model":"zen/new-model"})
        );
    }
}
