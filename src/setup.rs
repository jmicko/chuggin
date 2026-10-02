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
    pub helpers: crate::agents::Settings,
    pub active_hours: crate::schedule::Schedule,
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
            helpers: crate::agents::Settings::default(),
            active_hours: crate::schedule::Schedule::Always,
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
    Ok(base.join("chuggin/settings.json"))
}
pub fn settings() -> Result<Settings> {
    let p = settings_path()?;
    if p.exists() {
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    } else {
        Ok(Settings::default())
    }
}
const DRAFT_SETTING_FIELDS: &[&str] = &[
    "model",
    "ollama_url",
    "context_tokens",
    "output_tokens",
    "request_timeout_seconds",
    "run_duration_seconds",
    "allow_goal_completion",
    "command_review_seconds",
    "chat_model",
    "helpers",
    "active_hours",
];

fn draft_settings_path(root: &Path) -> PathBuf {
    root.join(".chuggin/project-draft-settings.json")
}

fn draft_settings(root: &Path) -> Result<Value> {
    let path = draft_settings_path(root);
    let draft = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!("Could not read project settings draft: {}", path.display())
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let fields = draft
        .as_object()
        .context("Project settings draft must be an object")?;
    for field in fields.keys() {
        anyhow::ensure!(
            DRAFT_SETTING_FIELDS.contains(&field.as_str()),
            "Unknown project settings draft field: {field}"
        );
    }
    Ok(draft)
}

fn draft_config_with(root: &Path, defaults: &Settings, draft: &Value) -> Result<runner::Config> {
    let root = fs::canonicalize(root).context("Project folder is unavailable")?;
    let mut merged = serde_json::to_value(defaults)?;
    merged["repo"] = json!(root);
    merged["goal"] = json!("");
    merged["checks"] = json!([]);
    merged["state_dir"] = json!(root.join(".chuggin"));
    merged["active_hours"] = json!({"mode":"shared"});
    let fields = draft
        .as_object()
        .context("Project settings draft must be an object")?;
    for (key, value) in fields {
        anyhow::ensure!(
            DRAFT_SETTING_FIELDS.contains(&key.as_str()),
            "Unknown project settings draft field: {key}"
        );
        merged[key] = value.clone();
    }
    let config: runner::Config = serde_json::from_value(merged)
        .context("A project settings draft value has the wrong type")?;
    runner::validate_token_limits(config.context_tokens, config.output_tokens)?;
    anyhow::ensure!(
        (1..=100).contains(&config.implementation_calls),
        "implementation_calls must be 1..100"
    );
    anyhow::ensure!(config.retry_seconds > 0, "retry_seconds must be positive");
    anyhow::ensure!(
        config.request_timeout_seconds <= 31_536_000 && config.run_duration_seconds <= 31_536_000,
        "Choose 0 (unlimited) or a duration no longer than one year"
    );
    anyhow::ensure!(
        (1..=86400).contains(&config.command_review_seconds),
        "Command review interval must be 1–86400 seconds"
    );
    for (label, name) in [("Model", &config.model), ("Chat model", &config.chat_model)] {
        anyhow::ensure!(
            !name.chars().any(char::is_whitespace),
            "{label} must be an exact model name without spaces"
        );
    }
    let url = reqwest::Url::parse(&config.ollama_url).context("Enter a valid Ollama server URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Ollama server must be an HTTP or HTTPS URL"
    );
    config.helpers.validate()?;
    if let crate::schedule::Schedule::Custom { window } = &config.active_hours {
        window.validate()?;
    }
    Ok(config)
}

/// Preview project options before its goal and checks have been accepted.
/// This deliberately does not create a runnable configuration or controller.
pub fn draft_config(root: &Path) -> Result<runner::Config> {
    draft_config_with(root, &settings()?, &draft_settings(root)?)
}

fn save_draft_patch_with(root: &Path, patch: Value, defaults: &Settings) -> Result<()> {
    anyhow::ensure!(
        !root.join("chuggin.json").exists(),
        "This project is already set up; change its project settings instead"
    );
    let fields = patch
        .as_object()
        .context("Project settings patch must be an object")?;
    let mut draft = draft_settings(root)?;
    for (key, value) in fields {
        anyhow::ensure!(
            DRAFT_SETTING_FIELDS.contains(&key.as_str()),
            "Unknown project setting: {key}"
        );
        draft[key] = value.clone();
    }
    draft_config_with(root, defaults, &draft)?;
    save(&draft_settings_path(root), &draft)
}

/// Persist only explicitly chosen options; remaining options follow global defaults.
pub fn save_draft_patch(root: &Path, patch: Value) -> Result<()> {
    save_draft_patch_with(root, patch, &settings()?)
}

pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(p)?;
    }
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".chuggin-save-")
        .tempfile_in(parent)?;
    tmp.write_all(&serde_json::to_vec_pretty(value)?)?;
    tmp.persist(path)?;
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
fn token_count(value: u32) -> String {
    let digits = value.to_string();
    digits
        .chars()
        .enumerate()
        .fold(String::new(), |mut text, (index, ch)| {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                text.push(',');
            }
            text.push(ch);
            text
        })
}
fn token_selector(
    label: &str,
    current: u32,
    min: u32,
    max: u32,
    context: bool,
) -> Result<Option<u32>> {
    if !io::stdin().is_terminal() {
        return number(label, current, min, max).map(Some);
    }
    let presets: &[u32] = if context {
        &[
            8192, 16384, 32768, 65536, 128000, 262144, 524288, 1_000_000, 1_048_576,
        ]
    } else {
        &[
            256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 128000,
        ]
    };
    let values: Vec<u32> = presets
        .iter()
        .copied()
        .filter(|v| (min..=max).contains(v))
        .collect();
    let mut rows: Vec<String> = values
        .iter()
        .map(|v| {
            format!(
                "{} tokens{}",
                token_count(*v),
                if *v == current { " · current" } else { "" }
            )
        })
        .collect();
    rows.push(format!(
        "Custom… · current: {} tokens",
        token_count(current)
    ));
    let selected = values
        .iter()
        .position(|v| *v == current)
        .unwrap_or(values.len());
    let Some(index) = crate::menu::select(label, &rows, selected)? else {
        return Ok(None);
    };
    if let Some(value) = values.get(index) {
        return Ok(Some(*value));
    }
    number(&format!("{label} · custom token count"), current, min, max).map(Some)
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
    crate::ui::clear_notes();
    crate::ui::notice(
        "Chuggin · Global settings\nThese defaults apply to every project unless overridden."
            .to_string(),
    );
    if io::stdin().is_terminal() {
        let names = select_provider_models(&s)?;
        let selected = names.iter().position(|n| n == &s.model).unwrap_or(0);
        let Some(index) = crate::menu::select("Default model", &names, selected)? else {
            anyhow::bail!("Setup cancelled");
        };
        s.model = names[index].clone();
        s.ollama_url = settings()?.ollama_url;
    } else {
        s.ollama_url = ask("Ollama server", &s.ollama_url)?
            .trim_end_matches('/')
            .into();
        let names = available_models(&s)?;
        let chosen = ask("Model name or number", &s.model)?;
        s.model = chosen
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| names.get(n))
            .cloned()
            .unwrap_or(chosen);
        anyhow::ensure!(names.contains(&s.model), "Choose an installed Ollama model");
    }
    s.context_tokens = token_selector(
        "Default context window",
        s.context_tokens,
        runner::MIN_CONTEXT_TOKENS,
        u32::MAX,
        true,
    )?
    .context("Setup cancelled")?;
    s.output_tokens = token_selector(
        "Default response limit",
        s.output_tokens.min(s.context_tokens / 4),
        256,
        s.context_tokens - 2048,
        false,
    )?
    .context("Setup cancelled")?;
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
        let config = p.join("chuggin.json");
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
pub fn ensure_git_identity(root: &Path) -> Result<()> {
    for (key, label) in [
        ("user.name", "Git author name"),
        ("user.email", "Git author email"),
    ] {
        if project::git(root, &["config", "--get", key]).is_ok_and(|value| !value.trim().is_empty())
        {
            continue;
        }
        anyhow::ensure!(
            io::stdin().is_terminal() || crate::ui::active(),
            "Git identity is missing {key}. Open chuggin interactively to set it up, or set git config --global {key} before continuing. Use an email associated with GitHub if you want GitHub attribution."
        );
        crate::ui::clear_notes();
        let guidance = if key == "user.name" {
            "Enter the author name to use on this project's Git commits."
        } else {
            "Enter the email to use on this project's Git commits. A GitHub verified or noreply email enables GitHub attribution."
        };
        crate::ui::notice(format!(
            "Git needs {key} before Chuggin can continue. {guidance} This setting will be saved for this project."
        ));
        let value = ask(label, "")?;
        // A new project may not have a repository yet. Initialize it only after
        // the operator has supplied an identity, without creating a commit.
        if project::git(root, &["rev-parse", "--git-dir"]).is_err() {
            project::git(root, &["init"])?;
        }
        project::git(root, &["config", "--local", key, &value])?;
    }
    crate::ui::clear_notes();
    Ok(())
}

