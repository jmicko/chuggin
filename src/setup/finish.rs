//! Accepted content and final review are separate from option-page progress.
//! Going back or exiting never repeats goal inference or creates a project.
use super::*;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Content {
    pub goal: Option<String>,
    pub validation: Option<Vec<String>>,
}
fn path(root: &Path) -> PathBuf {
    root.join(".chuggin/setup-content.json")
}
impl Content {
    pub fn load(root: &Path) -> Result<Self> {
        match fs::read(path(root)) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Could not read saved setup content")
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, root: &Path) -> Result<()> {
        save(&path(root), self)
    }
}
pub(super) fn clear(root: &Path) -> Result<()> {
    match fs::remove_file(path(root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
pub(super) enum Action {
    Finish,
    Options,
    Goal,
    Validation,
    Exit,
}

/// Interactive setup pins the choices it displayed. Legacy stdin setup retains
/// its existing shared-default inheritance for scripts and saved configurations.
pub(super) fn configuration(
    c: &runner::Config,
    goal: &str,
    check: &[String],
    timeout: u32,
    draft: &Value,
    pin_defaults: bool,
) -> Result<Value> {
    let mut accepted = if pin_defaults {
        serde_json::to_value(c)?
    } else {
        json!({"helpers":c.helpers,"active_hours":{"mode":"shared"},
            "activity_mode":c.activity_mode,"execution":c.execution,"self_profile":c.self_profile,
            "sampling":c.sampling,"setup_sampling":c.setup_sampling,"chat_sampling":c.chat_sampling,
            "model":c.model,"setup_model":c.setup_model,"reasoning":c.reasoning,
            "setup_reasoning":c.setup_reasoning,"chat_reasoning":c.chat_reasoning})
    };
    accepted["repo"] = json!(".");
    accepted["goal"] = json!(goal);
    accepted["state_dir"] = json!(".chuggin");
    accepted["checks"] = if c.activity_mode.is_chaos() {
        json!([])
    } else {
        json!([{"argv":check,"timeout_seconds":timeout}])
    };
    for (key, value) in draft
        .as_object()
        .context("Setup preferences must be an object")?
    {
        accepted[key] = value.clone();
    }
    accepted["command_review_seconds"] = json!(timeout);
    if c.activity_mode.is_chaos() {
        accepted["allow_goal_completion"] = json!(false);
    }
    Ok(accepted)
}

pub(super) fn validation(root: &Path, current: &[String]) -> Result<Vec<String>> {
    crate::ui::clear_notes();
    crate::ui::notice("Choose a real validation command for this project. Chuggin keeps unfinished work even when checks fail. Use tests, a document linter, a data checker, or another check appropriate to the work. Commands support quoted arguments; shell operators are not interpreted. Nothing is run during this step.".into());
    let default = if !current.is_empty() {
        shell_words::join(current)
    } else if root.join("Cargo.toml").is_file() {
        "cargo test".into()
    } else {
        String::new()
    };
    loop {
        let text = ask("Validation command", &default)?;
        match shell_words::split(&text) {
            Ok(command) if !command.is_empty() => return Ok(command),
            _ => crate::ui::notice("Enter a valid command with balanced quotes.".into()),
        }
    }
}

pub(super) fn review(
    root: &Path,
    c: &runner::Config,
    goal: &str,
    check: &[String],
) -> Result<Action> {
    let mut choices = vec!["Finish setup".into(), "Back · change setup options".into()];
    if !c.activity_mode.is_chaos() {
        choices.extend(["Edit project goal".into(), "Edit validation command".into()]);
    }
    choices.push("Save and exit".into());
    let total = wizard::progress_total(root)? + if c.activity_mode.is_chaos() { 1 } else { 3 };
    let current = format!(
        "Project: {}\n{}\nModel: {}",
        root.display(),
        c.activity_mode.label(),
        crate::model_picker::display_model(&c.model)
    );
    let next = if c.activity_mode.is_chaos() {
        if c.execution.is_private() {
            "Finish saves this project and prepares its private computer. Inference starts only when you later choose Resume project."
        } else {
            "Finish saves this project. Inference starts only when you later choose Resume project."
        }
    } else {
        "Finish saves this project, checks your Git commit identity, and prepares its initial commit if needed. Inference starts only when you later choose Resume project."
    };
    let detail = format!("{next}\n\n{}", summary(c, goal, check));
    let action = crate::ui::wizard_step(
        "Review project setup",
        total,
        total,
        &current,
        &detail,
        &choices,
        0,
    )?;
    Ok(match action {
        Some(0) => Action::Finish,
        Some(1) => Action::Options,
        Some(2) if !c.activity_mode.is_chaos() => Action::Goal,
        Some(3) if !c.activity_mode.is_chaos() => Action::Validation,
        _ => Action::Exit,
    })
}

fn summary(c: &runner::Config, goal: &str, check: &[String]) -> String {
    let role = |model: &str| {
        if model.is_empty() {
            "Use loop model".into()
        } else {
            crate::model_picker::display_model(model)
        }
    };
    let duration = if c.run_duration_seconds == 0 {
        "Unlimited".into()
    } else {
        format!("{} hours", c.run_duration_seconds as f64 / 3600.)
    };
    let timeout = if c.request_timeout_seconds == 0 {
        "Unlimited".into()
    } else {
        format!("{} minutes", c.request_timeout_seconds as f64 / 60.)
    };
    let schedule = c
        .active_hours
        .at(chrono::Utc::now())
        .map(|v| v.description)
        .unwrap_or_else(|error| error.to_string());
    let mut text = format!(
        "Computer: {}\nLoop model: {}\nLoop thinking: {}\nLoop sampling: {}\nContext: {} tokens\nResponse limit: {} tokens\nActive hours: {schedule}\nRun duration: {duration}\nRequest timeout: {timeout}",
        c.execution.label(),
        role(&c.model),
        c.reasoning.label(),
        c.sampling.label(),
        token_count(c.context_tokens),
        token_count(c.output_tokens)
    );
    if c.activity_mode.is_chaos() {
        text.push_str(&format!("\nPersonality: {}", c.self_profile.label()));
    }
    text.push_str(&format!("\nHelpers: {}\nHelper model: {}\nResponses per helper: {}\nHelper thinking: {}\nHelper sampling: {}\nSetup model: {}\nSetup thinking: {}\nSetup sampling: {}\nAssistant model: {}\nAssistant thinking: {}\nAssistant sampling: {}\nFirst command review: {} seconds\nResponses per cycle: {}\nCycle retry delay: {} seconds",
        if c.helpers.enabled {"Enabled"}else{"Disabled"},role(&c.helpers.model),c.helpers.max_calls,c.helpers.reasoning.label(),c.helpers.sampling.label(),
        role(&c.setup_model),c.setup_reasoning.label(),c.setup_sampling.label(),role(&c.chat_model),c.chat_reasoning.label(),c.chat_sampling.label(),c.command_review_seconds,c.implementation_calls,c.retry_seconds));
    if !c.activity_mode.is_chaos() {
        text.push_str(&format!(
            "\nGoal completion: {}\nValidation command: {}\n\nACCEPTED GOAL\n{goal}",
            if c.allow_goal_completion {
                "Model may mark the overall goal complete"
            } else {
                "Keep the loop running"
            },
            shell_words::join(check)
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_content_survives_option_changes_and_is_removed_only_after_finish() {
        let root = tempfile::tempdir().unwrap();
        let content = Content {
            goal: Some("Preserve this accepted goal".into()),
            validation: Some(vec!["checker".into(), "a b".into()]),
        };
        content.save(root.path()).unwrap();
        let restored = Content::load(root.path()).unwrap();
        assert_eq!(restored.goal, content.goal);
        assert_eq!(restored.validation, content.validation);
        assert!(!root.path().join("chuggin.json").exists());
        assert!(!root.path().join(".git").exists());
        clear(root.path()).unwrap();
        assert!(Content::load(root.path()).unwrap().goal.is_none());
    }

    #[test]
    fn guided_choices_are_pinned_while_legacy_stdin_keeps_shared_inheritance() {
        let root = tempfile::tempdir().unwrap();
        let defaults = Settings {
            model: "shared-model".into(),
            ..Settings::default()
        };
        let c = draft_config_with(
            root.path(),
            &defaults,
            &json!({"request_timeout_seconds":2700,"self_profile":"personality"}),
        )
        .unwrap();
        let command = vec!["checker".into(), "a b".into()];
        let guided =
            configuration(&c, "An accepted goal", &command, 300, &json!({}), true).unwrap();
        assert_eq!(guided["ollama_url"], c.ollama_url);
        assert_eq!(guided["context_tokens"], c.context_tokens);
        assert_eq!(guided["request_timeout_seconds"], 2700);
        assert_eq!(guided["checks"][0]["argv"], json!(command));
        assert_eq!(guided["command_review_seconds"], 300);
        let legacy =
            configuration(&c, "An accepted goal", &command, 120, &json!({}), false).unwrap();
        assert!(legacy.get("ollama_url").is_none());
        assert!(legacy.get("context_tokens").is_none());
        assert!(legacy.get("request_timeout_seconds").is_none());
        assert!(!root.path().join("chuggin.json").exists());
    }
}
