//! Owned command sessions. Yielding never kills a process; dropping its owner does.
use crate::{
    events::{self, Event},
    project::{Check, CheckResult},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Managed {
    handle: String,
    actor: String,
    id: String,
    log: PathBuf,
    argv: Vec<String>,
    child: std::sync::Weak<std::sync::Mutex<Child>>,
    exited: std::sync::Arc<std::sync::Mutex<Option<std::process::ExitStatus>>>,
    stopped: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    descendant_cleanup: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    mode: std::sync::Arc<std::sync::Mutex<String>>,
    purpose: std::sync::Arc<std::sync::Mutex<String>>,
    persist_guard: std::sync::Arc<std::sync::Mutex<()>>,
    pid: i32,
    started: Instant,
}
static MANAGED: std::sync::LazyLock<std::sync::Mutex<Vec<Managed>>> =
    std::sync::LazyLock::new(Default::default);
pub fn managed_commands() -> Vec<Value> {
    MANAGED.lock().unwrap().iter().rev().take(100).map(|m| json!({"command_id":m.handle,"actor":m.actor,"job_id":m.id,"pid":m.pid,"argv":m.argv,"mode":*m.mode.lock().unwrap(),"purpose":*m.purpose.lock().unwrap(),"running":m.exited.lock().unwrap().is_none() && m.stopped.lock().unwrap().is_none() && m.child.strong_count()>0,"stop_reason":*m.stopped.lock().unwrap(),"descendant_cleanup_reason":*m.descendant_cleanup.lock().unwrap(),"log":m.log,"elapsed_seconds":m.started.elapsed().as_secs()})).collect()
}
/// Background apps remain observable while finite workspace work retains its barrier.
pub fn workspace_checks() -> Vec<i32> {
    let background: Vec<i32> = MANAGED
        .lock()
        .unwrap()
        .iter()
        .filter(|managed| {
            managed.exited.lock().unwrap().is_none()
                && managed.child.strong_count() > 0
                && *managed.mode.lock().unwrap() == "background"
        })
        .map(|managed| managed.pid)
        .collect();
    crate::project::active_checks()
        .into_iter()
        .filter(|pid| !background.contains(pid))
        .collect()
}

/// Save a reason before a force stop even while the worker owns its job registry.
pub fn stop_actor(actor: &str, reason: &str) -> Result<Vec<Value>> {
    let handles: Vec<String> = MANAGED
        .lock()
        .unwrap()
        .iter()
        .filter(|managed| {
            managed.actor == actor
                && managed.exited.lock().unwrap().is_none()
                && managed.child.strong_count() > 0
        })
        .map(|managed| managed.handle.clone())
        .collect();
    handles
        .iter()
        .map(|handle| manage(handle, "stop_command", &json!({"reason":reason})))
        .collect()
}

fn persist_managed_state(managed: &Managed) -> Result<()> {
    let _guard = managed.persist_guard.lock().unwrap();
    let path = managed.log.with_extension("session.json");
    let mut value: Value = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({"command_id":managed.id,"argv":managed.argv,"pid":managed.pid}));
    let exit = *managed.exited.lock().unwrap();
    let stopped = managed.stopped.lock().unwrap().clone();
    value["running"] =
        json!(exit.is_none() && stopped.is_none() && managed.child.strong_count() > 0);
    value["exit_code"] = json!(exit.and_then(|status| status.code()));
    value["passed"] = json!(if stopped.is_some() {
        Some(false)
    } else {
        exit.map(|status| status.success())
    });
    value["stop_reason"] = json!(stopped);
    value["descendant_cleanup_reason"] = json!(*managed.descendant_cleanup.lock().unwrap());
    value["mode"] = json!(*managed.mode.lock().unwrap());
    value["purpose"] = json!(*managed.purpose.lock().unwrap());
    fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}
