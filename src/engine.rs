//! Private local transport to the one process owning the checkout.
use crate::operator::{Controller, text};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
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

/// A failed local transport is distinct from an error returned by the controller
/// (including inference provider errors). The UI can offer reconnection only here.
#[derive(Debug)]
pub struct Disconnected {
    pub socket: PathBuf,
    source: std::io::Error,
}
impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lost connection to Chuggin's background controller: {}",
            self.source
        )
    }
}
impl std::error::Error for Disconnected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
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

/// Serialize viewers opening the same checkout, including controller upgrades.
/// The kernel releases this lock if a viewer exits; the file itself is retained.
fn startup_lock(socket: &Path, deadline: Instant) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(socket.with_extension("startup.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.raw_os_error() != Some(libc::EAGAIN)
            {
                return Err(error).context("Could not lock controller startup");
            }
            ensure!(
                Instant::now() < deadline,
                "Another window is opening this project. Return home and try again shortly."
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(file)
}

/// Read only this startup attempt, retaining the newest useful failure output.
fn startup_output(log_path: &Path, offset: u64) -> Result<String> {
    let mut file = fs::File::open(log_path)?;
    let end = file.metadata()?.len();
    let offset = offset.min(end);
    let start = offset.max(end.saturating_sub(4000));
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(end - start).read_to_end(&mut bytes)?;
    let output = String::from_utf8_lossy(&bytes);
    let prefix = if start > offset {
        "[Earlier output from this startup omitted]\n"
    } else {
        ""
    };
    if output.trim().is_empty() {
        Ok("The controller did not write an error message during this startup.".into())
    } else {
        Ok(format!("{prefix}{}", output.trim()))
    }
}
impl Client {
    pub fn connect_interactive(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        let client = Self {
            socket: socket_path(&path)?,
        };
        // Settings often reuse a controller that is already open. Avoid a
        // loading surface (and its input handling) for that ordinary RPC.
        if let Ok(status) = client.request(json!({"action":"status"}))
            && status["version"] == env!("CARGO_PKG_VERSION")
            && status["config"].as_str() == path.to_str()
            && status["closing"] != true
        {
            return Ok(client);
        }
        crate::ui::busy("Opening project controller", move || Self::connect(&path))
    }

    pub fn connect(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        let client = Self {
            socket: socket_path(&path)?,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let _startup = startup_lock(&client.socket, deadline)?;
        if let Ok(status) = client.request(json!({"action":"status"})) {
            ensure!(
                status["config"].as_str() == path.to_str(),
                "This checkout is already controlled with a different project configuration"
            );
            let closing = status["closing"] == true;
            if status["version"] == env!("CARGO_PKG_VERSION") && !closing {
                return Ok(client);
            }
            crate::events::send(crate::events::Event::Phase(
                "Closing previous controller… waiting for shutdown to finish".into(),
            ));
            if !closing {
                ensure!(
                    status["running"] == false,
                    "The previous Chuggin version ({}) still has a running or paused loop. Stop that loop in its original window before opening this version. Saved files remain available.",
                    status["version"].as_str().unwrap_or("unknown"),
                );
                // The old owner must be idle. Loop-owned commands receive targeted
                // cleanup and a saved explanation; operator commands still block upgrade.
                client.request(json!({"action":"shutdown","cleanup_commands":true,"reason":"Controller version upgrade; loop-owned commands closed before restarting. Resume by inspecting saved files and command evidence."})).context(
                    "The previous Chuggin controller still has active work. Finish its chat, editing session or commands in the original window before upgrading",
                )?;
            }
            while client.socket.exists() {
                // Another new-version viewer may complete the upgrade first.
                if let Ok(updated) = client.request(json!({"action":"status"}))
                    && updated["version"] == env!("CARGO_PKG_VERSION")
                    && updated["config"].as_str() == path.to_str()
                    && updated["closing"] != true
                {
                    return Ok(client);
                }
                ensure!(
                    Instant::now() < deadline,
                    "The previous controller is still closing. Return home and try Resume project again shortly."
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        // Socket removal announces shutdown before the old process actually
        // exits. Wait for both checkout locks, not just the socket pathname.
        crate::runner::wait_project_idle(&crate::runner::load(&path)?, deadline)?;
        crate::events::send(crate::events::Event::Phase(
            "Starting project controller…".into(),
        ));
        let log_path = client.socket.with_extension("log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let log_offset = log.metadata()?.len();
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("engine")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(log.try_clone()?)
            .spawn()?;
        serde_json::to_writer(
            child
                .stdin
                .take()
                .context("Controller startup pipe missing")?,
            &path,
        )?;
        loop {
            if let Ok(status) = client.request(json!({"action":"status"})) {
                ensure!(
                    status["config"].as_str() == path.to_str()
                        && status["version"] == env!("CARGO_PKG_VERSION"),
                    "Another controller opened this checkout with a different configuration or version"
                );
                std::thread::spawn(move || match child.wait() {
                    Ok(status) if !status.success() => {
                        let _ = writeln!(
                            log,
                            "{} Background controller exited unexpectedly: {status}",
                            chrono::Utc::now()
                        );
                    }
                    Err(error) => {
                        let _ = writeln!(log, "Cannot observe controller exit: {error}");
                    }
                    _ => {}
                });
                return Ok(client);
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!(
                    "Could not open project controller ({status}).\n\n{}\n\nController log: {}",
                    startup_output(&log_path, log_offset)
                        .unwrap_or_else(|error| format!("Could not read startup details: {error}")),
                    log_path.display()
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
            let disconnected = |source| Disconnected {
                socket: self.socket.clone(),
                source,
            };
            let mut stream =
                std::os::unix::net::UnixStream::connect(&self.socket).map_err(disconnected)?;
            stream.set_read_timeout(Some(Duration::from_secs(120)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            let bytes = serde_json::to_vec(&value)?;
            ensure!(bytes.len() < 2_000_000, "Request too large");
            stream.write_all(&bytes).map_err(disconnected)?;
            stream.write_all(b"\n").map_err(disconnected)?;
            let mut line = String::new();
            BufReader::new(stream)
                .take(4_000_000)
                .read_line(&mut line)
                .map_err(disconnected)?;
            ensure!(
                !line.is_empty(),
                Disconnected {
                    socket: self.socket.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "Controller closed the connection without replying",
                    ),
                }
            );
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
    /// Human UI only: this operation is deliberately absent from chat/MCP schemas.
    pub fn update_human_settings(&self, revision: &Value, settings: Value) -> Result<Value> {
        self.request(
            json!({"action":"human_settings","expected_revision":revision,"settings":settings}),
        )
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
            "helper_settings" => self.controller.update_helpers(&v),
            "human_settings" => self.controller.update_human_settings(&v),
            "force_stop" => {
                let _admission = self.controller.lifecycle.lock().unwrap();
                ensure!(
                    !self.controller.closing.load(Ordering::SeqCst),
                    "Controller is already closing; reconnect before continuing"
                );
                self.controller.force_stop_loop_commands()?;
                crate::project::kill_active_check();
                self.controller.closing.store(true, Ordering::SeqCst);
                Ok(json!({"stopped":true}))
            }
            "shutdown" => {
                let _admission = self.controller.lifecycle.lock().unwrap();
                ensure!(
                    !self.controller.closing.load(Ordering::SeqCst),
                    "Controller is already closing; reconnect before continuing"
                );
                ensure!(
                    !self.controller.running.load(Ordering::SeqCst)
                        && !self.chats.any_running()
                        && !self.controller.has_editor(),
                    "Stop the loop and finish chat/editing before quitting the controller"
                );
                if v["cleanup_commands"] == true {
                    self.controller.shutdown_loop_commands(
                        v["reason"].as_str().filter(|reason| !reason.trim().is_empty())
                            .unwrap_or("Controller explicitly closed; loop-owned commands stopped, not verified"),
                    )?;
                } else {
                    ensure!(
                        crate::project::active_checks().is_empty(),
                        "Commands remain owned by the controller. Finish them or explicitly close with cleanup_commands=true"
                    );
                }
                self.controller.closing.store(true, Ordering::SeqCst);
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
                let name = text(&v, "name")?;
                let _admission = if name != "resume_loop"
                    && (name == "begin_edit" || crate::operator::is_mutation(name))
                {
                    Some(self.controller.lifecycle.lock().unwrap())
                } else {
                    None
                };
                ensure!(
                    !self.controller.closing.load(Ordering::SeqCst),
                    "Controller is closing; reconnect before continuing"
                );
                crate::events::set_actor(session);
                self.controller.call(
                    session,
                    name,
                    v["arguments"].clone(),
                    text(&v, "operation_id")?,
                )
            }
            "operation" => crate::operator::read_json(
                &self
                    .controller
                    .operation_path(text(&v, "session_id")?, text(&v, "operation_id")?)?,
            ),
            "chat_send" => {
                let _admission = self.controller.lifecycle.lock().unwrap();
                ensure!(
                    !self.controller.closing.load(Ordering::SeqCst),
                    "Controller is closing; reconnect before continuing"
                );
                self.chats.send(
                    self.controller.clone(),
                    text(&v, "session_id")?,
                    text(&v, "message")?,
                )
            }
            "chat_status" => self.chats.status(&self.controller, text(&v, "session_id")?),
            "chat_cancel" => {
                self.chats.cancel(text(&v, "session_id")?);
                Ok(json!({"cancel_requested":true}))
            }
            _ => anyhow::bail!("Unknown controller action"),
        }
    }
}

/// A successful close request retires admission immediately, but its worker must
/// finish the acknowledgement before the main thread can end the process. Even
/// a disconnected client gets a completed write/flush attempt before quitting.
#[cfg(unix)]
fn write_response(
    writer: &mut impl Write,
    result: Result<Value>,
    close_after_reply: bool,
    quit: &AtomicBool,
) -> std::io::Result<()> {
    let close_after_reply = close_after_reply && result.is_ok();
    let value = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
    };
    let written = writeln!(writer, "{value}");
    let flushed = writer.flush();
    if close_after_reply {
        quit.store(true, Ordering::SeqCst);
    }
    written.and(flushed)
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
                            let mut close_after_reply = false;
                            let result = (|| -> Result<Value> {
                                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                                stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                                let mut line = String::new();
                                BufReader::new(stream.try_clone()?)
                                    .take(2_000_000)
                                    .read_line(&mut line)?;
                                let request: Value = serde_json::from_str(&line)?;
                                close_after_reply = matches!(
                                    request["action"].as_str(),
                                    Some("shutdown" | "force_stop")
                                );
                                worker.request(request)
                            })();
                            let mut stream = stream;
                            let _ = write_response(
                                &mut stream,
                                result,
                                close_after_reply,
                                &worker.quit,
                            );
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
                && !engine.controller.closing.load(Ordering::SeqCst)
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
    let deadline = Instant::now() + Duration::from_secs(10);
    let _startup = startup_lock(&client.socket, deadline)?;
    if client.request(json!({"action":"status"})).is_ok() {
        crate::events::send(crate::events::Event::Phase(
            "Closing project controller… waiting for shutdown to finish".into(),
        ));
        client.request(json!({"action":"shutdown"}))?;
        while client.socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
        }
        ensure!(
            !client.socket.exists(),
            "The project controller is still closing; try again shortly"
        );
    }
    crate::runner::wait_project_idle(&crate::runner::load(path)?, deadline)?;
    Ok(())
}

pub fn close_idle_interactive(path: &Path) -> Result<()> {
    let path = path.to_owned();
    crate::ui::busy("Closing project controller", move || close_idle(&path))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct AcknowledgementWriter<'a> {
        quit: &'a AtomicBool,
        bytes: Vec<u8>,
        flushed: bool,
        broken: bool,
    }
    impl Write for AcknowledgementWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            assert!(
                !self.quit.load(Ordering::SeqCst),
                "Controller quit before its response was written"
            );
            if self.broken {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            // Simulate partial writes: quit must stay false throughout the JSON,
            // including its final newline, rather than after the first chunk.
            let count = bytes.len().min(2);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            assert!(
                !self.quit.load(Ordering::SeqCst),
                "Controller quit before its response was flushed"
            );
            self.flushed = true;
            Ok(())
        }
    }

    #[test]
    fn shutdown_waits_for_the_complete_acknowledgement_and_flush() {
        let quit = AtomicBool::new(false);
        let mut writer = AcknowledgementWriter {
            quit: &quit,
            bytes: vec![],
            flushed: false,
            broken: false,
        };
        write_response(&mut writer, Ok(json!({"closing":true})), true, &quit).unwrap();
        assert!(writer.flushed);
        assert!(quit.load(Ordering::SeqCst));
        assert!(writer.bytes.ends_with(b"\n"));
        let response: Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(response, json!({"ok":true,"result":{"closing":true}}));
    }

    #[test]
    fn rejected_shutdown_does_not_quit_and_a_lost_acknowledgement_still_flushes() {
        let quit = AtomicBool::new(false);
        let mut writer = AcknowledgementWriter {
            quit: &quit,
            bytes: vec![],
            flushed: false,
            broken: false,
        };
        write_response(
            &mut writer,
            Err(anyhow::anyhow!("Loop is still running")),
            true,
            &quit,
        )
        .unwrap();
        assert!(writer.flushed);
        assert!(!quit.load(Ordering::SeqCst));
        let response: Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"], "Loop is still running");
        let mut disconnected = AcknowledgementWriter {
            quit: &quit,
            bytes: vec![],
            flushed: false,
            broken: true,
        };
        assert!(
            write_response(&mut disconnected, Ok(json!({"stopped":true})), true, &quit).is_err()
        );
        assert!(disconnected.flushed);
        assert!(quit.load(Ordering::SeqCst));
    }

    #[test]
    fn startup_error_omits_logs_from_previous_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.log");
        let old = "OLD_RUN_OUTPUT\n".repeat(1000);
        fs::write(&path, &old).unwrap();
        let mut log = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(log, "CURRENT_STARTUP_FAILURE").unwrap();
        let output = startup_output(&path, old.len() as u64).unwrap();
        assert_eq!(output, "CURRENT_STARTUP_FAILURE");
        assert!(!output.contains("OLD_RUN_OUTPUT"));
    }

    #[test]
    fn startup_error_keeps_a_bounded_tail_and_handles_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.log");
        fs::write(&path, format!("{}\nLAST_FAILURE", "é".repeat(4000))).unwrap();
        let output = startup_output(&path, 0).unwrap();
        assert!(output.starts_with("[Earlier output from this startup omitted]"));
        assert!(output.ends_with("LAST_FAILURE"));
        assert!(output.len() < 4100);
    }

    #[test]
    fn empty_startup_does_not_reuse_an_old_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.log");
        fs::write(&path, "OLD_FAILURE").unwrap();
        let output = startup_output(&path, 11).unwrap();
        assert!(output.contains("did not write an error message"));
        assert!(!output.contains("OLD_FAILURE"));
    }

    #[test]
    fn startup_admission_waits_for_other_viewer_and_preserves_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("controller.sock");
        let owner = startup_lock(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let lock_path = socket.with_extension("startup.lock");
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&lock_path).unwrap().ino()
        };
        assert!(startup_lock(&socket, Instant::now()).is_err());
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(owner);
        });
        let next = startup_lock(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        release.join().unwrap();
        assert!(lock_path.exists());
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&lock_path).unwrap().ino(), inode);
        drop(next);
    }

    fn request_with_reply(reply: &'static str) -> anyhow::Error {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("controller.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let error = Client { socket }
            .request(json!({"action":"status"}))
            .unwrap_err();
        worker.join().unwrap();
        error
    }

    #[test]
    fn missing_local_controller_is_a_distinct_transport_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("missing.sock");
        let error = Client {
            socket: socket.clone(),
        }
        .request(json!({"action":"status"}))
        .unwrap_err();
        let disconnected = error.downcast_ref::<Disconnected>().unwrap();
        assert_eq!(disconnected.socket, socket);
        assert!(error.to_string().contains("background controller"));
    }

    #[test]
    fn controller_exiting_without_a_reply_offers_transport_recovery() {
        let error = request_with_reply("");
        assert_eq!(
            error.downcast_ref::<Disconnected>().unwrap().source.kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn provider_failure_returned_by_controller_is_not_a_disconnect() {
        let error = request_with_reply(
            "{\"ok\":false,\"error\":\"Connection refused by inference provider\"}\n",
        );
        assert!(error.downcast_ref::<Disconnected>().is_none());
        assert_eq!(
            error.to_string(),
            "Connection refused by inference provider"
        );
    }
}
