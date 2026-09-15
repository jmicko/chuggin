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
                        "Ready after cycle {}\nYour accepted work is saved.",
                        v["cycle"]
                    )
                })
                .unwrap_or("Goal accepted. Ready for the first cycle.".into())
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
                        let text=serde_json::from_str::<serde_json::Value>(&progress).ok().map(|v|{
                            let mut s=format!("Cycle {}\nAccepted branch: {}\nWorkspace: {}\n\nRecent attempts\n",v["cycle"],v["accepted_branch"].as_str().unwrap_or("—"),v["accepted_workspace"].as_str().unwrap_or("—"));
                            if let Some(items)=v["recent"].as_array(){for o in items.iter().rev(){s.push_str(&format!("\n#{} · {}\n{}\n",o["cycle"],o["disposition"].as_str().unwrap_or(""),o["task"].as_str().unwrap_or("")));}}s
                        }).unwrap_or(progress);
                        crate::ui::show("Saved progress", &text)?;
                    } else {
                        crate::ui::show("Saved progress", "This project has not started yet.")?;
                    }
                }
                3 => setup::choose_model()?,
                4 => setup::settings_menu()?,
                _ => {}
            }
            Ok(())
        })();
        if choice == 5 {
            return Ok(());
        }
        if let Err(e) = action {
            crate::ui::notice(format!("\n{e:#}"));
            pause().context("Could not return to menu")?;
        }
    }
}
