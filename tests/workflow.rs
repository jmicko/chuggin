use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
const BIN: &str = env!("CARGO_BIN_EXE_lupin");

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    done: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(wizard: bool, delay: bool) -> Self {
        Self::with_probe(wizard, delay, None)
    }
    fn with_probe(wizard: bool, delay: bool, probe: Option<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((key, v)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let data = if first.starts_with("GET") {
                    json!({"models":[{"name":"fake"}]}).to_string()
                } else {
                    let mut data = vec![0; length];
                    reader.read_exact(&mut data).unwrap();
                    let body: Value = serde_json::from_slice(&data).unwrap();
                    let mut requests = calls.lock().unwrap();
                    requests.push(body.clone());
                    let n = requests.len() - 1;
                    drop(requests);
                    if delay {
                        thread::sleep(Duration::from_millis(500));
                    }
                    let stage = n % 5;
                    let cycle = n / 5 + 1;
                    let (content, tools) = if wizard {
                        (
                            json!({"goal":format!("Build a useful editor. Draft {}.", n+1)})
                                .to_string(),
                            json!([]),
                        )
                    } else if let Some(invalid) = probe {
                        probe_reply(&body, invalid)
                    } else {
                        match stage {
                            0 => (json!({"gap":"Improve value","why_now":"Next increment","files":["value.txt"]}).to_string(),json!([])),
                            1 => (json!({"title":format!("Increment {cycle}"),"objective":"Improve value","acceptance":["Value updated"],"files":["value.txt"],"out_of_scope":["Other files"]}).to_string(),json!([])),
                            2 => ("".into(),json!([{"function":{"name":"write_file","arguments":{"path":"value.txt","content":cycle.to_string()}}}])),
                            3 => ("PRIVATE_IMPLEMENTATION_TRANSCRIPT".into(),json!([])),
                            _ => (json!({"decision":"accept","reason":"Diff changes value","criteria":[{"criterion":"Value updated","passed":true,"evidence":"value.txt diff"}]}).to_string(),json!([])),
                        }
                    };
                    json!({"message":{"role":"assistant","content":content,"tool_calls":tools},"done":true,"done_reason":"stop"}).to_string()+"
"
                };
                let response = format!(
                    "HTTP/1.1 200 OK
Content-Length: {}
Connection: close

{}",
                    data.len(),
                    data
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            requests,
            done,
            handle: Some(handle),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.handle.take().unwrap().join();
    }
}
fn git(repo: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn fixture(root: &Path, url: &str, pass: bool) -> std::path::PathBuf {
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("value.txt"), "0").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Initial",
        ],
    );
    fs::write(repo.join("value.txt"), "USER_EDIT").unwrap();
    // Git itself supplies deterministic passing/failing checks without another interpreter.
    let argv = if pass {
        vec!["git", "rev-parse", "HEAD"]
    } else {
        vec!["git", "rev-parse", "--verify", "nonexistent-ref"]
    };
    let config = root.join("lupin.json");
    fs::write(&config,json!({"repo":repo,"goal":"Improve value incrementally","ollama_url":url,"model":"fake",
        "context_tokens":32768,"output_tokens":4096,"implementation_calls":16,
        "checks":[{"argv":argv,"timeout_seconds":5}],"state_dir":root.join("state"),"retry_seconds":1}).to_string()).unwrap();
    config
}
fn command(root: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(root)
        .env("XDG_CONFIG_HOME", root.join(".lupin/global"));
    c
}
fn pipeline(pass: bool) {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path(), &server.url, pass);
    let head = git(&root.path().join("repo"), &["rev-parse", "HEAD"]);
    let o = command(root.path())
        .args(["run", "--config", config.to_str().unwrap(), "--cycles", "2"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 10);
    for n in [0, 1, 4, 5, 6, 9] {
        assert_eq!(requests[n]["messages"].as_array().unwrap().len(), 2);
    }
    assert!(
        !serde_json::to_string(&requests[5..9])
            .unwrap()
            .contains("PRIVATE_IMPLEMENTATION_TRANSCRIPT")
    );
    assert!(
        !serde_json::to_string(&*requests)
            .unwrap()
            .contains("USER_EDIT")
    );
    assert_eq!(
        fs::read_to_string(root.path().join("repo/value.txt")).unwrap(),
        "USER_EDIT"
    );
    assert_eq!(git(&root.path().join("repo"), &["rev-parse", "HEAD"]), head);
    if pass {
        assert_ne!(state["accepted_ref"], head);
        assert_eq!(
            fs::read_to_string(
                Path::new(state["accepted_workspace"].as_str().unwrap()).join("value.txt")
            )
            .unwrap(),
            "2"
        );
        assert!(requests[5].to_string().contains("Increment 1"));
    } else {
        assert_eq!(state["accepted_ref"], head);
    }
    drop(requests);
    let mut changed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    changed["goal"] = json!("Changed goal");
    fs::write(&config, changed.to_string()).unwrap();
    assert!(
        !command(root.path())
            .args(["run", "--config", config.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn fresh_cycles_and_checkout_isolation() {
    pipeline(true);
}
#[test]
fn failed_checks_override_review() {
    pipeline(false);
}
#[test]
fn setup_revises_goal_and_reuses_global_settings() {
    let server = Server::new(true, false);
    let root = tempfile::tempdir().unwrap();
    let input = format!(
        "{}
fake
32768
4096
16
Build an editor
Include tables
accept
git rev-parse HEAD
120
",
        server.url
    );
    let mut child = command(root.path())
        .arg("setup")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}
{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let config: Value =
        serde_json::from_slice(&fs::read(root.path().join("lupin.json")).unwrap()).unwrap();
    assert!(config["goal"].as_str().unwrap().contains("Draft 2"));
    assert!(config.get("ollama_url").is_none());
    let settings = root.path().join(".lupin/global/lupin/settings.json");
    assert!(settings.exists());
    let calls = server.requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].to_string().contains("Include tables"));
    drop(calls);
    let second_dir = tempfile::tempdir().unwrap();
    let second = second_dir.path().to_path_buf();
    let mut c = command(&second);
    c.env("XDG_CONFIG_HOME", root.path().join(".lupin/global"));
    let mut child = c
        .arg("setup")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"Another editor
accept
git rev-parse HEAD
120
",
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Ollama server ["));
}
#[cfg(unix)]
#[test]
fn soft_stop_finishes_cycle_and_force_stop_releases_lock() {
    let server = Server::new(false, true);
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    let mut child = command(root.path())
        .args(["run", "--forever"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    while server.requests.lock().unwrap().is_empty() {
        thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(server.requests.lock().unwrap().len(), 5);
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["cycle"], 1);
    let mut child = command(root.path())
        .args(["run", "--forever"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    while server.requests.lock().unwrap().len() < 6 {
        thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    thread::sleep(Duration::from_millis(40));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    assert_eq!(child.wait().unwrap().code(), Some(130));
    // A new process must acquire the kernel lock without manual cleanup.
    let out = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn setup_offers_first_commit_and_respects_ignored_files() {
    for accept in [false, true] {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".lupin/global/lupin")).unwrap();
        fs::write(
            root.path().join(".lupin/global/lupin/settings.json"),
            json!({"model":"fake","ollama_url":"http://127.0.0.1:1"}).to_string(),
        )
        .unwrap();
        fs::write(
            root.path().join(".lupin/goal-draft.json"),
            json!({"pitch":"Editor","goal":"Build an editor","feedback":""}).to_string(),
        )
        .unwrap();
        fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.path().join("ignored.txt"), "ignored data").unwrap();
        git(root.path(), &["init"]);
        // Even previously staged runtime files must stay out of the source baseline.
        git(root.path(), &["add", ".lupin/goal-draft.json"]);
        let mut child = command(root.path())
            .arg("setup")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = format!(
            "accept\ngit rev-parse HEAD\n120\n{}\n",
            if accept { "yes" } else { "no" }
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.status.success(),
            accept,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if accept {
            let files = git(root.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
            assert_eq!(files, ".gitignore\nmain.rs");
            assert!(root.path().join("lupin.json").exists());
        } else {
            assert!(
                !Command::new("git")
                    .arg("-C")
                    .arg(root.path())
                    .args(["rev-parse", "--verify", "HEAD"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert!(!root.path().join("lupin.json").exists());
            assert!(root.path().join(".lupin/goal-draft.json").exists());
        }
        assert_eq!(
            fs::read_to_string(root.path().join("main.rs")).unwrap(),
            "fn main() {}\n"
        );
    }
}

#[test]
fn cargo_generated_lockfile_does_not_reject_a_valid_task() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"scope_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.join(".gitignore"), "/target\n").unwrap();
    git(&repo, &["add", "Cargo.toml", "src/main.rs", ".gitignore"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Rust fixture",
        ],
    );
    let mut cfg: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    cfg["checks"] = json!([{"argv":[env!("CARGO"),"test"],"timeout_seconds":30}]);
    fs::write(&config, cfg.to_string()).unwrap();
    let out = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let outcome: Value = serde_json::from_slice(
        &fs::read(root.path().join("state/cycle-000001/outcome.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(outcome["disposition"], "accepted", "{outcome}");
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert!(
        Path::new(state["accepted_workspace"].as_str().unwrap())
            .join("Cargo.lock")
            .exists()
    );
}

#[cfg(unix)]
mod terminal_ui {
    use super::*;
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    };
    struct TerminalProcess {
        master: fs::File,
        slave: fs::File,
        child: std::process::Child,
        parser: vt100::Parser,
        original: libc::tcflag_t,
    }
    impl TerminalProcess {
        fn start(root: &Path) -> Self {
            let (mut master, mut slave) = (0, 0);
            let size = libc::winsize {
                ws_row: 36,
                ws_col: 110,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        &size,
                    )
                },
                0
            );
            let master = unsafe { fs::File::from_raw_fd(master) };
            let slave = unsafe { fs::File::from_raw_fd(slave) };
            let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
            assert_eq!(
                unsafe { libc::tcgetattr(slave.as_raw_fd(), term.as_mut_ptr()) },
                0
            );
            let original = unsafe { term.assume_init() }.c_lflag;
            let mut cmd = command(root);
            cmd.env("TERM", "xterm-256color")
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(slave.try_clone().unwrap());
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = cmd.spawn().unwrap();
            unsafe {
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
            }
            Self {
                master,
                slave,
                child,
                parser: vt100::Parser::new(36, 110, 0),
                original,
            }
        }
        fn read(&mut self) {
            let mut bytes = [0; 65536];
            while let Ok(n) = self.master.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                self.parser.process(&bytes[..n]);
            }
        }
        fn wait(&mut self, text: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                self.read();
                if self.parser.screen().contents().contains(text) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Missing {text:?}:\n{}",
                    self.parser.screen().contents()
                );
                thread::sleep(Duration::from_millis(30));
            }
        }
        fn send(&mut self, keys: &[u8]) {
            self.master.write_all(keys).unwrap();
        }
        fn restored(&mut self) {
            self.exited(0);
        }
        fn resize(&mut self, rows: u16, cols: u16) {
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                0
            );
            self.parser.screen_mut().set_size(rows, cols);
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGWINCH);
            }
            thread::sleep(Duration::from_millis(250));
            self.read();
        }
        fn exited(&mut self, code: i32) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                self.read();
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(code));
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(20));
            }
            let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
            unsafe {
                libc::tcgetattr(self.slave.as_raw_fd(), term.as_mut_ptr());
            }
            assert_eq!(
                unsafe { term.assume_init() }.c_lflag & (libc::ECHO | libc::ICANON),
                self.original & (libc::ECHO | libc::ICANON)
            );
        }
    }
    impl Drop for TerminalProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    #[test]
    fn full_screen_run_streams_soft_stops_and_restores_the_terminal() {
        let server = Server::new(false, true);
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, true);
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Resume project");
        ui.send(b"\r");
        ui.wait("LIVE");
        ui.wait("cycle 1");
        ui.send(b"?");
        ui.wait("Keyboard guide");
        ui.send(b"?");
        ui.wait("following live");
        ui.send(b"\x1b[5~");
        ui.wait("scrollback");
        ui.send(b"f");
        ui.wait("following live");
        ui.send(b"\x03");
        ui.wait("Finishing this cycle");
        ui.wait("Run saved");
        ui.resize(26, 80);
        assert!(!ui.parser.screen().contents().contains("Run health"));
        ui.wait("Run saved");
        let state: Value =
            serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap())
                .unwrap();
        assert_eq!(state["cycle"], 1);
        assert_eq!(state["recent"][0]["disposition"], "accepted");
        ui.send(b"\r");
        ui.wait("Resume project");
        ui.send(b"q");
        ui.restored();
    }
    #[test]
    fn full_screen_settings_and_unicode_input_do_not_modify_project() {
        let root = tempfile::tempdir().unwrap();
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Set up this project");
        ui.send(b"jjjj\r");
        ui.wait("Shared settings");
        ui.send(b"\r");
        ui.wait("Ctrl+U clear");
        ui.send("\x15http://localhost:11434".as_bytes());
        ui.send(b"\r");
        ui.wait("Shared settings");
        ui.send(b"\x1b");
        ui.wait("Set up this project");
        ui.send(b"q");
        ui.restored();
        let saved: Value = serde_json::from_slice(
            &fs::read(root.path().join(".lupin/global/lupin/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["ollama_url"], "http://localhost:11434");
        assert!(!root.path().join("lupin.json").exists());
    }
    #[test]
    fn brave_key_entry_is_masked_and_stored_separately() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Set up this project");
        ui.send(b"jjjj\r");
        ui.wait("Shared settings");
        ui.send(b"jjjjjjj\r");
        ui.wait("Enter or replace key");
        ui.send(b"\r");
        ui.wait("input is masked");
        ui.send(b"fake-brave-secret");
        ui.wait("••••");
        assert!(!ui.parser.screen().contents().contains("fake-brave-secret"));
        ui.send(b"\r");
        ui.wait("Configured");
        ui.send(b"\x1b");
        ui.wait("Set up this project");
        ui.send(b"q");
        ui.restored();
        let global = root.path().join(".lupin/global/lupin");
        assert_eq!(
            fs::read_to_string(global.join("brave.key")).unwrap(),
            "fake-brave-secret"
        );
        assert_eq!(
            fs::metadata(global.join("brave.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let text = fs::read_to_string(global.join("settings.json")).unwrap();
        assert!(!text.contains("fake-brave-secret"));
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["web_enabled"],
            true
        );
    }
    #[test]
    fn force_stop_restores_terminal_and_releases_the_run_lock() {
        let server = Server::new(false, true);
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, true);
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Resume project");
        ui.send(b"\r");
        ui.wait("LIVE");
        ui.send(b"\x03");
        ui.wait("Finishing this cycle");
        ui.send(b"\x03");
        ui.exited(130);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(root.path().join("state/run.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }
    #[test]
    fn goal_setup_uses_full_screen_drafting_and_persists_the_result() {
        let server = Server::new(true, true);
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join(".lupin/global/lupin/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            settings,
            json!({"ollama_url":server.url,"model":"fake"}).to_string(),
        )
        .unwrap();
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Set up this project");
        ui.send(b"\r");
        ui.wait("Describe what you want to build");
        ui.send("An éditeur in Rust".as_bytes());
        ui.send(b"\r");
        ui.wait("Review your goal");
        ui.wait("Build a useful editor. Draft 1.");
        ui.send(b"\r");
        ui.wait("Validation command");
        ui.send(b"\r");
        ui.wait("Check timeout (seconds)");
        ui.send(b"\r");
        ui.wait("Resume project");
        ui.send(b"q");
        ui.restored();
        let config: Value =
            serde_json::from_slice(&fs::read(root.path().join("lupin.json")).unwrap()).unwrap();
        assert_eq!(config["goal"], "Build a useful editor. Draft 1.");
    }
}

fn probe_reply(body: &Value, mode: u8) -> (String, Value) {
    let system = body["messages"][0]["content"].as_str().unwrap();
    let value = if system.starts_with("Start fresh") {
        json!({"gap":"Return two","why_now":"Required behavior","files":["src/lib.rs"]})
    } else if system.starts_with("Return JSON {title") {
        json!({"title":"Return two","objective":"value returns two","acceptance":["value returns two"],"files":["src/lib.rs"],"out_of_scope":[]})
    } else if system.starts_with("Implement the supplied") {
        if mode >= 3 {
            return dev_tool_reply(body, mode);
        }
        if body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
        {
            return ("Implemented and tested.".into(), json!([]));
        }
        return (
            String::new(),
            json!([{"function":{"name":"write_file","arguments":{"path":"src/lib.rs","content":"pub fn value() -> u8 { 2 }\n#[test] fn baseline() { assert!(value() > 0); }\n#[test] fn returns_two() { assert_eq!(value(), 2); }\n"}}}]),
        );
    } else if system.starts_with("Write an independent Rust") {
        json!({"code":if mode == 3 {"#[test] fn probe() { assert_eq!(probe_demo::value(), 2); }"} else if mode == 1 {"#[test] fn probe() { probe_demo::missing(); }"} else if mode == 2 { if body["messages"][1]["content"].as_str().unwrap().contains("compiler_feedback") {"#[test] fn probe() { assert_eq!(probe_demo::value(), 2); }"} else {"#[test] fn probe() { assert_eq!(value(), 2); }"} } else {"#[test] fn probe() { assert_eq!(probe_demo::value(), 3); }"},"rationale":"Fixture probe deliberately challenges the approving reviewer"})
    } else if system.starts_with("Independently review") {
        json!({"decision":"accept","reason":"Fixture reviewer always approves","criteria":[{"criterion":"C1","passed":true,"evidence":"implementation"},{"criterion":"C2","passed":true,"evidence":"focused test"}]})
    } else {
        panic!("Unexpected stage: {system}");
    };
    (value.to_string(), json!([]))
}
fn regression_probe(mode: u8) {
    let server = Server::with_probe(false, false, Some(mode));
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"probe_demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n#[test] fn baseline() { assert!(value() > 0); }\n",
    )
    .unwrap();
    fs::write(repo.join(".gitignore"), "/target\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Rust fixture",
        ],
    );
    let mut c: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    c["checks"] = json!([{"argv":["cargo","test","--offline"],"timeout_seconds":30}]);
    fs::write(&config, c.to_string()).unwrap();
    let output = command(root.path())
        .args(["run", "--config", config.to_str().unwrap(), "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let art = root.path().join("state/cycle-000001");
    let outcome: Value =
        serde_json::from_slice(&fs::read(art.join("outcome.json")).unwrap()).unwrap();
    if mode == 4 {
        assert_eq!(outcome["disposition"], "rejected/Accept");
        let gate: Value =
            serde_json::from_slice(&fs::read(art.join("acceptance.json")).unwrap()).unwrap();
        assert_eq!(gate["checks_pass"], false);
        let command: Value =
            serde_json::from_slice(&fs::read(art.join("tool-0-2.json")).unwrap()).unwrap();
        let result: Value = serde_json::from_str(command["result"].as_str().unwrap()).unwrap();
        assert_eq!(result["exit_code"], 0);
        return;
    }
    let probe: Value = serde_json::from_slice(
        &fs::read(art.join("probe-outcome.json")).unwrap_or_else(|_| {
            panic!(
                "Missing probe: {outcome}; {}",
                String::from_utf8_lossy(&output.stdout)
            )
        }),
    )
    .unwrap();
    assert_eq!(
        probe["status"],
        match mode {
            0 => "failed_and_retained",
            1 => "invalid_probe_removed",
            _ => "passed_and_retained",
        }
    );
    assert_eq!(
        art.join("workspace/tests/lupin_regression_1.rs").exists(),
        mode != 1
    );
    assert_eq!(outcome["disposition"] == "accepted", mode != 0, "{outcome}");
    if mode == 3 {
        for name in [
            "command-0-2.log",
            "diagnostics-0-1.log",
            "tool-1-0.json",
            "verification.json",
        ] {
            assert!(art.join(name).exists(), "Missing {name}");
        }
        let log: Value =
            serde_json::from_slice(&fs::read(art.join("tool-1-0.json")).unwrap()).unwrap();
        assert_eq!(log["ok"], true);
        assert!(log["result"].as_str().unwrap().contains("COMMAND_FINISHED"));
        assert!(
            fs::read_to_string(repo.join("src/lib.rs"))
                .unwrap()
                .contains("{ 1 }")
        );
    }
    let requests = server.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .filter(|r| r["tools"].is_null())
            .all(|r| r["format"].is_object())
    );
}
#[test]
fn failing_regression_overrides_approving_reviewer() {
    regression_probe(0);
}
#[test]
fn noncompiling_probe_is_removed_without_losing_valid_work() {
    regression_probe(1);
}

#[test]
fn regression_compile_error_gets_a_fresh_repair() {
    regression_probe(2);
}

fn dev_tool_reply(body: &Value, mode: u8) -> (String, Value) {
    let messages = body["messages"].as_array().unwrap();
    if messages
        .iter()
        .any(|m| m["tool_name"] == "read_command_log")
    {
        return ("Done".into(), json!([]));
    }
    if messages.iter().any(|m| m["tool_name"] == "run_command") {
        return (
            String::new(),
            json!([{"function":{"name":"read_command_log","arguments":{"log_id":"command-0-2.log"}}}]),
        );
    }
    let script = "cat > src/lib.rs <<'RS'\npub fn value() -> u8 { 2 }\n#[test] fn baseline() { assert!(value() > 0); }\n#[test] fn returns_two() { assert_eq!(value(), 2); }\nRS\necho COMMAND_FINISHED";
    let script = if mode == 4 {
        script.replace("assert_eq!(value(), 2)", "assert_eq!(value(), 3)")
    } else {
        script.into()
    };
    (
        String::new(),
        json!([
            {"function":{"name":"lookup_symbol","arguments":{"symbol":"value"}}},
            {"function":{"name":"compiler_diagnostics","arguments":{}}},
            {"function":{"name":"run_command","arguments":{"argv":["sh","-c",script]}}}
        ]),
    )
}
#[test]
fn execution_diagnostics_and_lookup_work_in_the_real_pipeline() {
    regression_probe(3);
}

#[test]
fn successful_command_cannot_override_failing_final_checks() {
    regression_probe(4);
}
