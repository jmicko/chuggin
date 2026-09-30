use crate::{runner, setup};
use anyhow::Result;
use dialoguer::{Select, theme::ColorfulTheme};
use std::{
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub fn select(title: &str, items: &[String], default: usize) -> Result<Option<usize>> {
    if crate::ui::active() {
        return crate::ui::select(title, items, default);
    }
    Ok(Select::with_theme(&ColorfulTheme::default())
        .with_prompt(title)
        .items(items)
        .default(default)
        .interact_opt()?)
}
pub(crate) fn progress_text(progress: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(progress) else {
        return progress.into();
    };
    let branch = v["working_branch"]
        .as_str()
        .or_else(|| v["accepted_branch"].as_str())
        .unwrap_or("—");
    let workspace = v["working_workspace"]
        .as_str()
        .or_else(|| v["accepted_workspace"].as_str())
        .unwrap_or("—");
    let passing = v["last_checks_passed_ref"]
        .as_str()
        .filter(|reference| !reference.is_empty())
        .unwrap_or("None recorded");
    let workspace_label = if v["schema_version"].as_u64().unwrap_or(0) < 4 {
        "Developing folder (legacy)"
    } else {
        "Visible project"
    };
    let mut text = format!(
        "Cycle {}\nBranch: {branch}\n{workspace_label}: {workspace}\nLast recovery snapshot with passing checks: {passing}\n\nCheckpoints save unfinished work too. Check results and review findings guide the next cycle.\n\nRecent cycles\n",
        v["cycle"]
    );
    text.push_str(&format!(
        "\nNormal branch commit: {}\nLatest recovery save: {}\n",
        v["branch_head"].as_str().unwrap_or("legacy"),
        v["working_ref"].as_str().unwrap_or("—")
    ));
    if let Some(reason) = v["commit_pending"].as_str() {
        text.push_str(&format!("Commit deferred: {reason}\n"));
    }
    if let Some(items) = v["recent"].as_array() {
        for outcome in items.iter().rev() {
            text.push_str(&format!(
                "\n#{} · {}\n{}\n",
                outcome["cycle"],
                crate::ui::outcome_label(outcome["disposition"].as_str().unwrap_or("")),
                outcome["task"].as_str().unwrap_or("")
            ));
        }
    }
    text
}
pub fn home(stop: Arc<AtomicBool>, running: Arc<AtomicBool>) -> Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Open chuggin in a terminal to use the menu. For unattended runs use chuggin run --forever."
    );
    let _screen = crate::ui::Screen::enter()?;
    loop {
        let project = setup::find_project()?;
        let settings = setup::settings()?;
        let items = vec![
            if project.is_some() {
                "Resume project"
            } else {
                "Set up this project"
            }
            .into(),
            "Project goal".into(),
            "Progress".into(),
            "Choose model".into(),
            "Settings".into(),
            "Run duration".into(),
            "Chat with this project".into(),
            "Tool requests".into(),
            "Quit".into(),
        ];
        let effective = project.as_ref().and_then(|p| runner::load(p).ok());
        let root = effective
            .as_ref()
            .map(|c| c.repo.clone())
            .unwrap_or(std::env::current_dir()?)
            .display()
            .to_string();
        let model = effective
            .as_ref()
            .map(|c| c.model.as_str())
            .unwrap_or(&settings.model);
        let model = if model.trim().is_empty() {
            "Choose a model during setup"
        } else {
            model
        };
        let status = if let Some(c) = &effective {
            std::fs::read(c.state_dir.join("state.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .map(|v| {
                    format!(
                        "Ready after cycle {}\nResume saved work and unresolved findings.",
                        v["cycle"]
                    )
                })
                .unwrap_or("Goal saved. Ready for the first cycle.".into())
        } else {
            "A fresh project. Start with a goal; Chuggin will help shape it.".into()
        };
        let Some(choice) = crate::ui::home_select(&items, &root, model, &status)? else {
            return Ok(());
        };
        let action: Result<()> = (|| {
            match choice {
                0 => {
                    if let Some(path) = project.as_ref() {
                        if !runner::migration_ui(path)? {
                            return Ok(());
                        }
                        stop.store(false, Ordering::SeqCst);
                        running.store(true, Ordering::SeqCst);
                        let result = crate::ui::dashboard(path, stop.clone(), running.clone());
                        running.store(false, Ordering::SeqCst);
                        result?;
                    } else {
                        setup::wizard()?;
                    }
                }
                1 => {
                    if let Some(path) = project.as_ref() {
                        crate::ui::show("Project goal", &runner::load(path)?.goal)?;
                    } else {
                        crate::ui::show(
                            "Project goal",
                            "Set up this project to draft and accept its goal.",
                        )?;
                    }
                }
                2 => {
                    if let Some(path) = project.as_ref() {
                        runner::recovery_ui(path)?;
                    } else {
                        crate::ui::show("Saved progress", "This project has not started yet.")?;
                    }
                }
                3 => setup::choose_model(project.as_deref())?,
                4 => setup::project_settings_menu(project.as_deref())?,
                5 => {
                    if let Some(path) = project.as_ref() {
                        setup::run_duration(path)?;
                    } else {
                        crate::ui::show("Run duration", "Set up this project first.")?;
                    }
                }
                6 => {
                    if let Some(path) = project.as_ref() {
                        crate::ui::chat_home(path, stop.clone(), running.clone())?;
                    }
                }
                7 => {
                    if let Some(path) = project.as_ref() {
                        crate::tool_requests::menu(path)?;
                    } else {
                        crate::ui::show("Tool requests", "Set up this project first.")?;
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if choice == 8 {
            return Ok(());
        }
        if let Err(e) = action {
            crate::ui::clear_notes();
            crate::ui::notice(format!("{e:#}"));
            crate::ui::select("Could not complete this action", &["Return home".into()], 0)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::progress_text;

    #[test]
    fn saved_progress_distinguishes_unfinished_work_from_passing_checks() {
        let text = progress_text(
            r#"{"cycle":5,"working_branch":"codex/working","working_workspace":"/project/workspace","last_checks_passed_ref":"abc123","recent":[{"cycle":5,"disposition":"checkpoint/checks-failing","task":"Repair the parser"}]}"#,
        );
        assert!(text.contains("Branch: codex/working"));
        assert!(text.contains("Last recovery snapshot with passing checks: abc123"));
        assert!(text.contains("Saved · checks failing"));
        assert!(text.contains("Checkpoints save unfinished work too."));
    }

    #[test]
    fn saved_progress_can_display_legacy_state_before_migration() {
        let text = progress_text(
            r#"{"cycle":2,"accepted_branch":"codex/old","accepted_workspace":"/old/workspace"}"#,
        );
        assert!(text.contains("Branch: codex/old"));
        assert!(text.contains("Developing folder (legacy): /old/workspace"));
        assert!(text.contains("Last recovery snapshot with passing checks: None recorded"));
    }
}
