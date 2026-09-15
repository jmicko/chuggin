mod code_index;
mod dev_tools;
mod events;
mod menu;
mod model;
mod project;
mod prompts;
mod runner;
mod setup;
mod symbols;
mod ui;
mod web_tools;

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
    about = "Fresh-context stages for persistent local coding experiments"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Interactive project setup without starting the loop.
    Setup,
    /// Edit shared Ollama and model defaults.
    Settings {
        #[arg(long)]
        show: bool,
    },
    /// Write a project configuration without interactive goal drafting.
    Init {
        #[arg(long, default_value = "lupin.json")]
        config: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        goal: String,
    },
    /// Run a bounded experiment, or use --forever.
    Run {
        #[arg(long, default_value = "lupin.json")]
        config: PathBuf,
        #[arg(long, default_value_t = 1)]
        cycles: u64,
        #[arg(long)]
        forever: bool,
    },
    /// Inspect saved progress.
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    let stopped = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let active = running.clone();
    ctrlc::set_handler(move || {
        if !active.load(Ordering::SeqCst) || flag.swap(true, Ordering::SeqCst) {
            ui::restore();
            project::kill_active_check();
            eprintln!("Stopped. Run lupin again to resume from saved progress.");
            std::process::exit(130);
        }
        events::log("Stop requested: finishing this loop, then saving and exiting. Press Ctrl-C again to stop immediately.".into());
    })?;
    match cli.command {
        None => menu::home(stopped, running),
        Some(Command::Setup) => {
            setup::wizard()?;
            Ok(())
        }
        Some(Command::Settings { show }) => setup::configure(show),
        Some(Command::Init { config, repo, goal }) => runner::init(&config, &repo, &goal),
        Some(Command::Status { config }) => {
            let path = config
                .or(setup::find_project()?)
                .unwrap_or(PathBuf::from("lupin.json"));
            runner::status(&path)
        }
        Some(Command::Run {
            config,
            cycles,
            forever,
        }) => {
            anyhow::ensure!(
                forever || cycles > 0,
                "Use --forever or a positive --cycles count"
            );
            running.store(true, Ordering::SeqCst);
            runner::run(&config, if forever { None } else { Some(cycles) }, stopped)
        }
    }
}