pub fn wizard() -> Result<PathBuf> {
    let root = std::env::current_dir()?;
    ensure_git_identity(&root)?;
    let config = root.join("chuggin.json");
    anyhow::ensure!(
        !config.exists(),
        "Project already configured; run chuggin to resume."
    );
    if draft_config(&root)?.model.trim().is_empty() {
        if io::stdin().is_terminal() {
            choose_project_model(None)?;
        } else {
            configure(false)?;
        }
    }
    let s = draft_config(&root)?;
    crate::ui::clear_notes();
    crate::ui::notice(format!(
        "Give a short description of this project: what you want to accomplish, any important requirements, and what a successful result would look like. Chuggin will draft a goal for you to review before work starts.\n\nProject: {}\nGoal drafting model: {} on {} (change from Project model on the home menu).",
        root.display(),
        s.model,
        crate::cloud::label(&s.ollama_url, &s.model)
    ));
    let draft_path = root.join(".chuggin/goal-draft.json");
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
            crate::ui::clear_notes();
            crate::ui::notice("Drafting your project goal…".to_string());
            let current = draft_config(&root)?;
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
                "Help define a project goal for software, documents, data, or other artifacts as appropriate to the user’s pitch. Return JSON {goal: string}. Expand the user's pitch into a clear goal with concrete capabilities, quality expectations, constraints, and observable success criteria. Preserve the user's ambition and explicit version/feature targets; do not silently narrow them to an MVP. Do not invent user requirements: label assumptions and leave genuinely unspecified choices flexible. Incorporate requested revisions. Keep under 700 words. This is goal drafting only, not implementation.",
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
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "Proposed project goal\n\n{}\n\nAccept this goal, request changes, or return to the menu with the draft saved. Work starts when you later select Resume project.",
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
                Some(1) => {
                    crate::ui::clear_notes();
                    crate::ui::notice(format!(
                        "Describe what should change in this goal: add requirements, correct assumptions, or explain what you want to keep.\n\nCurrent goal\n{}",
                        draft.goal
                    ));
                    ask("What should change?", "")?
                }
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
    crate::ui::clear_notes();
    crate::ui::notice("Choose a check Chuggin can use to guide refinement. All work is saved even when checks fail.\nChoose validation appropriate to this project: tests, a document linter, a data checker, or your own validation script.\nCommands support quoted arguments; shell operators are not interpreted.".to_string());
    let check = loop {
        let default_check = if root.join("Cargo.toml").is_file() {
            "cargo test"
        } else {
            ""
        };
        let text = ask("Validation command", default_check)?;
        match shell_words::split(&text) {
            Ok(argv) if !argv.is_empty() => break argv,
            _ => crate::ui::notice("Enter a valid command with balanced quotes.".to_string()),
        }
    };
    crate::ui::clear_notes();
    crate::ui::notice("Choose when Chuggin first checks a running command, in seconds. This sets the first review point for commands and validation checks; the watchdog can inspect work that takes longer.".into());
    let timeout = number(
        "First command review (seconds)",
        draft_config(&root)?.command_review_seconds as u32,
        1,
        86400,
    )?;
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
    for entry in ["/.chuggin/", "/chuggin.json"] {
        if !ignored.lines().any(|line| line == entry) {
            ignored.push_str(&format!("\n{entry}\n"));
        }
    }
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(exclude, ignored)?;
    if project::git(&root, &["rev-parse", "HEAD"]).is_err() {
        let scope = [".", ":(exclude).chuggin", ":(exclude)chuggin.json"];
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
            crate::ui::clear_notes();
            crate::ui::notice("\nThis project has no commits yet. Chuggin can commit all project files as its starting point. Git-ignored files and Chuggin's local state are excluded.\n".to_string());
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
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "Initialize Chuggin project",
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
    let mut accepted = json!({"repo":".","goal":draft.goal,"state_dir":".chuggin","helpers":draft_config(&root)?.helpers,"active_hours":{"mode":"shared"},
        "command_review_seconds":timeout,"checks":[{"argv":check,"timeout_seconds":timeout}]});
    for (key, value) in draft_settings(&root)?.as_object().unwrap() {
        accepted[key] = value.clone();
    }
    // The validation prompt is the final choice for the command review interval.
    accepted["command_review_seconds"] = json!(timeout);
    save(&config, &accepted)?;
    // Check merged project/global settings before starting.
    runner::load(&config)?;
    let preferences = draft_settings_path(&root);
    if preferences.exists() {
        fs::remove_file(preferences)?;
    }
    crate::ui::clear_notes();
    crate::ui::notice(
        "\nGoal accepted and saved. Chuggin will edit this folder on its current branch, keep recovery saves, and commit completed tasks. Existing staged changes are preserved. Select Resume project to start.".to_string(),
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
pub fn choose_model(project: Option<&Path>) -> Result<()> {
    let mut s = settings()?;
    if let Some(path) = project {
        let effective = runner::load(path)?;
        s.ollama_url = effective.ollama_url;
        s.model = effective.model;
    }
    let names = select_provider_models(&s)?;
    let selected = names.iter().position(|n| n == &s.model).unwrap_or(0);
    let title = if project.is_some() {
        "Choose model for this project"
    } else {
        "Choose default model · global"
    };
    if let Some(index) = crate::menu::select(title, &names, selected)? {
        s.model = names[index].clone();
        if let Some(path) = project {
            if runner::needs_migration(path)? {
                let mut config: Value = serde_json::from_slice(&fs::read(path)?)?;
                config["model"] = json!(s.model);
                save(path, &config)?;
            } else {
                save_live_setting(path, 0, &s.model)?;
            }
        } else {
            save(&settings_path()?, &s)?;
        }
        crate::ui::notice(format!("Model saved: {}", s.model));
    }
    Ok(())
}
pub fn choose_project_model(project: Option<&Path>) -> Result<()> {
    if project.is_some() {
        return choose_model(project);
    }
    let root = std::env::current_dir()?;
    let effective = draft_config(&root)?;
    let mut connection = settings()?;
    connection.model = effective.model;
    connection.ollama_url = effective.ollama_url;
    let names = select_provider_models(&connection)?;
    let selected = names
        .iter()
        .position(|name| name == &connection.model)
        .unwrap_or(0);
    if let Some(index) = crate::menu::select("Choose model for this project", &names, selected)? {
        save_draft_patch(&root, json!({"model": names[index]}))?;
        crate::ui::notice(format!("Project model saved: {}", names[index]));
    }
    Ok(())
}

fn editing_config(project: Option<&Path>) -> Result<runner::Config> {
    match project {
        Some(path) => runner::load(path),
        None => draft_config(&std::env::current_dir()?),
    }
}

fn save_project_patch(project: Option<&Path>, patch: Value) -> Result<()> {
    let Some(path) = project else {
        return save_draft_patch(&std::env::current_dir()?, patch);
    };
    let client = crate::engine::Client::connect(path)?;
    let session = client.open_session(None)?;
    let current = client.call(&session, "get_settings", json!({}), &crate::operator::id())?;
    let reply = client.call(
        &session,
        "update_settings",
        json!({"expected_revision":current["result"]["revision"],"settings":patch}),
        &crate::operator::id(),
    )?;
    anyhow::ensure!(reply["status"] == "complete", "{}", reply["error"]);
    Ok(())
}
pub fn settings_menu() -> Result<()> {
    loop {
        let mut s = settings()?;
        crate::ui::clear_notes();
        crate::ui::notice("Global connections are shared across projects. These model and working defaults apply where a project has no override; changing them preserves existing project overrides.".into());
        let items = vec![
            format!("Ollama server     {}", s.ollama_url),
            format!(
                "Default model     {}",
                if s.model.is_empty() {
                    "Not selected"
                } else {
                    &s.model
                }
            ),
            format!("Default context window    {}", s.context_tokens),
            format!("Default response limit    {}", s.output_tokens),
            format!("Default task responses    {}", s.implementation_calls),
            format!("Default retry delay       {} seconds", s.retry_seconds),
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
            "Default active hours".into(),
            "Groq connection and limits".into(),
            "Investigation helpers · defaults for new projects".into(),
            "OpenRouter · free cloud models and connection".into(),
            "Back".into(),
        ];
        let Some(index) =
            crate::menu::select("Global settings · connections and defaults", &items, 0)?
        else {
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
                choose_model(None)?;
                continue;
            }
            2 => {
                let Some(tokens) = token_selector(
                    "Default context window · global",
                    s.context_tokens,
                    runner::MIN_CONTEXT_TOKENS,
                    u32::MAX,
                    true,
                )?
                else {
                    continue;
                };
                s.context_tokens = tokens;
                s.output_tokens = s.output_tokens.min(s.context_tokens - 2048);
            }
            3 => {
                let Some(tokens) = token_selector(
                    "Default response limit · global",
                    s.output_tokens,
                    256,
                    s.context_tokens - 2048,
                    false,
                )?
                else {
                    continue;
                };
                s.output_tokens = tokens;
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
            9 => {
                active_hours_menu(None)?;
                continue;
            }
            10 => {
                groq_settings()?;
                continue;
            }
            11 => {
                helpers_menu(None)?;
                continue;
            }
            12 => {
                openrouter_settings()?;
                continue;
            }
            _ => return Ok(()),
        }
        save(&settings_path()?, &s)?;
    }
}

pub fn run_duration(path: &Path) -> Result<()> {
    project_run_duration(Some(path))
}
pub fn draft_run_duration() -> Result<()> {
    project_run_duration(None)
}
fn project_run_duration(project: Option<&Path>) -> Result<()> {
    let seconds = editing_config(project)?.run_duration_seconds;
    crate::ui::notice("Set the duration of each run in hours (0 means unlimited). The current cycle finishes before stopping. Resuming starts a new timer. This setting applies only to this project.".into());
    loop {
        let text = ask(
            "Run duration (hours)",
            &format!("{}", seconds as f64 / 3600.0),
        )?;
        if let Ok(hours) = text.parse::<f64>()
            && hours.is_finite()
            && (0.0..=8760.0).contains(&hours)
            && (hours == 0.0 || hours * 3600.0 >= 1.0)
        {
            save_project_setting(project, 2, &hours.to_string())?;
            return Ok(());
        }
        crate::ui::notice(
            "Enter 0 for unlimited, or a duration between one second and 8760 hours.".into(),
        );
    }
}

pub fn save_live_setting(path: &Path, field: usize, input: &str) -> Result<()> {
    save_project_setting(Some(path), field, input)
}
fn save_project_setting(project: Option<&Path>, field: usize, input: &str) -> Result<()> {
    let Some(path) = project else {
        let config = editing_config(None)?;
        return save_project_patch(None, setting_patch(&config, field, input)?);
    };
    let client = crate::engine::Client::connect(path)?;
    let session = client.open_session(None)?;
    let current = client.call(&session, "get_settings", json!({}), &crate::operator::id())?;
    let config: runner::Config = serde_json::from_value(current["result"]["settings"].clone())?;
    let patch = setting_patch(&config, field, input)?;
    let reply = client.call(
        &session,
        "update_settings",
        json!({"expected_revision":current["result"]["revision"],"settings":patch}),
        &crate::operator::id(),
    )?;
    anyhow::ensure!(reply["status"] == "complete", "{}", reply["error"]);
    Ok(())
}
fn setting_patch(current: &runner::Config, field: usize, input: &str) -> Result<Value> {
    let mut config = json!({});
    match field {
        0 => {
            let name = input.trim();
            anyhow::ensure!(
                !name.is_empty() && !name.chars().any(char::is_whitespace),
                "Enter an exact model name without spaces"
            );
            config["model"] = json!(name);
        }
        1 | 2 => {
            let value: f64 = input
                .trim()
                .parse()
                .context("Enter a number; 0 means unlimited")?;
            let seconds = value * if field == 1 { 60.0 } else { 3600.0 };
            anyhow::ensure!(
                seconds.is_finite()
                    && (0.0..=31536000.0).contains(&seconds)
                    && (seconds == 0.0 || seconds >= 1.0),
                "Choose 0 (unlimited) or between one second and one year"
            );
            config[if field == 1 {
                "request_timeout_seconds"
            } else {
                "run_duration_seconds"
            }] = json!(seconds.round() as u64);
        }
        3 => {
            let seconds: u64 = input.trim().parse().context("Enter whole seconds")?;
            anyhow::ensure!((1..=86400).contains(&seconds), "Choose 1–86400 seconds");
            config["command_review_seconds"] = json!(seconds);
        }
        4 => {
            config["allow_goal_completion"] = json!(match input.trim().to_lowercase().as_str() {
                "true" | "yes" | "on" => true,
                "false" | "no" | "off" => false,
                _ => anyhow::bail!("Enter on or off"),
            });
        }
        5 => {
            let context: u32 = input.trim().parse().context("Enter a whole token count")?;
            anyhow::ensure!(
                context >= runner::MIN_CONTEXT_TOKENS,
                "Context must be at least {} tokens",
                runner::MIN_CONTEXT_TOKENS
            );
            config["context_tokens"] = json!(context);
            // Keep a valid response allowance when reducing the context window.
            config["output_tokens"] = json!(current.output_tokens.min(context - 2048));
        }
        6 => {
            let output: u32 = input.trim().parse().context("Enter a whole token count")?;
            runner::validate_token_limits(current.context_tokens, output)?;
            config["output_tokens"] = json!(output);
        }
        _ => anyhow::bail!("Unknown project setting"),
    }
    Ok(config)
}

fn draft_recovery(root: &Path, detail: &str) -> Result<()> {
    let log = root.join(".chuggin/goal-error.txt");
    fs::write(&log, detail)?;
    crate::ui::clear_notes();
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
            Some(1) => project_settings_menu(None)?,
            _ => anyhow::bail!("Draft saved. Return to Setup to continue."),
        }
    } else {
        ask("Press Enter to retry, or Ctrl-C to exit", "retry")?;
    }
    Ok(())
}

pub fn active_hours_menu(project: Option<&Path>) -> Result<()> {
    active_hours_scope(project, false)
}
fn project_active_hours_menu(project: Option<&Path>) -> Result<()> {
    active_hours_scope(project, true)
}
fn active_hours_scope(project: Option<&Path>, project_scope: bool) -> Result<()> {
    use crate::schedule::{Closing, Schedule, Window};
    let current = if project.is_some() || project_scope {
        editing_config(project)?.active_hours
    } else {
        settings()?.active_hours
    };
    crate::ui::clear_notes();
    let current_description = match &current {
        Schedule::Always => "Always allowed".to_owned(),
        Schedule::Shared => "Use shared active hours".to_owned(),
        Schedule::Custom { window } => format!(
            "{}–{} · {} · {}",
            window.start,
            window.end,
            window.timezone,
            if window.closing == Closing::Cycle {
                "finish current cycle"
            } else {
                "finish current call"
            },
        ),
    };
    crate::ui::notice(format!(
        "Current setting: {current_description}\n\nActive hours control the loop. Chat remains available, and you can manually resume outside the schedule."
    ));
    let selected = match &current {
        Schedule::Always => 0,
        Schedule::Custom { .. } => 1,
        Schedule::Shared => 2,
    };
    let mut choices = vec!["Always allowed".into(), "Set active hours…".into()];
    if project.is_some() || project_scope {
        choices.push("Use shared active hours".into());
    }
    let Some(choice) = crate::ui::select(
        "Active hours · controls the loop; chat remains available",
        &choices,
        selected,
    )?
    else {
        return Ok(());
    };
    let schedule = match choice {
        0 => Schedule::Always,
        2 => Schedule::Shared,
        _ => {
            let mut w = if let Schedule::Custom { window } = current {
                window
            } else {
                Window::default()
            };
            w.start = ask("Loop may start at (HH:MM)", &w.start)?;
            w.end = ask("Loop pauses at (HH:MM)", &w.end)?;
            w.timezone = ask("Time zone", &w.timezone)?;
            let days = ask(
                "Opening days: Mon Tue Wed Thu Fri Sat Sun",
                &w.days
                    .iter()
                    .map(|i| ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][*i as usize])
                    .collect::<Vec<_>>()
                    .join(" "),
            )?;
            w.days = days
                .split_whitespace()
                .map(|d| {
                    ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                        .iter()
                        .position(|n| *n == d.to_lowercase())
                        .map(|i| i as u32)
                        .context("Use weekday names, for example Mon Tue Wed Thu Fri")
                })
                .collect::<Result<_>>()?;
            w.closing = match crate::ui::select(
                "At closing time",
                &[
                    "Finish current call (recommended) · started commands may finish".into(),
                    "Finish current cycle · can run past closing time".into(),
                ],
                usize::from(w.closing == Closing::Cycle),
            )? {
                Some(0) => Closing::Call,
                Some(_) => Closing::Cycle,
                None => return Ok(()),
            };
            w.validate()?;
            Schedule::Custom { window: w }
        }
    };
    if project.is_some() || project_scope {
        save_project_patch(project, json!({"active_hours":schedule}))?;
    } else {
        let mut s = settings()?;
        s.active_hours = schedule;
        save(&settings_path()?, &s)?;
    }
    Ok(())
}
pub fn external_control_info(path: &Path) -> Result<()> {
    let config = json!({"mcpServers":{"chuggin":{"command":std::env::current_exe()?,"args":["mcp","--project",fs::canonicalize(path)?]}}});
    crate::ui::show(
        "External AI access · MCP",
        &format!(
            "Add this local server to your AI application's MCP settings. Connecting doesn't start the loop.\n\n{config:#}\n\nOpen an operator session to inspect or control this project. The same tools are available in Chuggin Chat.\n\nIf the external AI uses its own file or shell tools, it must acquire begin_edit first, finish its background writers, then end_edit. Chuggin cannot coordinate edits that bypass this protocol.\n\nConnection and spending permissions remain in human settings."
        ),
    )
}
#[derive(Clone, Copy)]
pub enum ProjectSetting {
    Model,
    Context,
    Output,
    Timeout,
    Duration,
    ActiveHours,
    Completion,
    More,
    Global,
}
pub const PROJECT_SETTINGS: [ProjectSetting; 9] = [
    ProjectSetting::Model,
    ProjectSetting::Context,
    ProjectSetting::Output,
    ProjectSetting::Timeout,
    ProjectSetting::Duration,
    ProjectSetting::ActiveHours,
    ProjectSetting::Completion,
    ProjectSetting::More,
    ProjectSetting::Global,
];
impl ProjectSetting {
    pub fn label(self) -> &'static str {
        match self {
            Self::Model => "Project model",
            Self::Context => "Context window (tokens)",
            Self::Output => "Response limit (tokens)",
            Self::Timeout => "Request timeout (minutes; 0 unlimited)",
            Self::Duration => "Run duration (hours; 0 unlimited)",
            Self::ActiveHours => "Active hours",
            Self::Completion => "Allow goal completion (on/off)",
            Self::More => "More project settings…",
            Self::Global => "Global settings…",
        }
    }
    pub fn value(self, c: &runner::Config) -> String {
        match self {
            Self::Model => c.model.clone(),
            Self::Context => token_count(c.context_tokens),
            Self::Output => token_count(c.output_tokens),
            Self::Timeout => (c.request_timeout_seconds as f64 / 60.0).to_string(),
            Self::Duration => (c.run_duration_seconds as f64 / 3600.0).to_string(),
            Self::ActiveHours => c
                .active_hours
                .at(chrono::Utc::now())
                .map(|s| s.description)
                .unwrap_or_else(|e| e.to_string()),
            Self::Completion => if c.allow_goal_completion { "on" } else { "off" }.into(),
            Self::More => "Chat model, helpers, tool requests, command review, MCP".into(),
            Self::Global => "Shared connections and defaults for projects".into(),
        }
    }
    pub fn field(self) -> Option<usize> {
        match self {
            Self::Model => Some(0),
            Self::Timeout => Some(1),
            Self::Duration => Some(2),
            Self::Completion => Some(4),
            Self::Context => Some(5),
            Self::Output => Some(6),
            _ => None,
        }
    }
}
pub fn project_settings_menu(path: Option<&Path>) -> Result<()> {
    loop {
        let c = editing_config(path)?;
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "These settings apply only to {} and are saved automatically. Global connections and defaults have their own submenu.",
            c.repo.display()
        ));
        let mut rows: Vec<String> = PROJECT_SETTINGS
            .iter()
            .map(|s| format!("{}: {}", s.label(), s.value(&c)))
            .collect();
        rows.push("Back".into());
        let Some(index) = crate::menu::select("Project settings", &rows, 0)? else {
            return Ok(());
        };
        let Some(setting) = PROJECT_SETTINGS.get(index).copied() else {
            return Ok(());
        };
        match setting {
            ProjectSetting::Model => choose_project_model(path)?,
            ProjectSetting::ActiveHours => project_active_hours_menu(path)?,
            ProjectSetting::More => more_project_settings(path)?,
            ProjectSetting::Global => settings_menu()?,
            ProjectSetting::Context | ProjectSetting::Output => edit_project_tokens(path, setting)?,
            _ => {
                let value = ask(setting.label(), &setting.value(&c))?;
                save_project_setting(path, setting.field().unwrap(), &value)?;
            }
        }
    }
}
pub fn edit_project_tokens(path: Option<&Path>, setting: ProjectSetting) -> Result<()> {
    let c = editing_config(path)?;
    let (label, current, min, max, context) = match setting {
        ProjectSetting::Context => (
            "Context window · this project",
            c.context_tokens,
            runner::MIN_CONTEXT_TOKENS,
            u32::MAX,
            true,
        ),
        ProjectSetting::Output => (
            "Response limit · this project",
            c.output_tokens,
            1,
            c.context_tokens - 1,
            false,
        ),
        _ => anyhow::bail!("Not a token setting"),
    };
    if let Some(tokens) = token_selector(label, current, min, max, context)? {
        save_project_setting(path, setting.field().unwrap(), &tokens.to_string())?;
    }
    Ok(())
}
pub fn more_project_settings(path: Option<&Path>) -> Result<()> {
    loop {
        let c = editing_config(path)?;
        crate::ui::clear_notes();
        crate::ui::notice("These options apply only to this project. You can save them before setting up its goal.".into());
        let Some(choice) = crate::ui::select(
            "More project settings · only this project",
            &[
                format!(
                    "Command first review · {} seconds",
                    c.command_review_seconds
                ),
                format!(
                    "Chat model · {}",
                    if c.chat_model.is_empty() {
                        "Use project model"
                    } else {
                        &c.chat_model
                    }
                ),
                format!(
                    "Investigation helpers · {}",
                    if c.helpers.enabled {
                        "Enabled"
                    } else {
                        "Disabled"
                    }
                ),
                "External AI access (MCP)".into(),
                "Tool requests".into(),
                "Back".into(),
            ],
            0,
        )?
        else {
            return Ok(());
        };
        match choice {
            0 => {
                let value = ask(
                    "Command first review (seconds)",
                    &c.command_review_seconds.to_string(),
                )?;
                save_project_setting(path, 3, &value)?;
            }
            1 => {
                let model = ask(
                    "Chat model (enter 'default' to use project model)",
                    if c.chat_model.is_empty() {
                        "default"
                    } else {
                        &c.chat_model
                    },
                )?;
                save_project_patch(
                    path,
                    json!({"chat_model":if model=="default"{""}else{&model}}),
                )?;
            }
            2 => {
                helpers_scope(path, true)?;
            }
            3 => {
                if let Some(path) = path {
                    external_control_info(path)?;
                } else {
                    crate::ui::show(
                        "External AI access (MCP)",
                        "Set up this project's goal first. You can then connect another AI application to its tools and loop controls.",
                    )?;
                }
            }
            4 => {
                if let Some(path) = path {
                    crate::tool_requests::menu(path)?;
                } else {
                    crate::ui::show(
                        "Tool requests",
                        "No missing capabilities have been requested. Set up this project and start its loop to receive requests from the model.",
                    )?;
                }
            }
            _ => return Ok(()),
        }
    }
}

