use crate::{runner, setup};
use anyhow::{Context, Result};
use dialoguer::{Select, theme::ColorfulTheme};
use std::{
    io::{self, IsTerminal, Write},
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
pub fn pause() -> Result<()> {
    if crate::ui::active() {
        crate::ui::select("Continue", &["Back".into()], 0)?;
        return Ok(());
    }
    print!("\nPress Enter to return to the menu…");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(())
}
fn progress_text(progress: &str) -> String {
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
    let mut text = format!(
        "Cycle {}\nWorking branch: {branch}\nWorkspace: {workspace}\nLast checkpoint with passing checks: {passing}\n\nCheckpoints save unfinished work too. Check results and review findings guide the next cycle.\n\nRecent cycles\n",
        v["cycle"]
    );
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
            "Shared settings".into(),
            "Run duration".into(),
            "Quit".into(),
        ];
        let root = std::env::current_dir()?.display().to_string();
        let effective = project.as_ref().and_then(|p| runner::load(p).ok());
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
                        let c = runner::load(path)?;
                        let progress = std::fs::read_to_string(c.state_dir.join("state.json"))
                            .unwrap_or("This project has not started yet.".into());
                        let text = progress_text(&progress);
                        crate::ui::show("Saved progress", &text)?;
                    } else {
                        crate::ui::show("Saved progress", "This project has not started yet.")?;
                    }
                }
                3 => setup::choose_model(project.as_deref())?,
                4 => setup::settings_menu()?,
                5 => {
                    if let Some(path) = project.as_ref() {
                        setup::run_duration(path)?;
                    } else {
                        crate::ui::show("Run duration", "Set up this project first.")?;
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if choice == 6 {
            return Ok(());
        }
        if let Err(e) = action {
            crate::ui::notice(format!("\n{e:#}"));
            pause().context("Could not return to menu")?;
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
        assert!(text.contains("Working branch: codex/working"));
        assert!(text.contains("Last checkpoint with passing checks: abc123"));
        assert!(text.contains("Saved · checks failing"));
        assert!(text.contains("Checkpoints save unfinished work too."));
    }

    #[test]
    fn saved_progress_can_display_legacy_state_before_migration() {
        let text = progress_text(
            r#"{"cycle":2,"accepted_branch":"codex/old","accepted_workspace":"/old/workspace"}"#,
        );
        assert!(text.contains("Working branch: codex/old"));
        assert!(text.contains("Workspace: /old/workspace"));
        assert!(text.contains("Last checkpoint with passing checks: None recorded"));
    }
}
