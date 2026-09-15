use crate::{model::Model, project, runner};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub ollama_url: String,
    pub model: String,
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub implementation_calls: u32,
    pub retry_seconds: u64,
    pub web_enabled: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            ollama_url: "http://localhost:11434".into(),
            model: String::new(),
            context_tokens: 32768,
            output_tokens: 4096,
            implementation_calls: 48,
            retry_seconds: 10,
            web_enabled: false,
        }
    }
}
pub fn settings_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .context("Cannot locate user configuration directory")?;
    Ok(base.join("lupin/settings.json"))
}
pub fn settings() -> Result<Settings> {
    let p = settings_path()?;
    if p.exists() {
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    } else {
        Ok(Settings::default())
    }
}
pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
pub fn ask(label: &str, default: &str) -> Result<String> {
    if crate::ui::active() {
        return crate::ui::ask(label, default);
    }
    loop {
        if default.is_empty() {
            print!("{label}: ");
        } else {
            print!("{label} [{default}]: ");
        }
        io::stdout().flush()?;
        let mut line = String::new();
        anyhow::ensure!(
            io::stdin().read_line(&mut line)? != 0,
            "Input closed; setup was not started"
        );
        let value = line.trim();
        if !value.is_empty() {
            return Ok(value.into());
        }
        if !default.is_empty() {
            return Ok(default.into());
        }
    }
}
fn number(label: &str, default: u32, min: u32, max: u32) -> Result<u32> {
    loop {
        let value = ask(label, &default.to_string())?;
        if let Ok(n) = value.parse::<u32>()
            && (min..=max).contains(&n)
        {
            return Ok(n);
        }
        crate::ui::notice(format!("Enter a number between {min} and {max}."));
    }
}
pub fn configure(show: bool) -> Result<()> {
    let path = settings_path()?;
    let mut s = settings()?;
    if show {
        crate::ui::notice(format!(
            "{}\n{}",
            path.display(),
            serde_json::to_string_pretty(&s)?
        ));
        return Ok(());
    }
    crate::ui::notice(
        "Lupin · Shared settings\nThese defaults apply to every project unless overridden."
            .to_string(),
    );
    loop {
        s.ollama_url = ask("Ollama server", &s.ollama_url)?
            .trim_end_matches('/')
            .into();
        let url = s.ollama_url.clone();
        let models = crate::ui::busy("Connecting to Ollama", move || {
            Ok(reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?
                .get(format!("{}/api/tags", url))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Value>())?)
        });
        match models {
            Ok(v) => {
                let names: Vec<String> = v["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["name"].as_str().map(str::to_owned))
                    .collect();
                anyhow::ensure!(
                    !names.is_empty(),
                    "No models are installed on this Ollama server."
                );
                if io::stdin().is_terminal() {
                    let index = names.iter().position(|n| n == &s.model).unwrap_or(0);
                    let Some(index) = crate::menu::select("Choose model", &names, index)? else {
                        anyhow::bail!("Setup cancelled. Your progress is saved.");
                    };
                    s.model = names[index].clone();
                    break;
                }
                for (i, name) in names.iter().enumerate() {
                    crate::ui::notice(format!("  {}. {name}", i + 1));
                }
                let chosen = ask("Model name or number", &s.model)?;
                s.model = chosen
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| names.get(n))
                    .cloned()
                    .unwrap_or(chosen);
                if !names.contains(&s.model) {
                    crate::ui::notice(
                        "That model is not installed on this server. Choose an installed model."
                            .to_string(),
                    );
                    continue;
                }
                break;
            }
            Err(e) => crate::ui::notice(format!(
                "Could not reach Ollama: {e}\nEnter the server address again, or Ctrl-C to cancel."
            )),
        }
    }
    s.context_tokens = number("Context window (tokens)", s.context_tokens, 4096, 262144)?;
    s.output_tokens = number(
        "Maximum response (tokens)",
        s.output_tokens.min(s.context_tokens / 4),
        256,
        s.context_tokens - 2048,
    )?;
    s.implementation_calls = number(
        "Model responses per implementation task",
        s.implementation_calls,
        1,
        100,
    )?;
    save(&path, &s)?;
    crate::ui::notice(format!("Saved shared settings to {}.", path.display()));
    Ok(())
}
pub fn find_project() -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir()?;
    for p in cwd.ancestors() {
        let config = p.join("lupin.json");
        if config.is_file() {
            return Ok(Some(config));
        }
        if p.join(".git").exists() {
            break;
        }
    }
    Ok(None)
}
#[derive(Serialize, Deserialize)]
struct Draft {
    pitch: String,
    goal: String,
    #[serde(default)]
    feedback: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct GoalReply {
    goal: String,
}
pub fn wizard() -> Result<PathBuf> {
    let root = std::env::current_dir()?;
    let config = root.join("lupin.json");
    anyhow::ensure!(
        !config.exists(),
        "Project already configured; run lupin to resume."
    );
    crate::ui::notice(format!(
        "\nLupin · Get started\nProject: {}\n",
        root.display()
    ));
    if !settings_path()?.exists() || settings()?.model.trim().is_empty() {
        configure(false)?;
    }
    let s = settings()?;
    crate::ui::notice(format!(
        "Using {} on {} (change from Settings on the home menu).",
        s.model, s.ollama_url
    ));
    let draft_path = root.join(".lupin/goal-draft.json");
    let mut draft = if draft_path.exists() {
        crate::ui::notice("Resuming your unfinished goal draft.".to_string());
        serde_json::from_slice::<Draft>(&fs::read(&draft_path)?)?
    } else {
        Draft {
            pitch: ask("Describe what you want to build", "")?,
            goal: String::new(),
            feedback: String::new(),
        }
    };
    save(&draft_path, &draft)?;
    loop {
        if draft.goal.is_empty() || !draft.feedback.is_empty() {
            crate::ui::notice("Drafting your project goal…".to_string());
            let current = settings()?;
            let model = Model::new(
                &current.ollama_url,
                &current.model,
                current.context_tokens,
                current.output_tokens,
                Arc::new(AtomicBool::new(false)),
            )?;
            let input = json!({"pitch":draft.pitch,"current_draft":draft.goal,"requested_changes":draft.feedback});
            let result = crate::ui::busy("Drafting your project goal", move || {
                model.structured::<GoalReply>(
                "Help define a software project goal. Return JSON {goal: string}. Expand the user's pitch into a clear goal with concrete capabilities, quality expectations, constraints, and observable success criteria. Preserve the user's ambition and explicit version/feature targets; do not silently narrow them to an MVP. Do not invent user requirements: label assumptions and leave genuinely unspecified choices flexible. Incorporate requested revisions. Keep under 700 words. This is goal drafting only, not implementation.",
                input)
            });
            match result {
                Ok(value) if !value.goal.trim().is_empty() && value.goal.len() <= 12000 => {
                    draft.goal = value.goal;
                    draft.feedback.clear();
                    save(&draft_path, &draft)?;
                }
                Ok(_) => {
                    draft_recovery(&root, "The reply did not contain a usable project goal.")?;
                    continue;
                }
                Err(e) => {
                    draft_recovery(&root, &format!("{e:#}"))?;
                    continue;
                }
            }
        }
        crate::ui::notice(format!(
            "\n── Proposed project goal ──\n{}\n──────────────────────────",
            draft.goal
        ));
        let action = if io::stdin().is_terminal() {
            match crate::menu::select(
                "Review your goal",
                &[
                    "Accept goal".into(),
                    "Request changes".into(),
                    "Back to menu".into(),
                ],
                0,
            )? {
                Some(0) => "accept".into(),
                Some(1) => ask("What should change?", "")?,
                _ => anyhow::bail!("Goal draft saved. Continue setup when you are ready."),
            }
        } else {
            ask("Type accept, or describe the changes you want", "")?
        };
        if action.eq_ignore_ascii_case("accept") {
            break;
        }
        draft.feedback = action;
        save(&draft_path, &draft)?;
    }
    crate::ui::notice("\nChoose a check Lupin must pass before accepting changes.\nFor a new Rust project, cargo test starts failing until the project is created.\nCommands support quoted arguments; shell operators are not interpreted.".to_string());
    let check = loop {
        let text = ask("Validation command", "cargo test")?;
        match shell_words::split(&text) {
            Ok(argv) if !argv.is_empty() => break argv,
            _ => crate::ui::notice("Enter a valid command with balanced quotes.".to_string()),
        }
    };
    let timeout = number("Check timeout (seconds)", 120, 1, 86400)?;
    // Do not accidentally initialize or commit an ancestor repository.
    let top = project::git(&root, &["rev-parse", "--show-toplevel"]).ok();
    if let Some(top) = top {
        anyhow::ensure!(
            fs::canonicalize(top)? == root,
            "This directory is inside another Git repository. Run setup at its root or in a separate directory."
        );
    } else {
        project::git(&root, &["init"])?;
    }
    // Exclude runtime state before offering to commit project files.
    let exclude = root.join(project::git(
        &root,
        &["rev-parse", "--git-path", "info/exclude"],
    )?);
    let mut ignored = fs::read_to_string(&exclude).unwrap_or_default();
    for entry in ["/.lupin/", "/lupin.json"] {
        if !ignored.lines().any(|line| line == entry) {
            ignored.push_str(&format!("\n{entry}\n"));
        }
    }
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(exclude, ignored)?;
    if project::git(&root, &["rev-parse", "HEAD"]).is_err() {
        let scope = [".", ":(exclude).lupin", ":(exclude)lupin.json"];
        let mut list = vec![
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ];
        list.extend(scope);
        let files = project::git(&root, &list)?;
        let has_files = !files.is_empty();
        let paths: Vec<String> = files
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| format!(":(literal){p}"))
            .collect();
        if has_files {
            crate::ui::notice("\nThis project has no commits yet. Lupin can commit all project files as its starting point. Git-ignored files and Lupin's local state are excluded.\n".to_string());
            for file in files.split('\0').filter(|s| !s.is_empty()).take(40) {
                crate::ui::notice(format!("  {file}"));
            }
            let accepted = if io::stdin().is_terminal() {
                crate::menu::select(
                    "Create the first commit?",
                    &[
                        "Commit project files and continue".into(),
                        "Back to menu".into(),
                    ],
                    0,
                )? == Some(0)
            } else {
                matches!(
                    ask("Commit project files and continue? (yes/no)", "yes")?
                        .to_lowercase()
                        .as_str(),
                    "yes" | "y"
                )
            };
            anyhow::ensure!(accepted, "No commit created. Your goal draft is saved.");
            let mut add = vec!["add", "-A", "--"];
            add.extend(paths.iter().map(String::as_str));
            project::git(&root, &add)?;
        }
        let mut commit = vec![
            "-c",
            "user.name=Lupin",
            "-c",
            "user.email=lupin@localhost",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "Initialize Lupin project",
        ];
        if has_files {
            commit.extend(["--only", "--"]);
            commit.extend(paths.iter().map(String::as_str));
        } else {
            // --only with no paths makes an empty commit without importing staged runtime files.
            commit.push("--only");
        }
        project::git(&root, &commit)?;
        crate::ui::notice("Created the first commit.".to_string());
    }
    save(
        &config,
        &json!({"repo":".","goal":draft.goal,"state_dir":".lupin",
        "checks":[{"argv":check,"timeout_seconds":timeout}]}),
    )?;
    // Check merged project/global settings before starting.
    runner::load(&config)?;
    crate::ui::notice(
        "\nGoal accepted and saved. Select Resume project from the home menu to start.".to_string(),
    );
    Ok(config)
}

