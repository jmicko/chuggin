mod action_watch;
mod agents;
mod chat;
mod code_index;
mod command_jobs;
mod command_output;
mod command_session;
mod command_watch;
mod dev_tools;
mod engine;
mod events;
mod groq;
mod history;
mod image_tools;
mod inference;
mod mcp;
mod menu;
mod migration;
mod model;
mod nudge;
mod operator;
mod project;
mod prompts;
mod prose_watch;
mod provider;
mod repetition;
mod run_control;
mod runner;
mod schedule;
mod search;
mod setup;
mod stall_diagnostic;
mod symbols;
mod terminal_title;
mod tool_requests;
mod ui;
mod vision;
mod web_tools;
mod workspace;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(
    version,
    about = "Persistent projects and continuous local model refinement"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Expose project tools to an external AI application over local MCP.
    Mcp {
        #[arg(long)]
        project: PathBuf,
    },
    #[command(hide = true)]
    Engine {
        #[arg(long)]
        config: PathBuf,
    },
    /// Interactive project setup without starting the loop.
    Setup,
    /// Edit shared Ollama and model defaults.
    Settings {
        #[arg(long)]
        show: bool,
    },
    /// Write a project configuration without interactive goal drafting.
    Init {
        #[arg(long, default_value = "chuggin.json")]
        config: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        goal: String,
    },
    /// Run a bounded experiment, or use --forever.
    Run {
        #[arg(long, default_value = "chuggin.json")]
        config: PathBuf,
        #[arg(long, default_value_t = 1)]
        cycles: u64,
        #[arg(long)]
        forever: bool,
    },
    /// Preview or apply a legacy workspace migration (also available in the menu).
    Migrate {
        #[arg(long, default_value = "chuggin.json")]
        config: PathBuf,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        clear_staging: bool,
    },
    /// Inspect saved progress.
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    // Background engines and MCP must never emit terminal controls on protocol stdout.
    let _title = if matches!(
        cli.command,
        Some(Command::Mcp { .. } | Command::Engine { .. })
    ) {
        None
    } else {
        let requested = match &cli.command {
            Some(Command::Run { config, .. } | Command::Migrate { config, .. }) => {
                Some(config.clone())
            }
            Some(Command::Status { config }) => config.clone(),
            _ => None,
        };
        let project = requested
            .or_else(|| setup::find_project().ok().flatten())
            .and_then(|path| runner::load(&path).ok())
            .map(|c| c.repo)
            .unwrap_or(std::env::current_dir()?);
        Some(terminal_title::Guard::enter(&project))
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let active = running.clone();
    ctrlc::set_handler(move || {
        if !active.load(Ordering::SeqCst) || flag.swap(true, Ordering::SeqCst) {
            ui::restore();
            terminal_title::restore();
            engine::force_foreground();
            project::kill_active_check();
            eprintln!("Stopped. Run chuggin again to resume from saved progress.");
            std::process::exit(130);
        }
        events::log("Stop requested: finishing this loop, then saving. In the dashboard, press R to resume; Ctrl-C again stops immediately.".into());
    })?;
    match cli.command {
        Some(Command::Mcp { project }) => {
            events::protocol_output();
            let path = if project.is_dir() {
                project.join("chuggin.json")
            } else {
                project
            };
            mcp::serve(&path)
        }
        Some(Command::Engine { config }) => {
            events::protocol_output();
            engine::serve(&config)
        }
        None => menu::home(stopped, running),
        Some(Command::Setup) => {
            setup::wizard()?;
            Ok(())
        }
        Some(Command::Settings { show }) => setup::configure(show),
        Some(Command::Init { config, repo, goal }) => runner::init(&config, &repo, &goal),
        Some(Command::Migrate {
            config,
            apply,
            clear_staging,
        }) => {
            if apply {
                runner::migration_apply(&config, clear_staging)
            } else {
                events::log(runner::migration_preview(&config)?);
                Ok(())
            }
        }
        Some(Command::Status { config }) => {
            let path = config
                .or(setup::find_project()?)
                .unwrap_or(PathBuf::from("chuggin.json"));
            runner::status(&path)
        }
        Some(Command::Run {
            config,
            cycles,
            forever,
        }) => {
            running.store(true, Ordering::SeqCst);
            let config = if config == std::path::Path::new("chuggin.json") && !config.exists() {
                setup::find_project()?.unwrap_or(config)
            } else {
                config
            };
            runner::run(&config, if forever { None } else { Some(cycles) }, stopped)
        }
    }
}
