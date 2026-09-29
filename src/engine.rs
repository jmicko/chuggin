//! Private local transport to the one process owning the checkout.
use crate::operator::{Controller, text};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub sequence: u64,
    pub actor: String,
    pub event: crate::events::Event,
}
#[derive(Clone)]
pub struct Client {
    pub socket: PathBuf,
}
fn socket_path(path: &Path) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let c = crate::runner::load(path)?;
    let root = fs::canonicalize(crate::workspace::git_dir(&c.repo)?)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let base = crate::setup::settings_path()?
        .parent()
        .unwrap()
        .join("runtime");
    fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
    }
    Ok(base.join(format!("{:016x}.sock", h.finish())))
}
impl Client {
    pub fn connect(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        let client = Self {
            socket: socket_path(&path)?,
        };
        if let Ok(status) = client.request(json!({"action":"status"})) {
            ensure!(
                status["version"] == env!("CARGO_PKG_VERSION"),
                "A different Chuggin version owns this project. Stop that run and close its controller before upgrading."
            );
            ensure!(
                status["config"].as_str() == path.to_str(),
                "This checkout is already controlled with a different project configuration"
            );
            return Ok(client);
        }
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(client.socket.with_extension("log"))?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("engine")
            .arg("--config")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if client.request(json!({"action":"status"})).is_ok() {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(client);
            }
            if child.try_wait()?.is_some() {
                anyhow::bail!(
                    "Could not open project controller: {}. See {}",
                    fs::read_to_string(client.socket.with_extension("log"))
                        .map(|s| crate::project::excerpt(&s, 4000))
                        .unwrap_or_default(),
                    client.socket.with_extension("log").display()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "Controller startup timed out; inspect {}",
                client.socket.with_extension("log").display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn request(&self, value: Value) -> Result<Value> {
        #[cfg(unix)]
        {
            let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(120)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            let bytes = serde_json::to_vec(&value)?;
            ensure!(bytes.len() < 2_000_000, "Request too large");
            stream.write_all(&bytes)?;
            stream.write_all(b"\n")?;
            let mut line = String::new();
            BufReader::new(stream)
                .take(4_000_000)
                .read_line(&mut line)?;
            let reply: Value = serde_json::from_str(&line)?;
            if reply["ok"] == true {
                Ok(reply["result"].clone())
            } else {
                anyhow::bail!(
                    "{}",
                    reply["error"]
                        .as_str()
                        .unwrap_or("Controller request failed")
                )
            }
        }
        #[cfg(not(unix))]
        {
            let _ = value;
            anyhow::bail!("Local controller transport currently requires Unix");
        }
    }
    pub fn open_session(&self, id: Option<&str>) -> Result<String> {
        Ok(
            self.request(json!({"action":"open_session","session_id":id}))?["session_id"]
                .as_str()
                .context("Missing session")?
                .into(),
        )
    }
    pub fn call(&self, session: &str, name: &str, args: Value, operation: &str) -> Result<Value> {
        self.request(json!({"action":"call","session_id":session,"name":name,"arguments":args,"operation_id":operation}))
    }
}
struct Engine {
    controller: Arc<Controller>,
    events: Mutex<VecDeque<Record>>,
    chats: crate::chat::Chats,
    touched: Mutex<Instant>,
    quit: AtomicBool,
}
impl Engine {
    fn request(&self, v: Value) -> Result<Value> {
        *self.touched.lock().unwrap() = Instant::now();
        let action = text(&v, "action")?;
        if [
            "start",
            "resume",
            "pause",
            "stop",
            "retry",
            "force_stop",
            "shutdown",
        ]
        .contains(&action)
        {
            let path = self
                .controller
                .config()?
                .state_dir
                .join("operator/control.jsonl");
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(
                file,
                "{}",
                json!({"at":chrono::Utc::now(),"action":action,"mode":v["mode"],"cycles":v["cycles"]})
            )?;
        }
        match action {
            "status" => self.controller.status(),
            "events" => {
                let since = v["after"].as_u64().unwrap_or(0);
                let events: Vec<_> = self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| e.sequence > since)
                    .take(4096)
                    .cloned()
                    .collect();
                Ok(serde_json::to_value(events)?)
            }
            "start" => self.controller.start_count(
                v["mode"].as_str().unwrap_or("scheduled"),
                v["cycles"].as_u64(),
                v["reopen"].as_bool().unwrap_or(true),
            ),
            "pause" => {
                if !self.controller.controls.manual_pause() {
                    self.controller.controls.toggle_pause();
                }
                self.controller.status()
            }
            "resume" => self
                .controller
                .start(v["mode"].as_str().unwrap_or("scheduled")),
            "stop" => {
                self.controller.stop.store(true, Ordering::SeqCst);
                self.controller.status()
            }
            "retry" => Ok(json!({"retry_queued":self.controller.controls.retry_now()})),
            "force_stop" => {
                crate::project::kill_active_check();
                self.quit.store(true, Ordering::SeqCst);
                Ok(json!({"stopped":true}))
            }
            "shutdown" => {
                ensure!(
                    !self.controller.running.load(Ordering::SeqCst)
                        && !self.chats.any_running()
                        && !self.controller.has_editor()
                        && crate::project::active_checks().is_empty(),
                    "Stop the loop and finish commands before quitting the controller"
                );
                self.quit.store(true, Ordering::SeqCst);
                Ok(json!({"closing":true}))
            }
            "open_session" => {
                Ok(json!({"session_id":self.controller.open_session(v["session_id"].as_str())?}))
            }
            "touch" => {
                self.controller.touch(text(&v, "session_id")?)?;
                Ok(json!({"ok":true}))
            }
            "tools" => Ok(crate::operator::schemas()),
            "call" => {
                let session = text(&v, "session_id")?;
                crate::events::set_actor(session);
                self.controller.call(
                    session,
                    text(&v, "name")?,
                    v["arguments"].clone(),
                    text(&v, "operation_id")?,
                )
            }
            "operation" => crate::operator::read_json(
                &self
                    .controller
                    .operation_path(text(&v, "session_id")?, text(&v, "operation_id")?)?,
            ),
            "chat_send" => self.chats.send(
                self.controller.clone(),
                text(&v, "session_id")?,
                text(&v, "message")?,
            ),
            "chat_status" => self.chats.status(&self.controller, text(&v, "session_id")?),
            "chat_cancel" => {
                self.chats.cancel(text(&v, "session_id")?);
                Ok(json!({"cancel_requested":true}))
            }
            _ => anyhow::bail!("Unknown controller action"),
        }
    }
}
pub fn serve(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let controller = Controller::new(path)?; // Acquire checkout locks BEFORE removing stale socket.
        let socket = socket_path(path)?;
        if socket.exists() {
            fs::remove_file(&socket)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        listener.set_nonblocking(false)?;
        let rx = crate::events::tap();
        let engine = Arc::new(Engine {
            controller,
            events: Mutex::default(),
            chats: crate::chat::Chats::default(),
            touched: Mutex::new(Instant::now()),
            quit: AtomicBool::new(false),
        });
        let connection_engine = engine.clone();
        std::thread::spawn(move || {
            let engine = connection_engine;
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        let worker = engine.clone();
                        std::thread::spawn(move || {
                            let result = (|| -> Result<Value> {
                                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                                let mut line = String::new();
                                BufReader::new(stream.try_clone()?)
                                    .take(2_000_000)
                                    .read_line(&mut line)?;
                                worker.request(serde_json::from_str(&line)?)
                            })();
                            let value = match result {
                                Ok(v) => json!({"ok":true,"result":v}),
                                Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
                            };
                            let mut stream = stream;
                            let _ = writeln!(stream, "{value}");
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        let mut sequence = 0;
        while !engine.quit.load(Ordering::SeqCst) {
            for (actor, event) in rx.try_iter().take(8192) {
                sequence += 1;
                let mut events = engine.events.lock().unwrap();
                events.push_back(Record {
                    sequence,
                    actor,
                    event,
                });
                if events.len() > 16000 {
                    events.pop_front();
                }
            }
            engine.controller.maintenance();
            engine.chats.heartbeat(&engine.controller);
            if engine.controller.running.load(Ordering::SeqCst)
                && engine.touched.lock().unwrap().elapsed() > Duration::from_secs(60)
                && !engine.controller.controls.manual_pause()
            {
                engine.controller.controls.toggle_pause();
                crate::events::log("Viewer disconnected: pausing the loop after its current operation. Reopen Chuggin and resume when ready.".into());
            }
            std::thread::sleep(Duration::from_millis(20));
            if !engine.controller.running.load(Ordering::SeqCst)
                && !engine.chats.any_running()
                && !engine.controller.has_editor()
                && crate::project::active_checks().is_empty()
                && engine.touched.lock().unwrap().elapsed() > Duration::from_secs(60)
            {
                break;
            }
        }
        let marker = engine
            .controller
            .config()?
            .state_dir
            .join("operator/controller-running.json");
        if marker.exists() {
            fs::remove_file(marker)?;
        }
        fs::remove_file(socket)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("Local controller transport currently requires Unix");
    }
}

static FOREGROUND: std::sync::LazyLock<Mutex<Option<Client>>> =
    std::sync::LazyLock::new(Default::default);
pub fn foreground(client: Option<Client>) {
    static HEARTBEAT: std::sync::Once = std::sync::Once::new();
    *FOREGROUND.lock().unwrap() = client;
    HEARTBEAT.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(10));
                let current = FOREGROUND.lock().unwrap().clone();
                if let Some(client) = current {
                    let _ = client.request(json!({"action":"status"}));
                }
            }
        });
    });
}
pub fn force_foreground() {
    if let Some(c) = FOREGROUND.lock().unwrap().as_ref() {
        let _ = c.request(json!({"action":"force_stop"}));
        let deadline = Instant::now() + Duration::from_secs(3);
        while c.socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Headless runs attach to the same owner as TUI and MCP.
pub fn run(path: &Path, count: Option<u64>, stop: Arc<AtomicBool>) -> Result<()> {
    let client = Client::connect(path)?;
    ensure!(
        client.request(json!({"action":"status"}))?["running"] != true,
        "This project already has an active run; attach with the terminal interface"
    );
    foreground(Some(client.clone()));
    client.request(json!({"action":"start","cycles":count,"reopen":false}))?;
    let mut cursor = 0;
    let mut stopping = false;
    let result = (|| -> Result<()> {
        loop {
            if stop.load(Ordering::SeqCst) && !stopping {
                client.request(json!({"action":"stop"}))?;
                stopping = true;
            }
            let records: Vec<Record> =
                serde_json::from_value(client.request(json!({"action":"events","after":cursor}))?)?;
            for record in records {
                cursor = record.sequence;
                if record.actor == "loop"
                    && let crate::events::Event::Log(line) = record.event
                {
                    println!("{line}");
                }
            }
            let state = client.request(json!({"action":"status"}))?;
            if state["running"] == false {
                std::thread::sleep(Duration::from_millis(60));
                let records: Vec<Record> = serde_json::from_value(
                    client.request(json!({"action":"events","after":cursor}))?,
                )?;
                for record in records {
                    if record.actor == "loop"
                        && let crate::events::Event::Log(line) = record.event
                    {
                        println!("{line}");
                    }
                }
                if let Some(error) = state["last_error"].as_str() {
                    anyhow::bail!("{error}");
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();
    foreground(None);
    let _ = close_idle(path);
    result
}
/// Legacy history dialogs require the idle controller to relinquish its checkout lock.
pub fn close_idle(path: &Path) -> Result<()> {
    let client = Client {
        socket: socket_path(path)?,
    };
    if client.request(json!({"action":"status"})).is_ok() {
        client.request(json!({"action":"shutdown"}))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while client.socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
        }
        ensure!(
            !client.socket.exists(),
            "The project controller is still closing; try again shortly"
        );
    }
    Ok(())
}