pub fn manage(id: &str, action: &str, args: &Value) -> Result<Value> {
    anyhow::ensure!(
        matches!(action, "command_status" | "command_input" | "stop_command"),
        "Unknown command action"
    );
    let m = MANAGED
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m.handle == id)
        .cloned()
        .context("Unknown command; use list_commands")?;
    let mut written = None;
    if action == "stop_command" {
        let reason = args["reason"]
            .as_str()
            .filter(|reason| !reason.trim().is_empty())
            .context("Supply a reason for stopping this command")?;
        if let Some(child) = m.child.upgrade() {
            let mut child = child.lock().unwrap();
            if m.exited.lock().unwrap().is_none() {
                if let Some(status) = child.try_wait()? {
                    // A successful completed process wins a stale stop request.
                    *m.exited.lock().unwrap() = Some(status);
                } else {
                    *m.stopped.lock().unwrap() = Some(reason.into());
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    if let Err(error) = child.kill()
                        && child.try_wait()?.is_none()
                    {
                        return Err(error.into());
                    }
                    let status = child.wait()?;
                    if status.success() {
                        *m.stopped.lock().unwrap() = None;
                    }
                    *m.exited.lock().unwrap() = Some(status);
                }
                crate::project::unregister_check(child.id() as i32);
            }
        }
        persist_managed_state(&m)?;
    } else if action == "command_input" {
        let child = m.child.upgrade().context("Command has ended")?;
        let mut child = child.lock().unwrap();
        anyhow::ensure!(m.exited.lock().unwrap().is_none(), "Command has ended");
        let text = args["text"].as_str().unwrap_or("");
        anyhow::ensure!(text.len() <= 16000, "Input exceeds 16000 bytes");
        let stdin = child.stdin.as_mut().context("stdin is closed")?;
        let n = match stdin.write(text.as_bytes()) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e.into()),
        };
        if args["close_stdin"] == true && n == text.len() {
            child.stdin.take();
        }
        written = Some(n);
    }
    let log_id = evidence_log_id(&m.log);
    let requested = if action == "command_status" {
        args.get("output_offset")
            .map(|offset| offset.as_u64().context("output_offset must be nonnegative"))
            .transpose()?
    } else {
        None
    };
    let log_changed = action == "command_status"
        && args["output_log_id"]
            .as_str()
            .is_some_and(|id| id != log_id);
    let offset = if log_changed {
        0
    } else {
        requested.unwrap_or(0)
    };
    let initial_bytes = fs::metadata(&m.log)?.len();
    anyhow::ensure!(
        offset <= initial_bytes,
        "output_offset exceeds log size; use offset 0 for this log_id"
    );
    if action == "command_status" {
        let wait = args
            .get("wait_ms")
            .map(|wait| wait.as_u64().context("wait_ms must be nonnegative"))
            .transpose()?
            .unwrap_or(0);
        anyhow::ensure!(wait <= 60000, "wait_ms must be between 0 and 60000");
        let wait_offset = requested.map(|_| offset).unwrap_or(initial_bytes);
        let deadline = Instant::now() + Duration::from_millis(wait);
        loop {
            // No registry or process mutex is held while waiting. An independent
            // operator can stop the command, and its owning Session can finish.
            let running = m.exited.lock().unwrap().is_none()
                && m.stopped.lock().unwrap().is_none()
                && m.child.strong_count() > 0;
            if !running || fs::metadata(&m.log)?.len() > wait_offset || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
    let mut f = fs::File::open(&m.log)?;
    let bytes = f.metadata()?.len();
    f.seek(SeekFrom::Start(bytes.saturating_sub(7000)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let exit = *m.exited.lock().unwrap();
    let stopped = m.stopped.lock().unwrap().clone();
    let running = exit.is_none() && stopped.is_none() && m.child.strong_count() > 0;
    let mut value = json!({"command_id":m.handle,"job_id":m.id,"actor":m.actor,"pid":m.pid,"argv":m.argv,"mode":*m.mode.lock().unwrap(),"purpose":*m.purpose.lock().unwrap(),"running":exit.is_none() && stopped.is_none() && m.child.strong_count()>0,"exit_code":exit.and_then(|e|e.code()),"passed":if stopped.is_some(){Some(false)}else{exit.map(|e|e.success())},"stop_reason":stopped,"descendant_cleanup_reason":*m.descendant_cleanup.lock().unwrap(),"output_tail":String::from_utf8_lossy(&tail),"log":m.log,"log_id":log_id,"log_bytes":bytes,"written_bytes":written,"next_output_offset":bytes});
    value["lifecycle_instruction"] = json!(if stopped.is_some() {
        "Chuggin intentionally stopped this process for the recorded stop_reason. Interrupted commands are not passing validation; do not treat this as an unexplained application crash."
    } else if m.descendant_cleanup.lock().unwrap().is_some() {
        "The parent command exited and Chuggin cleaned up its descendants. Launch applications or servers directly with mode=background instead of shell backgrounding with &."
    } else {
        "This command stays owned by its controller. Use the existing command_id for status, input or targeted stop. A running process is not passing validation."
    });
    if action == "command_status" {
        value.as_object_mut().unwrap().remove("output_tail");
        value.as_object_mut().unwrap().extend(
            read_incremental_output(&m.log, offset, running)?
                .as_object()
                .unwrap()
                .clone(),
        );
        value["output_log_changed"] = json!(log_changed);
    }
    Ok(value)
}

fn evidence_log_id(log: &Path) -> String {
    format!(
        "{}/{}",
        log.parent()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy(),
        log.file_name().unwrap_or_default().to_string_lossy()
    )
}

fn read_incremental_output(log: &Path, offset: u64, running: bool) -> Result<Value> {
    let mut file = fs::File::open(log)?;
    let total = file.metadata()?.len();
    anyhow::ensure!(
        offset <= total,
        "output_offset exceeds log size; use offset 0 for this log_id"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(12000).read_to_end(&mut bytes)?;
    let read_end = offset + bytes.len() as u64;
    // Cursors preserve boundaries of valid UTF-8 text; non-UTF-8 command
    // output is still displayed with replacement characters.
    let mut pending_utf8 = false;
    if let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
        && (error.valid_up_to() > 0 || running)
    {
        bytes.truncate(error.valid_up_to());
        pending_utf8 = running && read_end == total;
    }
    let next = offset + bytes.len() as u64;
    Ok(
        json!({"output":String::from_utf8_lossy(&bytes),"output_offset":offset,"next_output_offset":next,"output_truncated":next < total,"output_pending_utf8":pending_utf8,"log_bytes":total}),
    )
}
pub struct Session {
    pub id: String,
    pub log: PathBuf,
    pub argv: Vec<String>,
    child: std::sync::Arc<std::sync::Mutex<Child>>,
    pid: u32,
    exited: std::sync::Arc<std::sync::Mutex<Option<std::process::ExitStatus>>>,
    started: Instant,
    next_review: Instant,
    pub reviews: u64,
    pub result: Option<CheckResult>,
    pub stop_reason: Option<String>,
    external_stop: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    descendant_cleanup: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    mode: std::sync::Arc<std::sync::Mutex<String>>,
    purpose: std::sync::Arc<std::sync::Mutex<String>>,
    persist_guard: std::sync::Arc<std::sync::Mutex<()>>,
}
impl Session {
    pub fn start(root: &Path, c: &Check, log: &Path, id: &str) -> Result<Self> {
        crate::dev_tools::validate_argv(&c.argv)?;
        let file = fs::File::create(log)?;
        let mut cmd = Command::new(&c.argv[0]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .args(&c.argv[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .context("Cannot start command")?;
        // Avoid blocking the agent if a process stops consuming stdin.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if let Some(stdin) = &mut child.stdin {
                let fd = stdin.as_raw_fd();
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
            }
        }
        crate::project::register_check(child.id() as i32);
        events::send(Event::Check {
            command: c.argv.join(" "),
            path: log.into(),
        });
        let pid = child.id();
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        let exited = std::sync::Arc::new(std::sync::Mutex::new(None));
        let external_stop = std::sync::Arc::new(std::sync::Mutex::new(None));
        let descendant_cleanup = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mode = std::sync::Arc::new(std::sync::Mutex::new("finite".to_owned()));
        let purpose = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let persist_guard = std::sync::Arc::new(std::sync::Mutex::new(()));
        let mut registry = MANAGED.lock().unwrap();
        if registry.len() >= 1024
            && let Some(index) = registry.iter().position(|m| m.child.strong_count() == 0)
        {
            registry.remove(index);
        }
        let managed = Managed {
            handle: crate::operator::id(),
            actor: events::actor(),
            id: id.into(),
            log: log.into(),
            argv: c.argv.clone(),
            child: std::sync::Arc::downgrade(&child),
            exited: exited.clone(),
            stopped: external_stop.clone(),
            descendant_cleanup: descendant_cleanup.clone(),
            mode: mode.clone(),
            purpose: purpose.clone(),
            persist_guard: persist_guard.clone(),
            pid: pid as i32,
            started: Instant::now(),
        };
        registry.push(managed.clone());
        drop(registry);
        let monitored = child.clone();
        let observed = exited.clone();
        std::thread::spawn(move || {
            loop {
                let mut child = monitored.lock().unwrap();
                let status = child.try_wait();
                if let Ok(Some(status)) = status {
                    #[cfg(unix)]
                    unsafe {
                        if libc::kill(-(pid as i32), 0) == 0 {
                            *managed.descendant_cleanup.lock().unwrap() = Some("Parent command exited; Chuggin cleaned up its remaining process-group descendants. Launch apps or servers directly with mode=background instead of shell backgrounding with &.".into());
                        }
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                    crate::project::unregister_check(pid as i32);
                    *observed.lock().unwrap() = Some(status);
                    let _ = persist_managed_state(&managed);
                    break;
                }
                if status.is_err() {
                    break;
                }
                drop(child);
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let session = Self {
            pid,
            exited,
            id: id.into(),
            log: log.into(),
            argv: c.argv.clone(),
            child,
            started: Instant::now(),
            next_review: Instant::now() + Duration::from_secs(c.timeout_seconds.clamp(1, 86400)),
            reviews: 0,
            result: None,
            stop_reason: None,
            external_stop,
            descendant_cleanup,
            mode,
            purpose,
            persist_guard,
        };
        session.persist()?;
        Ok(session)
    }
    pub fn pid(&self) -> i32 {
        self.pid as i32
    }
    pub fn set_mode(&mut self, mode: &str, purpose: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(mode, "finite" | "background"),
            "Unknown command mode"
        );
        anyhow::ensure!(
            mode == "finite" || !purpose.trim().is_empty(),
            "Explain the purpose of a background command"
        );
        *self.mode.lock().unwrap() = mode.into();
        *self.purpose.lock().unwrap() = purpose.trim().into();
        self.persist()
    }
    pub fn running(&self) -> bool {
        self.result.is_none()
    }
    pub fn due(&self) -> bool {
        self.running() && Instant::now() >= self.next_review
    }
    pub fn extend(&mut self, seconds: u64) {
        self.next_review = Instant::now() + Duration::from_secs(seconds.clamp(1, 900));
    }
    fn cleanup(&mut self) {
        let mut child = self.child.lock().unwrap();
        if self.exited.lock().unwrap().is_none() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                *self.exited.lock().unwrap() = Some(status);
            }
        }
        crate::project::unregister_check(self.pid as i32);
    }
    pub fn tail(&self) -> Result<String> {
        let mut f = fs::File::open(&self.log)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(7000)))?;
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        Ok(String::from_utf8_lossy(&b).into())
    }
    fn finish(&mut self, exit_code: Option<i32>, passed: bool) -> Result<()> {
        self.cleanup();
        self.result = Some(CheckResult {
            exit_code,
            argv: self.argv.clone(),
            passed,
            timed_out: false,
            output: self.tail()?,
        });
        events::send(Event::CheckDone(passed));
        self.persist()?;
        Ok(())
    }
    pub fn poll(&mut self, milliseconds: u64, stop: &AtomicBool) -> Result<Value> {
        self.poll_output(milliseconds, None, stop, None)
    }
    /// A supplied byte offset yields on fresh log evidence as well as completion.
    /// Checking pause and cancellation during the wait keeps observation controls
    /// responsive without killing an otherwise legitimate long-running process.
    pub fn poll_output(
        &mut self,
        milliseconds: u64,
        output_offset: Option<u64>,
        stop: &AtomicBool,
        controls: Option<&crate::run_control::RunControl>,
    ) -> Result<Value> {
        if let Some(reason) = self.external_stop.lock().unwrap().clone() {
            self.stop_reason = Some(reason);
        }
        let deadline = Instant::now() + Duration::from_millis(milliseconds.min(60000));
        while self.running() {
            if let Some(controls) = controls {
                controls.wait_until_resumed(|| stop.load(Ordering::SeqCst));
            }
            let exit_status = *self.exited.lock().unwrap();
            if let Some(status) = exit_status {
                self.finish(
                    status.code(),
                    status.success() && self.stop_reason.is_none(),
                )?;
                break;
            }
            if stop.load(Ordering::SeqCst) {
                self.terminate("Stopped by operator")?;
                break;
            }
            if output_offset.is_some_and(|offset| {
                fs::metadata(&self.log).is_ok_and(|metadata| metadata.len() > offset)
            }) {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let v = self.snapshot()?;
        events::send(Event::CommandStatus {
            id: self.id.clone(),
            elapsed: self.started.elapsed().as_secs(),
            next_review: self
                .next_review
                .saturating_duration_since(Instant::now())
                .as_secs(),
            running: self.running(),
        });
        self.persist()?;
        Ok(v)
    }
    pub fn output_since(&self, offset: u64) -> Result<Value> {
        read_incremental_output(&self.log, offset, self.running())
    }
    pub fn terminate(&mut self, reason: &str) -> Result<()> {
        // A process may have completed while its watchdog was considering the evidence.
        if !self.running() {
            return Ok(());
        }
        if let Some(stopped) = self.external_stop.lock().unwrap().clone() {
            self.stop_reason = Some(stopped);
        }
        let mut exit_status = *self.exited.lock().unwrap();
        if exit_status.is_none() {
            // Check the child directly as well as the async monitor: a process
            // can finish between the last observation and a stop request.
            exit_status = self.child.lock().unwrap().try_wait()?;
            if let Some(status) = exit_status {
                *self.exited.lock().unwrap() = Some(status);
            }
        }
        if let Some(status) = exit_status {
            return self.finish(
                status.code(),
                status.success() && self.stop_reason.is_none(),
            );
        }
        self.stop_reason = Some(reason.into());
        *self.external_stop.lock().unwrap() = Some(reason.into());
        self.finish(None, false)
    }
    pub fn input(&mut self, text: &str, close: bool) -> Result<usize> {
        anyhow::ensure!(self.running(), "Command has finished");
        anyhow::ensure!(text.len() <= 16000, "Input exceeds 16000 bytes");
        let mut child = self.child.lock().unwrap();
        let stdin = child.stdin.as_mut().context("stdin is closed")?;
        let written = match stdin.write(text.as_bytes()) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e.into()),
        };
        if close && written == text.len() {
            child.stdin.take();
        }
        Ok(written)
    }
    pub fn snapshot(&self) -> Result<Value> {
        let log_id = evidence_log_id(&self.log);
        let stopped = self
            .stop_reason
            .clone()
            .or_else(|| self.external_stop.lock().unwrap().clone());
        let descendant_cleanup = self.descendant_cleanup.lock().unwrap().clone();
        let observed_exit = *self.exited.lock().unwrap();
        let running = self.running() && observed_exit.is_none() && stopped.is_none();
        let exit_code = self
            .result
            .as_ref()
            .and_then(|result| result.exit_code)
            .or_else(|| observed_exit.and_then(|status| status.code()));
        let passed = if stopped.is_some() {
            Some(false)
        } else {
            self.result
                .as_ref()
                .map(|result| result.passed)
                .or_else(|| observed_exit.map(|status| status.success()))
        };
        Ok(
            json!({"pid":self.pid,"command_id":self.id,"argv":self.argv,"mode":*self.mode.lock().unwrap(),"purpose":*self.purpose.lock().unwrap(),"running":running,"elapsed_seconds":self.started.elapsed().as_secs(),"next_review_seconds":self.next_review.saturating_duration_since(Instant::now()).as_secs(),"watchdog_reviews":self.reviews,"log_bytes":fs::metadata(&self.log)?.len(),"next_output_offset":fs::metadata(&self.log)?.len(),"seconds_since_output":fs::metadata(&self.log)?.modified().ok().and_then(|t|t.elapsed().ok()).map(|d|d.as_secs()),"processes":process_observations(self.pid),"exit_code":exit_code,"passed":passed,"timed_out":false,"stop_reason":stopped,"descendant_cleanup_reason":descendant_cleanup,"output_tail":self.tail()?,"log_id":log_id,"instruction":"Commands stay owned by the controller across turns, cycles and soft stops. Use command_status with wait_ms up to 60000 and the existing command_id for fresh output or completion. Use read_command_log with log_id for older/full output. Finite work must finish before editing its workspace; deliberate background apps can remain open. A running command is not a passing check. If stop_reason or descendant_cleanup_reason is present, Chuggin performed the recorded cleanup; do not mistake it for an unexplained application crash. Launch apps/servers directly with mode=background, without shell &."}),
        )
    }
    fn persist(&self) -> Result<()> {
        let _guard = self.persist_guard.lock().unwrap();
        fs::write(
            self.log.with_extension("session.json"),
            serde_json::to_vec_pretty(&self.snapshot()?)?,
        )?;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.running() {
            let _ = self.terminate("Session owner ended; command interrupted, not verified");
        }
    }
}

/// Linux process-tree counters are evidence, not a hang heuristic. Other platforms
/// still supply elapsed time and log activity. Bounded traversal avoids huge output.
fn process_observations(pid: u32) -> Vec<Value> {
    let mut result = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let mut pending = vec![pid];
        while let Some(id) = pending.pop() {
            if result.len() >= 24 {
                break;
            }
            if let Ok(stat) = fs::read_to_string(format!("/proc/{id}/stat")) {
                if let Some((_, fields)) = stat.rsplit_once(") ") {
                    let f: Vec<_> = fields.split_whitespace().collect();
                    result.push(json!({"pid":id,"state":f.first(),"user_cpu_ticks":f.get(11),"system_cpu_ticks":f.get(12),"resident_pages":f.get(21)}));
                }
                if let Ok(children) = fs::read_to_string(format!("/proc/{id}/task/{id}/children")) {
                    pending.extend(
                        children
                            .split_whitespace()
                            .take(24)
                            .filter_map(|p| p.parse::<u32>().ok()),
                    );
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(script: &str) -> Check {
        Check {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            timeout_seconds: 1,
        }
    }
    #[test]
    fn yielding_preserves_process_and_stdin_and_completion() {
        let d = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let mut s = Session::start(
            d.path(),
            &command("printf ready; read value; printf '\nreceived:%s' \"$value\""),
            &d.path().join("command-test.log"),
            "test",
        )
        .unwrap();
        assert_eq!(s.poll(100, &stop).unwrap()["running"], true);
        assert!(s.tail().unwrap().contains("ready"));
        s.input("hello\n", true).unwrap();
        assert_eq!(s.poll(1000, &stop).unwrap()["passed"], true);
        assert!(s.tail().unwrap().contains("received:hello"));
        assert_eq!(s.poll(0, &stop).unwrap()["passed"], true);
    }
    #[test]
    fn review_deadline_does_not_kill_and_stop_removes_descendants() {
        let d = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let mut s = Session::start(
            d.path(),
            &command("(sleep 2; touch escaped) & wait"),
            &d.path().join("command-test.log"),
            "test",
        )
        .unwrap();
        s.next_review = Instant::now();
        assert!(s.due());
        assert_eq!(s.poll(0, &stop).unwrap()["running"], true);
        s.terminate("Test confirmed runaway").unwrap();
        assert_eq!(s.snapshot().unwrap()["passed"], false);
        thread::sleep(Duration::from_millis(2100));
        assert!(!d.path().join("escaped").exists());
    }
    #[test]
    fn completed_process_wins_over_stale_termination_and_drop_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Session::start(
            d.path(),
            &command("exit 0"),
            &d.path().join("command-test.log"),
            "test",
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        s.terminate("stale diagnosis").unwrap();
        assert!(s.result.as_ref().unwrap().passed);
        assert!(s.stop_reason.is_none());
        {
            let _child = Session::start(
                d.path(),
                &command("sleep 1; touch escaped"),
                &d.path().join("command-drop.log"),
                "drop",
            )
            .unwrap();
        }
        thread::sleep(Duration::from_millis(1100));
        assert!(!d.path().join("escaped").exists());
        let record: Value =
            serde_json::from_slice(&fs::read(d.path().join("command-drop.session.json")).unwrap())
                .unwrap();
        assert_eq!(record["running"], false);
        assert_eq!(record["passed"], false);
        assert!(
            record["stop_reason"]
                .as_str()
                .unwrap()
                .contains("Session owner ended")
        );
    }
    #[test]
    fn waiting_yields_on_new_output_and_cursors_preserve_unicode() {
        let fixture = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let mut session = Session::start(
            fixture.path(),
            &command("sleep 0.2; printf 'héllo'; sleep 2"),
            &fixture.path().join("command-wait.log"),
            "wait",
        )
        .unwrap();
        let start = Instant::now();
        assert_eq!(
            session.poll_output(5000, Some(0), &stop, None).unwrap()["running"],
            true
        );
        assert!(start.elapsed() >= Duration::from_millis(150));
        assert!(start.elapsed() < Duration::from_secs(2));
        let output = session.output_since(0).unwrap();
        assert_eq!(output["output"], "héllo");
        assert_eq!(output["next_output_offset"], 6);
        assert_eq!(session.output_since(6).unwrap()["output"], "");
        session.terminate("Test finished").unwrap();
        // One multibyte character straddles the 12 kB page boundary.
        fs::write(&session.log, format!("{}éending", "x".repeat(11999))).unwrap();
        let first = session.output_since(0).unwrap();
        let next = first["next_output_offset"].as_u64().unwrap();
        let second = session.output_since(next).unwrap();
        assert_eq!(
            format!(
                "{}{}",
                first["output"].as_str().unwrap(),
                second["output"].as_str().unwrap()
            ),
            format!("{}éending", "x".repeat(11999))
        );
    }
    #[test]
    fn extended_wait_is_cancelled_promptly_and_honors_pause() {
        use std::sync::Arc;
        let fixture = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let controls = Arc::new(crate::run_control::RunControl::default());
        let mut session = Session::start(
            fixture.path(),
            &command("sleep 10"),
            &fixture.path().join("command-cancel.log"),
            "cancel",
        )
        .unwrap();
        let control_thread = controls.clone();
        let stop_thread = stop.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            control_thread.toggle_pause();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !control_thread.is_paused() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let paused = control_thread.is_paused();
            stop_thread.store(true, Ordering::SeqCst);
            paused
        });
        let started = Instant::now();
        let reply = session
            .poll_output(60000, Some(0), &stop, Some(&controls))
            .unwrap();
        assert!(
            worker.join().unwrap(),
            "Pause should be acknowledged while waiting"
        );
        assert_eq!(reply["running"], false);
        assert_eq!(reply["passed"], false);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn operator_observation_waits_for_fresh_output_and_another_operator_can_stop_it() {
        let fixture = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let mut session = Session::start(
            fixture.path(),
            &command("printf ready; read value; printf 'héllo'; sleep 10"),
            &fixture.path().join("command-observer.log"),
            "operator-observer",
        )
        .unwrap();
        session.poll(100, &stop).unwrap();
        let handle = MANAGED
            .lock()
            .unwrap()
            .iter()
            .find(|managed| managed.log == session.log)
            .unwrap()
            .handle
            .clone();
        let first = manage(&handle, "command_status", &json!({"wait_ms":0})).unwrap();
        assert_eq!(first["output"], "ready");
        let waiter_handle = handle.clone();
        let waiter = thread::spawn(move || {
            manage(&waiter_handle, "command_status", &json!({"wait_ms":5000,"output_offset":first["next_output_offset"],"output_log_id":first["log_id"]})).unwrap()
        });
        thread::sleep(Duration::from_millis(100));
        session.input("go\n", false).unwrap();
        let second = waiter.join().unwrap();
        assert_eq!(second["output"], "héllo");
        assert_eq!(second["running"], true);
        let waiter_handle = handle.clone();
        let waiter = thread::spawn(move || {
            manage(&waiter_handle, "command_status", &json!({"wait_ms":60000,"output_offset":second["next_output_offset"],"output_log_id":second["log_id"]})).unwrap()
        });
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        manage(
            &handle,
            "stop_command",
            &json!({"reason":"Operator ended the probe"}),
        )
        .unwrap();
        assert!(!crate::project::active_checks().contains(&(session.pid as i32)));
        let final_status = waiter.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(final_status["running"], false);
        assert_eq!(final_status["passed"], false);
        assert_eq!(final_status["output"], "");
        assert_eq!(final_status["stop_reason"], "Operator ended the probe");
        let record: Value =
            serde_json::from_slice(&fs::read(session.log.with_extension("session.json")).unwrap())
                .unwrap();
        assert_eq!(record["stop_reason"], "Operator ended the probe");
        assert_eq!(record["passed"], false);
    }
    #[test]
    fn background_intent_is_visible_to_operators_and_releases_only_its_workspace_barrier() {
        let fixture = tempfile::tempdir().unwrap();
        let mut app = Session::start(
            fixture.path(),
            &command("sleep 10"),
            &fixture.path().join("command-app.log"),
            "app",
        )
        .unwrap();
        let finite = Session::start(
            fixture.path(),
            &command("sleep 10"),
            &fixture.path().join("command-build.log"),
            "build",
        )
        .unwrap();
        assert!(workspace_checks().contains(&app.pid()));
        app.set_mode("background", "Inspect the running application")
            .unwrap();
        assert!(!workspace_checks().contains(&app.pid()));
        assert!(workspace_checks().contains(&finite.pid()));
        let managed = managed_commands()
            .into_iter()
            .find(|record| record["log"] == json!(app.log))
            .unwrap();
        let status = manage(
            managed["command_id"].as_str().unwrap(),
            "command_status",
            &json!({}),
        )
        .unwrap();
        assert_eq!(status["mode"], "background");
        assert_eq!(status["purpose"], "Inspect the running application");
        assert_eq!(status["passed"], Value::Null);
        app.terminate("Inspection completed").unwrap();
        let status = manage(
            managed["command_id"].as_str().unwrap(),
            "command_status",
            &json!({}),
        )
        .unwrap();
        assert_eq!(status["stop_reason"], "Inspection completed");
        assert_eq!(status["passed"], false);
    }
    #[cfg(unix)]
    #[test]
    fn a_parent_exit_records_descendant_cleanup_without_inventing_a_parent_failure() {
        let fixture = tempfile::tempdir().unwrap();
        let mut session = Session::start(
            fixture.path(),
            &command("sleep 10 & exit 0"),
            &fixture.path().join("command-shell.log"),
            "shell",
        )
        .unwrap();
        let status = session.poll(1000, &AtomicBool::new(false)).unwrap();
        assert_eq!(status["passed"], true);
        assert_eq!(status["stop_reason"], Value::Null);
        assert!(
            status["descendant_cleanup_reason"]
                .as_str()
                .unwrap()
                .contains("Parent command exited")
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(session.log.with_extension("session.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved["descendant_cleanup_reason"],
            status["descendant_cleanup_reason"]
        );
    }
    #[test]
    fn operator_observation_can_wait_past_a_second_for_completion() {
        let fixture = tempfile::tempdir().unwrap();
        let session = Session::start(
            fixture.path(),
            &command("sleep 1.2; exit 0"),
            &fixture.path().join("command-observer-complete.log"),
            "operator-observer-complete",
        )
        .unwrap();
        let handle = MANAGED
            .lock()
            .unwrap()
            .iter()
            .find(|managed| managed.log == session.log)
            .unwrap()
            .handle
            .clone();
        let start = Instant::now();
        let status = manage(
            &handle,
            "command_status",
            &json!({"wait_ms":5000,"output_offset":0}),
        )
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(1100));
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(status["running"], false);
        assert_eq!(status["passed"], true);
        assert!(status.get("output_tail").is_none());
        assert_eq!(status["next_output_offset"], 0);
        let stale_stop = manage(
            &handle,
            "stop_command",
            &json!({"reason":"Stale stop request"}),
        )
        .unwrap();
        assert_eq!(stale_stop["passed"], true);
        assert_eq!(stale_stop["stop_reason"], Value::Null);
    }
    #[test]
    fn a_character_written_in_parts_keeps_its_cursor_until_complete() {
        let fixture = tempfile::tempdir().unwrap();
        let log = fixture.path().join("command-utf8.log");
        fs::write(&log, [0xc3]).unwrap();
        let pending = read_incremental_output(&log, 0, true).unwrap();
        assert_eq!(pending["output"], "");
        assert_eq!(pending["next_output_offset"], 0);
        assert_eq!(pending["output_pending_utf8"], true);
        fs::write(&log, [0xc3, 0xa9]).unwrap();
        assert_eq!(
            read_incremental_output(&log, 0, true).unwrap()["output"],
            "é"
        );
        fs::write(&log, [0xc3]).unwrap();
        let ended = read_incremental_output(&log, 0, false).unwrap();
        assert_eq!(ended["next_output_offset"], 1);
        assert_eq!(ended["output_pending_utf8"], false);
    }
}
