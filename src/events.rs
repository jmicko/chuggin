//! Bounded, best-effort observation. A slow renderer never blocks the agent.
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{
    Mutex, OnceLock,
    mpsc::{self, Receiver, SyncSender},
};

#[derive(Clone, Debug)]
pub enum Event {
    Log(String),
    Phase(String),
    Cycle(u64),
    Task(String),
    Request,
    RequestModel(String),
    RequestFinished,
    ProviderWait {
        reason: String,
        seconds: u64,
    },
    Delta(String),
    Metrics {
        prompt: u64,
        generated: u64,
        seconds: f64,
    },
    Tool(String),
    Check {
        command: String,
        path: PathBuf,
    },
    CheckDone(bool),
    ValidationDone {
        passed: bool,
        checkpoint: Option<String>,
    },
    Outcome {
        disposition: String,
        task: String,
    },
}
static SINK: OnceLock<Mutex<Option<SyncSender<Event>>>> = OnceLock::new();
pub fn subscribe() -> Receiver<Event> {
    let (tx, rx) = mpsc::sync_channel(4096);
    *SINK.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(tx);
    rx
}
pub fn unsubscribe() {
    if let Some(s) = SINK.get() {
        *s.lock().unwrap() = None;
    }
}
pub fn send(event: Event) {
    if let Some(s) = SINK.get()
        && let Ok(s) = s.lock()
        && let Some(tx) = s.as_ref()
    {
        let _ = tx.try_send(event);
    }
}
pub fn log(text: String) {
    let active = SINK
        .get()
        .and_then(|s| s.lock().ok())
        .is_some_and(|s| s.is_some());
    if active {
        send(Event::Log(text));
    } else {
        println!("{text}");
    }
}
pub fn artifact(stage: &str, value: &Value) {
    if stage.starts_with("scope-extension-") {
        send(Event::Log(format!(
            "Scope expanded: {} · {}",
            value["path"].as_str().unwrap_or(""),
            value["reason"].as_str().unwrap_or("")
        )));
    } else if stage.starts_with("response-error-") || stage.starts_with("patch-error-") {
        send(Event::Log(format!(
            "Recovery: {}",
            value.as_str().unwrap_or("See cycle artifacts for details")
        )));
    } else if stage.starts_with("tool-") && value["ok"] == false {
        send(Event::Log(format!(
            "Tool error: {}",
            value["error"].as_str().unwrap_or("See cycle artifacts")
        )));
    }
    match stage {
        "task" => {
            if let Some(t) = value["title"].as_str() {
                send(Event::Task(t.into()));
            }
        }
        "outcome" => send(Event::Outcome {
            disposition: value["disposition"].as_str().unwrap_or("retry").into(),
            task: value["task"].as_str().unwrap_or("").into(),
        }),
        _ => {}
    }
}