fn available_models(s: &Settings) -> Result<Vec<String>> {
    let v: Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .get(format!("{}/api/tags", s.ollama_url.trim_end_matches('/')))
        .send()
        .context("Could not connect to Ollama. Check the server address in Settings")?
        .error_for_status()?
        .json()?;
    let names: Vec<String> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            !m["capabilities"].as_array().is_some_and(|a| {
                a.contains(&json!("embedding")) && !a.contains(&json!("completion"))
            })
        })
        .filter_map(|m| m["name"].as_str().map(str::to_owned))
        .collect();
    anyhow::ensure!(
        !names.is_empty(),
        "No chat models found on this Ollama server."
    );
    Ok(names)
}
pub fn choose_model() -> Result<()> {
    let mut s = settings()?;
    crate::ui::notice(format!("\nLoading models from {}…", s.ollama_url));
    let connection = s.clone();
    let names = crate::ui::busy("Loading available models", move || {
        available_models(&connection)
    })?;
    let selected = names.iter().position(|n| n == &s.model).unwrap_or(0);
    if let Some(index) = crate::menu::select("Choose model (shared default)", &names, selected)? {
        s.model = names[index].clone();
        save(&settings_path()?, &s)?;
        crate::ui::notice(format!("Model saved: {}", s.model));
    }
    Ok(())
}
pub fn settings_menu() -> Result<()> {
    loop {
        let mut s = settings()?;
        let items = vec![
            format!("Ollama server     {}", s.ollama_url),
            format!(
                "Model             {}",
                if s.model.is_empty() {
                    "Not selected"
                } else {
                    &s.model
                }
            ),
            format!("Context window    {}", s.context_tokens),
            format!("Response limit    {}", s.output_tokens),
            format!("Task responses    {}", s.implementation_calls),
            format!("Retry delay       {} seconds", s.retry_seconds),
            format!(
                "Brave web tools   {}",
                if s.web_enabled { "Enabled" } else { "Disabled" }
            ),
            format!(
                "Brave API key     {}",
                if crate::web_tools::key()?.is_some() {
                    "Configured"
                } else {
                    "Not configured"
                }
            ),
            "Test Brave connection".into(),
            "Back".into(),
        ];
        let Some(index) = crate::menu::select("Shared settings", &items, 0)? else {
            return Ok(());
        };
        match index {
            0 => {
                let url = ask("Ollama server", &s.ollama_url)?;
                let parsed = reqwest::Url::parse(&url)
                    .context("Enter a full server address, such as http://localhost:11434")?;
                anyhow::ensure!(
                    matches!(parsed.scheme(), "http" | "https"),
                    "Server must use http or https."
                );
                s.ollama_url = url.trim_end_matches('/').into();
            }
            1 => {
                choose_model()?;
                continue;
            }
            2 => {
                s.context_tokens =
                    number("Context window (tokens)", s.context_tokens, 4096, 262144)?;
                s.output_tokens = s.output_tokens.min(s.context_tokens - 2048);
            }
            3 => {
                s.output_tokens = number(
                    "Response limit (tokens)",
                    s.output_tokens,
                    256,
                    s.context_tokens - 2048,
                )?
            }
            4 => {
                s.implementation_calls =
                    number("Model responses per task", s.implementation_calls, 1, 100)?
            }
            5 => {
                s.retry_seconds =
                    number("Retry delay (seconds)", s.retry_seconds as u32, 1, 3600)? as u64
            }
            6 => {
                if !s.web_enabled {
                    anyhow::ensure!(
                        crate::web_tools::key()?.is_some(),
                        "Add a Brave API key first."
                    );
                }
                s.web_enabled = !s.web_enabled;
            }
            7 => {
                match crate::menu::select(
                    "Brave API key",
                    &[
                        "Enter or replace key".into(),
                        "Remove key".into(),
                        "Back".into(),
                    ],
                    0,
                )? {
                    Some(0) => {
                        let value = if crate::ui::active() {
                            crate::ui::ask_secret("Brave API key · input is masked")?
                        } else {
                            dialoguer::Password::new()
                                .with_prompt("Brave API key")
                                .interact()?
                        };
                        crate::web_tools::save_key(&crate::web_tools::key_path()?, &value)?;
                        s.web_enabled = true;
                    }
                    Some(1) => {
                        let path = crate::web_tools::key_path()?;
                        if path.exists() {
                            fs::remove_file(path)?;
                        }
                        s.web_enabled = false;
                    }
                    _ => {}
                }
            }
            8 => {
                let result = crate::ui::busy("Testing Brave Search", || {
                    crate::web_tools::search("site:doc.rust-lang.org Rust str get")
                })?;
                crate::ui::notice(format!(
                    "Brave connection works. Received {} results.",
                    result["results"].as_array().map_or(0, Vec::len)
                ));
            }
            _ => return Ok(()),
        }
        save(&settings_path()?, &s)?;
    }
}

fn draft_recovery(root: &Path, detail: &str) -> Result<()> {
    let log = root.join(".lupin/goal-error.txt");
    fs::write(&log, detail)?;
    crate::ui::notice(format!(
        "\nCouldn't finish drafting your goal. Your pitch and any earlier draft are saved.\nDetails: {}",
        log.display()
    ));
    if io::stdin().is_terminal() {
        match crate::menu::select(
            "What would you like to do?",
            &[
                "Retry drafting".into(),
                "Settings".into(),
                "Back to menu".into(),
            ],
            0,
        )? {
            Some(0) => {}
            Some(1) => settings_menu()?,
            _ => anyhow::bail!("Draft saved. Return to Setup to continue."),
        }
    } else {
        ask("Press Enter to retry, or Ctrl-C to exit", "retry")?;
    }
    Ok(())
}