fn helper_rows(helpers: &crate::agents::Settings) -> Vec<String> {
    vec![
        format!(
            "Helpers · {}",
            if helpers.enabled {
                "Enabled"
            } else {
                "Disabled"
            }
        ),
        format!(
            "Model · {}",
            if helpers.model.is_empty() {
                "Use project model"
            } else {
                &helpers.model
            }
        ),
        format!("Model responses per helper · {}", helpers.max_calls),
        "Back".into(),
    ]
}

pub fn helpers_menu(project: Option<&Path>) -> Result<()> {
    helpers_scope(project, false)
}
fn helpers_scope(project: Option<&Path>, project_scope: bool) -> Result<()> {
    loop {
        let mut helpers = if project.is_some() || project_scope {
            editing_config(project)?.helpers
        } else {
            settings()?.helpers
        };
        let title = if project.is_some() || project_scope {
            "Investigation helpers · this project"
        } else {
            "Investigation helpers · defaults for new projects"
        };
        crate::ui::clear_notes();
        crate::ui::notice(
            "Optional helpers investigate a specific question with fresh context and return evidence to the main conversation. They can read files, search, inspect saved logs, and use enabled web tools; they cannot edit or run commands. The main model chooses when a helper is useful.\n\nUse project model shares its connection and budget. Ollama requests to the same server run one at a time. Cloud helpers use the selected provider's connection and limits. OpenCode Zen needs no account; OpenRouter needs a key. Free models can have capacity limits or temporary availability. Changes apply to the next helper; running helpers keep their model. The response allowance ends the investigation with its available findings, without rejecting the main work."
                .into(),
        );
        let Some(choice) = crate::menu::select(title, &helper_rows(&helpers), 0)? else {
            return Ok(());
        };
        match choice {
            0 => helpers.enabled = !helpers.enabled,
            1 => {
                crate::ui::clear_notes();
                let Some(route) = crate::menu::select(
                    "Helper model",
                    &[
                        "Use project model".into(),
                        "Choose a model and inference provider…".into(),
                        "Back".into(),
                    ],
                    usize::from(!helpers.model.is_empty()),
                )?
                else {
                    continue;
                };
                if route == 0 {
                    helpers.model.clear();
                } else if route == 1 {
                    let mut connection = settings()?;
                    if project.is_some() || project_scope {
                        let config = editing_config(project)?;
                        connection.model = config.model;
                        connection.ollama_url = config.ollama_url;
                    }
                    if !helpers.model.is_empty() {
                        connection.model = helpers.model.clone();
                    }
                    let names = select_provider_models(&connection)?;
                    let selected = names
                        .iter()
                        .position(|name| name == &connection.model)
                        .unwrap_or(0);
                    if let Some(index) =
                        crate::menu::select("Choose investigation model", &names, selected)?
                    {
                        helpers.model = names[index].clone();
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            2 => {
                helpers.max_calls = number(
                    "Model responses per helper (partial findings are kept)",
                    helpers.max_calls,
                    1,
                    1000,
                )?;
            }
            _ => return Ok(()),
        }
        if let Some(path) = project {
            let client = crate::engine::Client::connect(path)?;
            let session = client.open_session(None)?;
            let current =
                client.call(&session, "get_settings", json!({}), &crate::operator::id())?;
            let reply = client.update_helpers(&current["result"]["revision"], &helpers)?;
            anyhow::ensure!(reply["status"] == "complete", "{}", reply["error"]);
        } else if project_scope {
            save_project_patch(None, json!({"helpers":helpers}))?;
        } else {
            let mut defaults = settings()?;
            defaults.helpers = helpers;
            save(&settings_path()?, &defaults)?;
        }
    }
}

fn select_provider_models(s: &Settings) -> Result<Vec<String>> {
    crate::ui::clear_notes();
    crate::ui::notice("Choose an inference provider, then a model from its catalog. Loading the catalog does not generate a response. Shared connections can be configured in Global settings.".into());
    let selected = match crate::cloud::provider(&s.model) {
        Some(crate::cloud::Provider::Zen) => 2,
        Some(crate::cloud::Provider::OpenRouter) => 3,
        None => usize::from(crate::groq::model_id(&s.model).is_some()),
    };
    let choice = crate::menu::select(
        "Inference provider",
        &[
            format!("Ollama · {}", s.ollama_url),
            "Groq · cloud API".into(),
            "OpenCode Zen · free models, no account needed".into(),
            "OpenRouter · free models, API key needed".into(),
        ],
        selected,
    )?;
    match choice {
        Some(0) => {
            let mut connection = s.clone();
            if s.model.is_empty() {
                connection.ollama_url = ask("Ollama server", &s.ollama_url)?;
                let mut defaults = settings()?;
                defaults.ollama_url = connection.ollama_url.clone();
                save(&settings_path()?, &defaults)?;
            }
            crate::ui::busy("Loading Ollama models", move || {
                available_models(&connection)
            })
        }
        Some(1) => {
            if crate::groq::key().is_err() {
                groq_settings()?;
            }
            crate::ui::busy("Loading Groq models", crate::groq::models)
        }
        Some(2) => {
            crate::ui::clear_notes();
            crate::ui::notice("OpenCode Zen offers free models without an account. Some models are temporary previews; availability, capacity, and rate limits can change. Chuggin waits and retries provider errors, and never falls back to a paid model. Loading the catalog does not generate a response.".into());
            crate::ui::busy("Loading free OpenCode Zen models", || {
                crate::cloud::models(crate::cloud::Provider::Zen)
            })
        }
        Some(3) => {
            if crate::cloud::key("openrouter/connection-check")?.is_none() {
                openrouter_settings()?;
            }
            anyhow::ensure!(
                crate::cloud::key("openrouter/connection-check")?.is_some(),
                "Add an OpenRouter API key in Global settings before selecting its models"
            );
            crate::ui::clear_notes();
            crate::ui::notice("Only free OpenRouter models are offered here. Free-tier rate and capacity limits can require waiting; Chuggin retries without switching to a paid model. Your project context and response settings remain unchanged.".into());
            crate::ui::busy("Loading free OpenRouter models", || {
                crate::cloud::models(crate::cloud::Provider::OpenRouter)
            })
        }
        _ => anyhow::bail!("Model selection cancelled; settings unchanged"),
    }
}

pub fn openrouter_settings() -> Result<()> {
    loop {
        let key_set = crate::cloud::key("openrouter/connection-check")?.is_some();
        crate::ui::clear_notes();
        crate::ui::notice("This shared connection is used only when you select an openrouter/ model. Chuggin offers free models and does not switch to paid models. Free capacity and rate limits vary; exhausted capacity uses provider backoff. A catalog check lists models without generating a response or consuming an inference request.".into());
        let rows = vec![
            format!(
                "API key · {}",
                if key_set {
                    "Configured"
                } else {
                    "Not configured"
                }
            ),
            "Test catalog connection / list free models".into(),
            "Free providers and availability".into(),
            "Back".into(),
        ];
        match crate::menu::select("OpenRouter · shared free-model connection", &rows, 0)? {
            Some(0) => match crate::menu::select(
                "OpenRouter API key",
                &[
                    "Enter or replace key".into(),
                    "Remove key".into(),
                    "Back".into(),
                ],
                0,
            )? {
                Some(0) => {
                    let value = if crate::ui::active() {
                        crate::ui::ask_secret("OpenRouter API key · input is masked")?
                    } else {
                        dialoguer::Password::new()
                            .with_prompt("OpenRouter API key")
                            .interact()?
                    };
                    crate::web_tools::save_key(&crate::groq::path("openrouter.key")?, &value)?;
                }
                Some(1) => {
                    let path = crate::groq::path("openrouter.key")?;
                    if path.exists() {
                        fs::remove_file(path)?;
                    }
                }
                _ => {}
            },
            Some(1) => {
                let names = crate::ui::busy("Checking OpenRouter's free-model catalog", || {
                    crate::cloud::models(crate::cloud::Provider::OpenRouter)
                })?;
                crate::ui::show(
                    "OpenRouter · free models",
                    &format!(
                        "The catalog connection works. This check lists models; it does not generate a response or verify your inference allowance.\n\n{}",
                        names.join("\n")
                    ),
                )?;
            }
            Some(2) => {
                crate::ui::show(
                    "Free cloud providers",
                    "OpenCode Zen needs no account for the free models in its picker. Some entries are temporary previews and may disappear or reach capacity.\n\nOpenRouter requires your API key for inference. Its picker includes only tool-capable models with free pricing. Providers may restrict requests, daily usage, context, or simultaneous responses.\n\nChuggin keeps your selected model and retries capacity/rate errors with backoff. It never authorizes a paid fallback. Selecting a cloud model does not change your saved local context or response settings.",
                )?;
            }
            _ => return Ok(()),
        }
    }
}
pub fn groq_settings() -> Result<()> {
    loop {
        let mut limits = crate::groq::Limits::load()?;
        let key_set = crate::groq::key().is_ok();
        let rows = vec![
            format!(
                "API key · {}",
                if key_set {
                    "Configured"
                } else {
                    "Not configured"
                }
            ),
            "Test connection / list models".into(),
            format!("Requests per minute · {}", limits.requests_per_minute),
            format!("Requests per day · {}", limits.requests_per_day),
            format!("Tokens per minute · {}", limits.tokens_per_minute),
            format!("Tokens per day · {}", limits.tokens_per_day),
            format!(
                "Input tokens per minute · {} (0 = combined limit only)",
                limits.input_tokens_per_minute
            ),
            format!(
                "Output tokens per minute · {} (0 = combined limit only)",
                limits.output_tokens_per_minute
            ),
            "Explain budgets".into(),
            format!("Maximum response tokens · {}", limits.max_response_tokens),
            "Back".into(),
        ];
        match crate::menu::select("Groq · shared connection and budgets", &rows, 0)? {
            Some(0) => {
                let value = if crate::ui::active() { crate::ui::ask_secret("Groq API key · input is masked")? } else { dialoguer::Password::new().with_prompt("Groq API key").interact()? };
                crate::web_tools::save_key(&crate::groq::path("groq.key")?, &value)?;
            }
            Some(1) => {
                let models = crate::ui::busy("Testing Groq connection", crate::groq::models)?;
                crate::ui::notice(format!("Groq connection works. Available chat models:\n{}", models.join("\n")));
            }
            Some(i @ 2..=7) => {
                let fields = [&mut limits.requests_per_minute, &mut limits.requests_per_day, &mut limits.tokens_per_minute, &mut limits.tokens_per_day, &mut limits.input_tokens_per_minute, &mut limits.output_tokens_per_minute];
                let value = fields.into_iter().nth(i-2).unwrap();
                *value = number("Shared Groq limit", *value as u32, if i>=6 {0}else{1}, 1_000_000_000)? as u64;
                save(&crate::groq::path("groq-limits.json")?, &limits)?;
            }
            Some(9) => { limits.max_response_tokens = number("Maximum Groq response tokens", limits.max_response_tokens, 128, 32768)?; save(&crate::groq::path("groq-limits.json")?, &limits)?; }
            Some(8) => crate::ui::notice("Free-tier defaults leave headroom: 24 requests/minute, 900/day, 7,200 tokens/minute, 180,000/day. All Chuggin projects and chats on this machine share this budget. Usage survives restarts and model changes. Each request reserves estimated input plus maximum output; final usage replaces that reservation. Interrupted requests keep their reservation. Daily limits use a conservative rolling 24-hour window. Groq headers can impose stricter waits. Other apps using the account are only visible through those headers. Manual retry rechecks budgets and never bypasses them. Raise limits only to match your Groq account. Requests use a smaller working context when necessary; full request logs remain available. Groq responses default to a 1,024-token cap, adjustable here.".into()),
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod project_draft_tests {
    use super::*;

    #[test]
    fn draft_preferences_follow_defaults_without_becoming_a_configured_project() {
        let root = tempfile::tempdir().unwrap();
        let mut defaults = Settings {
            model: "shared-model".into(),
            context_tokens: 65536,
            output_tokens: 8192,
            ..Settings::default()
        };
        let preview = draft_config_with(root.path(), &defaults, &json!({})).unwrap();
        assert_eq!(preview.model, "shared-model");
        assert!(preview.goal.is_empty());
        assert!(preview.checks.is_empty());
        assert!(!root.path().join(".chuggin").exists());

        save_draft_patch_with(
            root.path(),
            json!({"model":"project-model","context_tokens":1_000_000,"run_duration_seconds":36000,
                "allow_goal_completion":true,"helpers":{"enabled":true,"max_calls":24}}),
            &defaults,
        )
        .unwrap();
        let saved = draft_settings(root.path()).unwrap();
        assert_eq!(saved["model"], "project-model");
        assert!(saved.get("output_tokens").is_none());
        defaults.model = "new-shared-model".into();
        defaults.output_tokens = 16384;
        let preview = draft_config_with(root.path(), &defaults, &saved).unwrap();
        assert_eq!(preview.model, "project-model");
        assert_eq!(preview.context_tokens, 1_000_000);
        assert_eq!(preview.output_tokens, 16384);
        assert_eq!(preview.run_duration_seconds, 36000);
        assert!(preview.allow_goal_completion);
        assert!(preview.helpers.enabled);
        assert_eq!(preview.helpers.max_calls, 24);
        assert!(preview.goal.is_empty());
        assert!(preview.checks.is_empty());
        for path in [
            "chuggin.json",
            ".git",
            ".chuggin/state.json",
            ".chuggin/operator",
        ] {
            assert!(!root.path().join(path).exists(), "Unexpected {path}");
        }
    }

    #[test]
    fn invalid_draft_patches_preserve_saved_options() {
        let root = tempfile::tempdir().unwrap();
        let defaults = Settings::default();
        save_draft_patch_with(root.path(), json!({"model":"project-model"}), &defaults).unwrap();
        let path = draft_settings_path(root.path());
        let before = fs::read(&path).unwrap();
        for patch in [
            json!({"goal":"Must not turn preferences into an accepted goal"}),
            json!({"model":"invalid model name"}),
            json!({"ollama_url":"not a URL"}),
            json!({"context_tokens":2048}),
            json!({"output_tokens":32768}),
            json!({"output_tokens":"8192"}),
            json!({"request_timeout_seconds":31_536_001}),
            json!({"command_review_seconds":0}),
            json!({"allow_goal_completion":"yes"}),
            json!({"helpers":{"max_calls":0}}),
            json!({"active_hours":{"mode":"custom","window":{"start":"01:00","end":"01:00","timezone":"UTC","days":[0]}}}),
        ] {
            assert!(save_draft_patch_with(root.path(), patch, &defaults).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        fs::write(root.path().join("chuggin.json"), "{}").unwrap();
        assert!(
            save_draft_patch_with(root.path(), json!({"model":"another-model"}), &defaults)
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
