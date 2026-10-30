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
    let file_observations = v["activity_mode"] == "type2" || v["activity_mode"] == "chaos";
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
    if file_observations {
        text = format!(
            "Cycle {}\nWorkspace: {workspace}\n\nConversation, tool history, and activity records are saved. Files remain in the workspace. File hashes support change detection; they are not recoverable backups. There are no automatic commits or branch restrictions.\n\nRecent cycles\n",
            v["cycle"]
        );
    }
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
    let mut computers = startup_computers();
    loop {
        let project = setup::find_project()?;
        let preview = if project.is_none() {
            Some(setup::draft_config(&std::env::current_dir()?)?)
        } else {
            None
        };
        let effective = project.as_ref().and_then(|p| runner::load(p).ok());
        let items = vec![
            if project.is_some() {
                "Resume project"
            } else {
                "Set up this project"
            }
            .into(),
            "Project settings".into(),
            "Global settings".into(),
            "Quit".into(),
        ];
        let root = effective
            .as_ref()
            .map(|c| c.repo.clone())
            .unwrap_or(std::env::current_dir()?)
            .display()
            .to_string();
        let model = effective
            .as_ref()
            .map(|c| c.model.as_str())
            .or_else(|| preview.as_ref().map(|c| c.model.as_str()))
            .unwrap_or("");
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
                .unwrap_or(if c.activity_mode.is_chaos() {
                    "Session saved. Ready for continuous inference.".into()
                } else {
                    "Goal saved. Ready for the first cycle.".into()
                })
        } else {
            "A fresh project. Setup offers goal-driven work or No goals, only chaos.".into()
        };
        let status = if let Some(c) = effective.as_ref().or(preview.as_ref()) {
            format!("{}\n\n{status}", c.activity_mode.label())
        } else {
            status
        };
        let status = private_computer_notice(&status, &computers);
        let Some(choice) = crate::ui::home_select(&items, &root, model, &status)? else {
            return Ok(());
        };
        if items[choice] == "Quit" {
            return Ok(());
        }
        let action: Result<()> = (|| {
            match choice {
                0 => {
                    if let Some(path) = project.as_ref() {
                        crate::ui::loading(
                            "Opening project",
                            "Loading saved project settings and checking its saved work…",
                        )?;
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
                1 => setup::project_settings_menu(project.as_deref())?,
                2 => setup::settings_menu()?,
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = action {
            crate::ui::clear_notes();
            crate::ui::notice(format!("{e:#}"));
            crate::ui::select("Could not complete this action", &["Return home".into()], 0)?;
        }
        computers = startup_computers();
    }
}

fn startup_computers() -> Vec<crate::managed_computers::Computer> {
    // Podman is optional. A discovery failure must not prevent ordinary work;
    // opening the management screen shows the full error if troubleshooting is needed.
    crate::ui::busy("Checking private computers", crate::managed_computers::scan)
        .unwrap_or_default()
}

fn private_computer_notice(
    status: &str,
    computers: &[crate::managed_computers::Computer],
) -> String {
    let count = computers
        .iter()
        .filter(|computer| computer.orphaned)
        .count();
    if count == 0 {
        return status.into();
    }
    format!(
        "{} private computer{} need{} attention.\nGlobal settings → Private computers to reconnect or delete.\n\n{status}",
        count,
        if count == 1 { "" } else { "s" },
        if count == 1 { "s" } else { "" }
    )
}

fn computer_name(computer: &crate::managed_computers::Computer) -> String {
    computer
        .project
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unlinked computer".into())
}

/// Global inventory is also reachable when the original project directory is gone.
pub fn private_computers() -> Result<()> {
    loop {
        let mut computers =
            crate::ui::busy("Checking private computers", crate::managed_computers::scan)?;
        computers.sort_by_key(|computer| !computer.orphaned);
        crate::ui::clear_notes();
        crate::ui::notice(if computers.is_empty() {
            "No Chuggin private computers were found. Nothing has been deleted. Private computers appear here after their first setup.".into()
        } else {
            "These private computers belong to Chuggin. Deleting a project folder does not remove its computer. Reconnect a moved project or explicitly delete the computer you no longer need. Discovery never stops or deletes anything.".into()
        });
        let mut rows: Vec<String> = computers
            .iter()
            .map(|computer| {
                format!(
                    "{} · {} · {}",
                    computer_name(computer),
                    if computer.orphaned {
                        "Project needs attention"
                    } else {
                        "Connected"
                    },
                    if computer.running {
                        "Running"
                    } else {
                        "Stopped"
                    }
                )
            })
            .collect();
        rows.extend(["Refresh list".into(), "Back".into()]);
        let Some(index) = select("Private computers", &rows, 0)? else {
            return Ok(());
        };
        let Some(computer) = computers.get(index) else {
            if index == computers.len() {
                continue;
            }
            return Ok(());
        };
        if let Err(error) = private_computer_actions(computer) {
            crate::ui::show("Could not update private computer", &format!("{error:#}"))?;
        }
    }
}

fn private_computer_actions(computer: &crate::managed_computers::Computer) -> Result<()> {
    loop {
        crate::ui::clear_notes();
        crate::ui::notice(format!(
            "{}\n\nContainer: {}\nProject: {}\nChuggin history: {}\nState: {}\n\n{}",
            computer_name(computer),
            computer.name,
            computer
                .project
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "Unknown".into()),
            computer
                .state_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "Unknown".into()),
            if computer.running {
                "Running"
            } else {
                "Stopped"
            },
            computer.reason
        ));
        let mut rows = Vec::new();
        if computer.orphaned {
            rows.push("Reconnect moved project…".into());
        }
        rows.extend(["Delete private computer…".into(), "Back".into()]);
        let Some(index) = select("Private computer", &rows, 0)? else {
            return Ok(());
        };
        let choice = rows[index].as_str();
        match choice {
            "Reconnect moved project…" => {
                crate::ui::clear_notes();
                crate::ui::notice("Choose the new location of this computer's original project folder, including its saved .chuggin history. Enter the folder or its chuggin.json file. Reconnecting keeps the computer's files and conversation history; it closes applications inside the private computer while repairing its connection. Stop any Chuggin run using this computer first.".into());
                let path = crate::setup::ask("New project folder or chuggin.json", "")?;
                if path.trim().is_empty() {
                    continue;
                }
                let path = expand_project_path(path.trim())?;
                crate::ui::clear_notes();
                crate::ui::notice(format!(
                    "Reconnect {} to {}?\n\nIts Linux installation and files are preserved. Running applications inside the private computer will close during the repair; save any open work first. Chuggin backs up the affected configuration and saved state before changing paths. A busy project must be stopped first; this does not start inference.",
                    computer.name,
                    path.display()
                ));
                if select(
                    "Reconnect moved project?",
                    &["Cancel".into(), "Reconnect and keep files".into()],
                    0,
                )? != Some(1)
                {
                    continue;
                }
                let name = computer.name.clone();
                crate::ui::busy(
                    "Reconnecting private computer · preserving files",
                    move || crate::managed_computers::reconnect(&name, &path),
                )?;
                crate::ui::show(
                    "Private computer reconnected",
                    "The computer is connected to the new project location. Its files and saved conversation are retained. Open Chuggin in that project to resume; no inference has been started.",
                )?;
                return Ok(());
            }
            "Delete private computer…" => {
                crate::ui::clear_notes();
                crate::ui::notice(format!(
                    "Permanently delete {} ({})?\n\nThis deletes this private computer's Linux installation, installed applications and files, including work that exists only inside it. Any running applications will close. This cannot be undone.\n\nThe project folder and Chuggin conversation history are kept. Other computers and shared desktop images are not deleted. Stop any Chuggin run using this computer first.",
                    computer_name(computer),
                    computer.name
                ));
                if select(
                    "Delete private computer?",
                    &[
                        "Keep private computer".into(),
                        "Delete private computer permanently".into(),
                    ],
                    0,
                )? != Some(1)
                {
                    continue;
                }
                let name = computer.name.clone();
                let config = computer
                    .project
                    .as_ref()
                    .filter(|_| !computer.orphaned)
                    .map(|root| root.join("chuggin.json"));
                crate::ui::busy("Deleting the selected private computer", move || {
                    if let Some(config) = config.filter(|path| path.is_file())
                        && crate::runner::load(&config).is_ok()
                    {
                        // Retire an idle owner through its normal shutdown path.
                        // Running inference, chat and commands still block deletion.
                        crate::engine::close_idle(&config)?;
                    }
                    crate::managed_computers::delete(&name)
                })?;
                crate::ui::show(
                    "Private computer deleted",
                    "The selected private computer was removed. The project folder, conversation history, other computers and shared desktop images are retained. Resuming this project will create a new empty private computer; the model will receive a one-time notice about the replacement.",
                )?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

fn expand_project_path(input: &str) -> Result<std::path::PathBuf> {
    if input == "~" || input.starts_with("~/") {
        let task_home = std::env::var_os("HOME").ok_or_else(|| {
            anyhow::anyhow!("Home directory is unavailable; enter an absolute path")
        })?;
        return Ok(std::path::PathBuf::from(task_home).join(input.strip_prefix("~/").unwrap_or("")));
    }
    Ok(input.into())
}

/// Project utilities live under Project settings, leaving the welcome screen focused.
pub fn project_tools(path: Option<&std::path::Path>) -> Result<()> {
    loop {
        crate::ui::clear_notes();
        crate::ui::notice("Inspect progress, talk to the model, or open its desktop. These tools belong to this project.".into());
        let private = match path {
            Some(path) => runner::load(path)?.execution.is_private(),
            None => setup::draft_config(&std::env::current_dir()?)?
                .execution
                .is_private(),
        };
        let mut rows = vec![
            "Progress".into(),
            "Project assistant".into(),
            "Messages with the loop".into(),
            "Tool requests".into(),
            "External AI access (MCP)".into(),
        ];
        if private {
            rows.push("Open desktop".into());
            rows.push("Manage private computers".into());
        }
        rows.push("Back".into());
        let Some(index) = select("Project tools", &rows, 0)? else {
            return Ok(());
        };
        if rows[index] == "Back" {
            return Ok(());
        }
        let Some(path) = path else {
            crate::ui::show(
                "Set up this project first",
                "Once setup is complete, return to Project settings → Project tools to inspect progress, chat, or open the desktop.",
            )?;
            continue;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(false));
        match index {
            0 => runner::recovery_ui(path)?,
            1 => crate::ui::chat_home(path, stop, running)?,
            2 => crate::ui::messages_home(path, stop, running)?,
            3 => crate::tool_requests::menu(path)?,
            4 => setup::external_control_info(path)?,
            5 if private => setup::view_private_desktop(Some(path))?,
            6 if private => private_computers()?,
            _ => return Ok(()),
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
