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
    pub setup_model: String,
    pub reasoning: crate::reasoning::Setting,
    pub setup_reasoning: crate::reasoning::Setting,
    pub chat_reasoning: crate::reasoning::Setting,
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
            setup_model: String::new(),
            reasoning: crate::reasoning::Setting::default(),
            setup_reasoning: crate::reasoning::Setting::default(),
            chat_reasoning: crate::reasoning::Setting::default(),
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
    "setup_model",
    "reasoning",
    "setup_reasoning",
    "chat_reasoning",
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
    for (label, name) in [
        ("Loop model", &config.model),
        ("Setup model", &config.setup_model),
        ("Chat model", &config.chat_model),
    ] {
        anyhow::ensure!(
            !name.chars().any(char::is_whitespace),
            "{label} must be an exact model name without spaces"
        );
    }
    runner::validate_inference_connections(&config)?;
    config.helpers.validate()?;
    config.reasoning.validate()?;
    config.setup_reasoning.validate()?;
    config.chat_reasoning.validate()?;
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
        let Some(selection) =
            crate::model_picker::choose(&s.model, &s.ollama_url, "Choose default model · global")?
        else {
            return Ok(());
        };
        s = settings()?;
        s.model = selection.model;
        if let Some(url) = selection.local_url {
            s.ollama_url = url;
        }
    } else {
        s.model = ask(
            "Model ID (local name, groq/, chatgpt/, zen/, or openrouter/)",
            &s.model,
        )?;
        if crate::model_picker::Provider::for_model(&s.model)
            == Some(crate::model_picker::Provider::Ollama)
        {
            s.ollama_url = ask("Ollama server", &s.ollama_url)?
                .trim_end_matches('/')
                .into();
            let names = available_models(&s)?;
            anyhow::ensure!(names.contains(&s.model), "Choose an installed Ollama model");
        }
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
    let goal = if io::stdin().is_terminal() {
        crate::goal_setup::choose_goal(&root, &s)?
    } else {
        let setup_model = if s.setup_model.is_empty() {
            &s.model
        } else {
            &s.setup_model
        };
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "Give a short description of this project: what you want to accomplish, any important requirements, and what a successful result would look like. Chuggin will draft a goal for you to review before work starts.\n\nProject: {}\nGoal drafting model: {} on {} (change Setup model in More project settings).",
            root.display(),
            setup_model,
            crate::cloud::label(&s.ollama_url, setup_model)
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
                let setup_model = if current.setup_model.is_empty() {
                    &current.model
                } else {
                    &current.setup_model
                };
                let mut model = Model::new(
                    &current.ollama_url,
                    setup_model,
                    current.context_tokens,
                    current.output_tokens,
                    Arc::new(AtomicBool::new(false)),
                )?;
                model.set_reasoning(current.setup_reasoning.clone());
                model.set_request_timeout(current.request_timeout_seconds);
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
            let action = ask("Type accept, or describe the changes you want", "")?;
            if action.eq_ignore_ascii_case("accept") {
                break;
            }
            draft.feedback = action;
            save(&draft_path, &draft)?;
        }
        draft.goal
    };
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
    let preferences = draft_config(&root)?;
    let mut accepted = json!({"repo":".","goal":goal,"state_dir":".chuggin","helpers":preferences.helpers,"active_hours":{"mode":"shared"},
        "model":preferences.model,"setup_model":preferences.setup_model,
        "reasoning":preferences.reasoning,"setup_reasoning":preferences.setup_reasoning,"chat_reasoning":preferences.chat_reasoning,
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
    let defaults = settings()?;
    let (model, url) = if let Some(path) = project {
        let config = runner::load(path)?;
        (config.model, config.ollama_url)
    } else {
        (defaults.model, defaults.ollama_url)
    };
    let title = if project.is_some() {
        "Choose model for this project"
    } else {
        "Choose default model · global"
    };
    let Some(selection) = crate::model_picker::choose(&model, &url, title)? else {
        return Ok(());
    };
    if let Some(path) = project {
        if runner::needs_migration(path)? {
            let mut config: Value = serde_json::from_slice(&fs::read(path)?)?;
            for (key, value) in selection.patch("model").as_object().unwrap() {
                config[key] = value.clone();
            }
            save(path, &config)?;
        } else {
            save_project_patch(Some(path), selection.patch("model"))?;
        }
    } else {
        let mut defaults = settings()?;
        defaults.model = selection.model.clone();
        if let Some(url) = &selection.local_url {
            defaults.ollama_url = url.clone();
        }
        save(&settings_path()?, &defaults)?;
    }
    crate::ui::notice(format!("Model saved: {}", selection.model));
    Ok(())
}

pub fn choose_project_model(project: Option<&Path>) -> Result<()> {
    if project.is_some() {
        return choose_model(project);
    }
    let root = std::env::current_dir()?;
    let config = draft_config(&root)?;
    if let Some(selection) = crate::model_picker::choose(
        &config.model,
        &config.ollama_url,
        "Choose model for this project",
    )? {
        save_draft_patch(&root, selection.patch("model"))?;
        crate::ui::notice(format!("Project model saved: {}", selection.model));
    }
    Ok(())
}

fn role_model_choice(
    name: &str,
    loop_model: &str,
    url: &str,
    title: &str,
) -> Result<Option<crate::model_picker::Selection>> {
    crate::ui::clear_notes();
    crate::ui::notice(format!(
        "Choose an independent model for this role, or use the loop model ({loop_model}). This does not change the model used by project loops. Catalog requests do not generate model output."
    ));
    match crate::menu::select(
        title,
        &[
            "Use loop model".into(),
            "Choose a model and inference provider…".into(),
            "Back".into(),
        ],
        usize::from(!name.is_empty()),
    )? {
        Some(0) => Ok(Some(crate::model_picker::Selection {
            model: String::new(),
            local_url: None,
        })),
        Some(1) => {
            crate::model_picker::choose(if name.is_empty() { loop_model } else { name }, url, title)
        }
        _ => Ok(None),
    }
}

/// Setup may use a larger model without changing the project's looping model.
pub fn choose_setup_model(project: Option<&Path>) -> Result<()> {
    let config = editing_config(project)?;
    if let Some(selection) = role_model_choice(
        &config.setup_model,
        &config.model,
        &config.ollama_url,
        "Setup model · this project",
    )? {
        save_project_patch(project, selection.patch("setup_model"))?;
    }
    Ok(())
}

fn thinking_choice(
    current: &crate::reasoning::Setting,
    model: &str,
    url: &str,
    output_tokens: u32,
    title: &str,
) -> Result<Option<crate::reasoning::Setting>> {
    use crate::reasoning::Setting;
    crate::ui::clear_notes();
    let model_name = model.to_owned();
    let server = url.to_owned();
    let discovered = if model.is_empty() {
        Err(anyhow::anyhow!(
            "Select a model first to discover its thinking controls."
        ))
    } else {
        crate::ui::busy("Checking model thinking controls", move || {
            crate::reasoning::discover(&server, &model_name)
        })
    };
    let (mut options, budget, detail) = match &discovered {
        Ok(capabilities) => (
            capabilities.options.clone(),
            capabilities.supports_token_budget && output_tokens > 1,
            format!(
                "Model: {model}\nSource: {}\n{}{}",
                capabilities.source,
                capabilities.detail,
                capabilities
                    .default
                    .as_ref()
                    .map(|value| format!("\nProvider default: {value}"))
                    .unwrap_or_default()
            ),
        ),
        Err(error) => (
            vec![Setting::Default],
            false,
            format!(
                "Model: {model}\nThinking controls could not be verified: {error:#}\nModel default sends no thinking override and remains available."
            ),
        ),
    };
    if !options.iter().any(Setting::is_default) {
        options.insert(0, Setting::Default);
    }
    crate::ui::notice(format!(
        "{detail}\n\nCurrent: {}. Only confirmed controls are offered. Model default lets the provider choose; it does not force thinking off. Changes apply to the next model call.",
        current.label()
    ));
    let mut rows: Vec<String> = options.iter().map(Setting::label).collect();
    if budget {
        rows.push(format!(
            "Custom reasoning token budget… · response allowance: {} tokens",
            token_count(output_tokens)
        ));
    }
    rows.push("Back".into());
    let selected = options
        .iter()
        .position(|option| option == current)
        .unwrap_or({
            if budget && matches!(current, Setting::TokenBudget(_)) {
                options.len()
            } else {
                0
            }
        });
    let Some(choice) = crate::menu::select(title, &rows, selected)? else {
        return Ok(None);
    };
    let setting = if let Some(option) = options.get(choice) {
        option.clone()
    } else if budget && choice == options.len() {
        let default = match current {
            Setting::TokenBudget(tokens) => *tokens,
            _ => 4096.min(output_tokens / 2),
        };
        Setting::TokenBudget(number(
            "Reasoning token budget · must leave room for the answer",
            default.min(output_tokens - 1),
            1,
            output_tokens - 1,
        )?)
    } else {
        return Ok(None);
    };
    setting.validate()?;
    if let Ok(capabilities) = discovered {
        capabilities.validate_selection(&setting)?;
    }
    Ok(Some(setting))
}

pub fn choose_project_reasoning(project: Option<&Path>) -> Result<()> {
    let config = editing_config(project)?;
    if let Some(setting) = thinking_choice(
        &config.reasoning,
        &config.model,
        &config.ollama_url,
        config.output_tokens,
        "Loop thinking · this project",
    )? {
        save_project_patch(project, json!({"reasoning":setting}))?;
    }
    Ok(())
}

pub fn choose_setup_reasoning(project: Option<&Path>) -> Result<()> {
    let config = editing_config(project)?;
    let name = if config.setup_model.is_empty() {
        &config.model
    } else {
        &config.setup_model
    };
    if let Some(setting) = thinking_choice(
        &config.setup_reasoning,
        name,
        &config.ollama_url,
        config.output_tokens,
        "Setup thinking · this project",
    )? {
        save_project_patch(project, json!({"setup_reasoning":setting}))?;
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
    let client = crate::engine::Client::connect_interactive(path)?;
    let session = client.open_session(None)?;
    let current = client.call(&session, "get_settings", json!({}), &crate::operator::id())?;
    let reply = client.update_human_settings(&current["result"]["revision"], patch)?;
    anyhow::ensure!(reply["status"] == "complete", "{}", reply["error"]);
    Ok(())
}
pub fn settings_menu() -> Result<()> {
    loop {
        let mut s = settings()?;
        crate::ui::clear_notes();
        crate::ui::notice("Global connections are shared across projects. These model and working defaults apply where a project has no override; changing them preserves existing project overrides.".into());
        let items = vec![
            "Inference providers…".into(),
            format!(
                "Default loop model     {}",
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
            "Investigation helpers · defaults for new projects".into(),
            format!(
                "Default setup model     {}",
                if s.setup_model.is_empty() {
                    "Use loop model"
                } else {
                    &s.setup_model
                }
            ),
            format!("Default loop thinking     {}", s.reasoning.label()),
            format!("Default setup thinking    {}", s.setup_reasoning.label()),
            format!("Default chat thinking     {}", s.chat_reasoning.label()),
            "Back".into(),
        ];
        let Some(index) =
            crate::menu::select("Global settings · connections and defaults", &items, 0)?
        else {
            return Ok(());
        };
        match index {
            0 => {
                crate::model_picker::connections_menu()?;
                continue;
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
                helpers_menu(None)?;
                continue;
            }
            11 => {
                let Some(model) = role_model_choice(
                    &s.setup_model,
                    &s.model,
                    &s.ollama_url,
                    "Default setup model · global",
                )?
                else {
                    continue;
                };
                s = settings()?;
                s.setup_model = model.model;
                if let Some(url) = model.local_url {
                    s.ollama_url = url;
                }
            }
            12 => {
                let Some(setting) = thinking_choice(
                    &s.reasoning,
                    &s.model,
                    &s.ollama_url,
                    s.output_tokens,
                    "Default loop thinking · global",
                )?
                else {
                    continue;
                };
                s.reasoning = setting;
            }
            13 => {
                let model = if s.setup_model.is_empty() {
                    &s.model
                } else {
                    &s.setup_model
                };
                let Some(setting) = thinking_choice(
                    &s.setup_reasoning,
                    model,
                    &s.ollama_url,
                    s.output_tokens,
                    "Default setup thinking · global",
                )?
                else {
                    continue;
                };
                s.setup_reasoning = setting;
            }
            14 => {
                let Some(setting) = thinking_choice(
                    &s.chat_reasoning,
                    &s.model,
                    &s.ollama_url,
                    s.output_tokens,
                    "Default chat thinking · global",
                )?
                else {
                    continue;
                };
                s.chat_reasoning = setting;
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
    let client = crate::engine::Client::connect_interactive(path)?;
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
    Thinking,
    Context,
    Output,
    Timeout,
    Duration,
    ActiveHours,
    Completion,
    More,
    Global,
}
pub const PROJECT_SETTINGS: [ProjectSetting; 10] = [
    ProjectSetting::Model,
    ProjectSetting::Thinking,
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
            Self::Thinking => "Loop thinking",
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
            Self::Thinking => c.reasoning.label(),
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
            Self::More => "Setup and chat models, thinking, helpers, tool requests, MCP".into(),
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
            ProjectSetting::Thinking => choose_project_reasoning(path)?,
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
                    "Setup model · {}",
                    if c.setup_model.is_empty() {
                        "Use loop model"
                    } else {
                        &c.setup_model
                    }
                ),
                format!("Setup thinking · {}", c.setup_reasoning.label()),
                format!(
                    "Chat model · {}",
                    if c.chat_model.is_empty() {
                        "Use loop model"
                    } else {
                        &c.chat_model
                    }
                ),
                format!("Chat thinking · {}", c.chat_reasoning.label()),
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
                choose_setup_model(path)?;
            }
            2 => {
                choose_setup_reasoning(path)?;
            }
            3 => {
                if let Some(model) = role_model_choice(
                    &c.chat_model,
                    &c.model,
                    &c.ollama_url,
                    "Chat model · this project",
                )? {
                    save_project_patch(path, model.patch("chat_model"))?;
                }
            }
            4 => {
                let name = if c.chat_model.is_empty() {
                    &c.model
                } else {
                    &c.chat_model
                };
                if let Some(setting) = thinking_choice(
                    &c.chat_reasoning,
                    name,
                    &c.ollama_url,
                    c.output_tokens,
                    "Chat thinking · this project",
                )? {
                    save_project_patch(path, json!({"chat_reasoning":setting}))?;
                }
            }
            5 => helpers_scope(path, true)?,
            6 => {
                if let Some(path) = path {
                    external_control_info(path)?;
                } else {
                    crate::ui::show(
                        "External AI access (MCP)",
                        "Set up this project's goal first. You can then connect another AI application to its tools and loop controls.",
                    )?;
                }
            }
            7 => {
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
        format!("Thinking · {}", helpers.reasoning.label()),
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
            "Optional helpers investigate a specific question with fresh context and return evidence to the main conversation. They can read files, search, inspect saved logs, and use enabled web tools; they cannot edit or run commands. The main model chooses when a helper is useful.\n\nUse project model shares its connection and budget. Ollama requests to the same server run one at a time. Cloud helpers use the selected provider's connection and limits. OpenCode Zen needs no account; OpenRouter needs a key. Free models can have capacity limits or temporary availability. Model and response allowance changes apply to the next investigation; running helpers keep their model and allowance. Helper thinking changes apply to its next model call. The response allowance ends the investigation with its available findings, without rejecting the main work."
                .into(),
        );
        let Some(choice) = crate::menu::select(title, &helper_rows(&helpers), 0)? else {
            return Ok(());
        };
        let mut selected_local_url = None;
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
                    if let Some(selection) = crate::model_picker::choose(
                        &connection.model,
                        &connection.ollama_url,
                        "Choose investigation model",
                    )? {
                        helpers.model = selection.model;
                        selected_local_url = selection.local_url;
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
            3 => {
                let (model, url, output_tokens) = if project.is_some() || project_scope {
                    let config = editing_config(project)?;
                    (config.model, config.ollama_url, config.output_tokens)
                } else {
                    let defaults = settings()?;
                    (defaults.model, defaults.ollama_url, defaults.output_tokens)
                };
                let name = if helpers.model.is_empty() {
                    &model
                } else {
                    &helpers.model
                };
                let Some(setting) = thinking_choice(
                    &helpers.reasoning,
                    name,
                    &url,
                    output_tokens,
                    "Investigation helper thinking",
                )?
                else {
                    continue;
                };
                helpers.reasoning = setting;
            }
            _ => return Ok(()),
        }
        if project.is_some() || project_scope {
            let mut patch = json!({"helpers":helpers});
            if let Some(url) = selected_local_url {
                patch["ollama_url"] = json!(url);
            }
            save_project_patch(project, patch)?;
        } else {
            let mut defaults = settings()?;
            defaults.helpers = helpers;
            if let Some(url) = selected_local_url {
                defaults.ollama_url = url;
            }
            save(&settings_path()?, &defaults)?;
        }
    }
}

pub fn groq_limits_menu() -> Result<()> {
    loop {
        let mut limits = crate::groq::Limits::load()?;
        crate::ui::clear_notes();
        crate::ui::notice("These budgets are shared across all Groq connections on this machine. They do not change local or other cloud providers. Set them to match your account allowance.".into());
        let rows = vec![
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
        match crate::menu::select("Groq · shared usage budgets", &rows, 0)? {
            Some(i @ 0..=5) => {
                let fields = [
                    &mut limits.requests_per_minute,
                    &mut limits.requests_per_day,
                    &mut limits.tokens_per_minute,
                    &mut limits.tokens_per_day,
                    &mut limits.input_tokens_per_minute,
                    &mut limits.output_tokens_per_minute,
                ];
                let value = fields.into_iter().nth(i).unwrap();
                *value = number(
                    "Shared Groq limit",
                    *value as u32,
                    if i >= 4 { 0 } else { 1 },
                    1_000_000_000,
                )? as u64;
                save(&crate::groq::path("groq-limits.json")?, &limits)?;
            }
            Some(7) => {
                limits.max_response_tokens = number(
                    "Maximum Groq response tokens",
                    limits.max_response_tokens,
                    128,
                    32768,
                )?;
                save(&crate::groq::path("groq-limits.json")?, &limits)?;
            }
            Some(6) => crate::ui::show(
                "Groq usage budgets",
                "Free-tier defaults leave headroom: 24 requests/minute, 900/day, 7,200 tokens/minute, 180,000/day. All Chuggin projects and chats on this machine share this budget. Usage survives restarts and model changes. Each request reserves estimated input plus maximum output; final usage replaces that reservation. Interrupted requests keep their reservation. Daily limits use a conservative rolling 24-hour window. Groq headers can impose stricter waits. Other apps using the account are only visible through those headers. Manual retry rechecks budgets and never bypasses them. Raise limits only to match your Groq account. Requests use a smaller working context when necessary; full request logs remain available. Groq responses default to a 1,024-token cap, adjustable here.",
            )?,
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod project_draft_tests {
    use super::*;

    #[test]
    fn existing_settings_keep_provider_default_thinking_for_every_role() {
        let defaults: Settings = serde_json::from_value(json!({"model":"existing-model"})).unwrap();
        assert!(defaults.setup_model.is_empty());
        assert!(defaults.reasoning.is_default());
        assert!(defaults.setup_reasoning.is_default());
        assert!(defaults.chat_reasoning.is_default());
        assert!(defaults.helpers.reasoning.is_default());
    }

    #[test]
    fn independent_model_and_thinking_choices_survive_goal_setup_preferences() {
        use crate::reasoning::Setting;
        let root = tempfile::tempdir().unwrap();
        let defaults = Settings {
            model: "local-loop-model".into(),
            setup_model: "cloud-setup-model".into(),
            setup_reasoning: Setting::Effort("high".into()),
            ..Settings::default()
        };
        let inherited = draft_config_with(root.path(), &defaults, &json!({})).unwrap();
        assert_eq!(inherited.model, "local-loop-model");
        assert_eq!(inherited.setup_model, "cloud-setup-model");
        assert_eq!(inherited.setup_reasoning, Setting::Effort("high".into()));
        assert!(inherited.reasoning.is_default());
        save_draft_patch_with(
            root.path(),
            json!({
                "setup_model":"different-setup-model",
                "setup_reasoning":{"mode":"token_budget","value":8192},
                "reasoning":{"mode":"on"},
                "chat_reasoning":{"mode":"off"},
                "helpers":{"reasoning":{"mode":"effort","value":"low"}}
            }),
            &defaults,
        )
        .unwrap();
        let selected = draft_config_with(
            root.path(),
            &defaults,
            &draft_settings(root.path()).unwrap(),
        )
        .unwrap();
        assert_eq!(selected.model, "local-loop-model");
        assert_eq!(selected.setup_model, "different-setup-model");
        assert_eq!(selected.setup_reasoning, Setting::TokenBudget(8192));
        assert_eq!(selected.reasoning, Setting::On);
        assert_eq!(selected.chat_reasoning, Setting::Off);
        assert_eq!(selected.helpers.reasoning, Setting::Effort("low".into()));
        assert!(!root.path().join("chuggin.json").exists());
    }

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
            json!({"setup_model":"invalid model name"}),
            json!({"reasoning":{"mode":"effort","value":"invalid\neffort"}}),
            json!({"setup_reasoning":{"mode":"token_budget","value":0}}),
            json!({"chat_reasoning":{"mode":"unrecognized"}}),
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
            assert!(
                save_draft_patch_with(root.path(), patch.clone(), &defaults).is_err(),
                "Unexpectedly accepted {patch}"
            );
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
