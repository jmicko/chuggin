//! Bounded, best-effort observation. A slow renderer never blocks the agent.
use serde_json::Value;
use std::cell::RefCell;
use std::path::PathBuf;
thread_local! { static ACTOR: RefCell<String> = RefCell::new("loop".into()); }
pub fn actor() -> String {
    ACTOR.with(|a| a.borrow().clone())
}
pub fn set_actor(actor: &str) {
    ACTOR.with(|a| *a.borrow_mut() = actor.into());
}
use std::sync::{
    Mutex, OnceLock,
    mpsc::{self, Receiver, SyncSender},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Event {
    Workspace {
        path: String,
        branch: String,
        head: String,
        recovery: String,
        pending: Option<String>,
    },
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
    CommandStatus {
        id: String,
        elapsed: u64,
        next_review: u64,
        running: bool,
    },
    ValidationDone {
        passed: bool,
        checkpoint: Option<String>,
    },
    Outcome {
        disposition: String,
        task: String,
    },
}
struct Subscriber {
    actor: String,
    sender: SyncSender<Event>,
}
static SINK: OnceLock<Mutex<Vec<Subscriber>>> = OnceLock::new();
pub fn subscribe() -> Receiver<Event> {
    subscribe_actor("loop")
}
pub fn subscribe_actor(actor: &str) -> Receiver<Event> {
    let (tx, rx) = mpsc::sync_channel(4096);
    SINK.get_or_init(Default::default)
        .lock()
        .unwrap()
        .push(Subscriber {
            actor: actor.into(),
            sender: tx,
        });
    rx
}
pub fn unsubscribe() { /* Receivers unsubscribe on drop; other viewers remain attached. */
}
pub fn send(event: Event) {
    let actor = actor();
    if let Some(taps) = TAPS.get() {
        taps.lock().unwrap().retain(|tx| {
            !matches!(
                tx.try_send((actor.clone(), event.clone())),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
    }
    if let Some(s) = SINK.get() {
        s.lock().unwrap().retain(|sub| {
            if sub.actor != actor {
                return true;
            }
            !matches!(
                sub.sender.try_send(event.clone()),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
    }
}
static PROTOCOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn protocol_output() {
    PROTOCOL.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn log(text: String) {
    send(Event::Log(text.clone()));
    if !crate::ui::active() {
        if PROTOCOL.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
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

type TapSender = SyncSender<(String, Event)>;
static TAPS: OnceLock<Mutex<Vec<TapSender>>> = OnceLock::new();
pub fn tap() -> Receiver<(String, Event)> {
    let (tx, rx) = mpsc::sync_channel(16384);
    TAPS.get_or_init(Default::default).lock().unwrap().push(tx);
    rx
}
