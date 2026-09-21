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

pub struct Session {
    pub id: String,
    pub log: PathBuf,
    pub argv: Vec<String>,
    child: Child,
    started: Instant,
    next_review: Instant,
    pub reviews: u64,
    pub result: Option<CheckResult>,
    pub stop_reason: Option<String>,
}
impl Session {
    pub fn start(root: &Path, c: &Check, log: &Path, id: &str) -> Result<Self> {
        anyhow::ensure!(!c.argv.is_empty(), "Command argv must not be empty");
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
        Ok(Self {
            id: id.into(),
            log: log.into(),
            argv: c.argv.clone(),
            child,
            started: Instant::now(),
            next_review: Instant::now() + Duration::from_secs(c.timeout_seconds.clamp(1, 86400)),
            reviews: 0,
            result: None,
            stop_reason: None,
        })
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
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        crate::project::unregister_check(self.child.id() as i32);
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
        let deadline = Instant::now() + Duration::from_millis(milliseconds.min(1000));
        while self.running() {
            if let Some(status) = self.child.try_wait()? {
                self.finish(status.code(), status.success())?;
                break;
            }
            if stop.load(Ordering::SeqCst) {
                self.terminate("Stopped by operator")?;
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
    pub fn terminate(&mut self, reason: &str) -> Result<()> {
        // A process may have completed while its watchdog was considering the evidence.
        if !self.running() {
            return Ok(());
        }
        if let Some(status) = self.child.try_wait()? {
            return self.finish(status.code(), status.success());
        }
        self.stop_reason = Some(reason.into());
        self.finish(None, false)
    }
    pub fn input(&mut self, text: &str, close: bool) -> Result<usize> {
        anyhow::ensure!(self.running(), "Command has finished");
        anyhow::ensure!(text.len() <= 16000, "Input exceeds 16000 bytes");
        let stdin = self.child.stdin.as_mut().context("stdin is closed")?;
        let written = match stdin.write(text.as_bytes()) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e.into()),
        };
        if close && written == text.len() {
            self.child.stdin.take();
        }
        Ok(written)
    }
    pub fn snapshot(&self) -> Result<Value> {
        let log_id = format!(
            "{}/{}",
            self.log
                .parent()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy(),
            self.log.file_name().unwrap_or_default().to_string_lossy()
        );
        Ok(
            json!({"command_id":self.id,"argv":self.argv,"running":self.running(),"elapsed_seconds":self.started.elapsed().as_secs(),"next_review_seconds":self.next_review.saturating_duration_since(Instant::now()).as_secs(),"watchdog_reviews":self.reviews,"log_bytes":fs::metadata(&self.log)?.len(),"seconds_since_output":fs::metadata(&self.log)?.modified().ok().and_then(|t|t.elapsed().ok()).map(|d|d.as_secs()),"processes":process_observations(self.child.id()),"exit_code":self.result.as_ref().and_then(|r|r.exit_code),"passed":self.result.as_ref().map(|r|r.passed),"timed_out":false,"stop_reason":self.stop_reason,"output_tail":self.tail()?,"log_id":log_id,"instruction":"If running, inspect relevant evidence or use command_status to wait. Do not launch duplicates or edit files being checked. A running command is not a passing check."}),
        )
    }
    fn persist(&self) -> Result<()> {
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
    }
}
