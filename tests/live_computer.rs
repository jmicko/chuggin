//! Opt-in inference smoke test. Prepare a NEW disposable private computer first;
//! this test never installs Podman, builds images, or uses a real user project.
//!
//! CHUGGIN_LIVE_COMPUTER_ROOT=/tmp/marked-fixture \
//! CHUGGIN_LIVE_OLLAMA_URL=http://your-authorized-ollama:11434 \
//! cargo test --test live_computer -- --ignored --nocapture
//!
//! The root must contain `.live-fixture` with exactly
//! `disposable private computer\n`, `chuggin.json`, and that config's prepared
//! `state_dir/computer/ready.json`. No prior conversation is accepted. Logs and
//! model artifacts remain there for inspection. Only native `beam:cloud` runs.
#![cfg(unix)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const BIN: &str = env!("CARGO_BIN_EXE_chuggin");

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn inside(root: &Path, value: &Value) -> PathBuf {
    let path = Path::new(
        value
            .as_str()
            .expect("Fixture config must name repo/state_dir"),
    );
    let path = fs::canonicalize(if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    })
    .unwrap();
    assert!(
        path.starts_with(root),
        "Fixture path escapes disposable root"
    );
    path
}

fn rpc(socket: &Path, request: Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(if request["action"] == "call" {
            65
        } else {
            2
        })))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{request}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(2_000_000)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if reply["ok"] == true {
        Ok(reply["result"].clone())
    } else {
        Err(reply["error"].to_string())
    }
}

struct Live {
    root: PathBuf,
    engine: Child,
    run: Option<Child>,
    socket: PathBuf,
    session: String,
    serial: u64,
}
impl Live {
    fn start(root: &Path, config: &Path) -> Self {
        let global = root.join(".chuggin/global");
        let runtime = global.join("chuggin/runtime");
        assert!(
            !runtime.exists()
                || fs::read_dir(&runtime)
                    .unwrap()
                    .flatten()
                    .all(|e| e.path().extension().is_none_or(|s| s != "sock")),
            "Use a disposable fixture with no active controller"
        );
        let log = fs::File::create(root.join("live-smoke-engine.log")).unwrap();
        let mut engine = Command::new(BIN)
            .args(["engine", "--config"])
            .arg(config)
            .current_dir(root)
            .env("XDG_CONFIG_HOME", &global)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        let socket = loop {
            if let Ok(entries) = fs::read_dir(&runtime)
                && let Some(socket) = entries.flatten().map(|e| e.path()).find(|path| {
                    path.extension().is_some_and(|s| s == "sock")
                        && rpc(path, json!({"action":"status"})).is_ok_and(|status| {
                            status["config"] == config.to_string_lossy().as_ref()
                        })
                })
            {
                break socket;
            }
            if engine.try_wait().unwrap().is_some() || Instant::now() >= until {
                let _ = engine.kill();
                let _ = engine.wait();
                panic!(
                    "Live controller startup failed: {}",
                    fs::read_to_string(root.join("live-smoke-engine.log")).unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(30));
        };
        let mut live = Self {
            root: root.into(),
            engine,
            run: None,
            socket,
            session: String::new(),
            serial: 0,
        };
        live.session = rpc(&live.socket, json!({"action":"open_session"})).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        live
    }
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.serial += 1;
        let result = rpc(&self.socket, json!({"action":"call","session_id":self.session,"operation_id":format!("live-smoke-{}",self.serial),"name":name,"arguments":arguments})).unwrap();
        assert_eq!(result["status"], "complete", "{name}: {result}");
        result["result"].clone()
    }
    fn run_cycle(&mut self, config: &Path) -> ExitStatus {
        let log = fs::File::create(self.root.join("live-smoke-run.log")).unwrap();
        self.run = Some(
            Command::new(BIN)
                .args(["run", "--config"])
                .arg(config)
                .args(["--cycles", "1"])
                .current_dir(&self.root)
                .env("XDG_CONFIG_HOME", self.root.join(".chuggin/global"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(status) = self.run.as_mut().unwrap().try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                // Exercise normal Ctrl-C first, then its explicit forced-stop
                // path. Drop still closes the known disposable controller.
                for grace in [Duration::from_secs(5), Duration::from_secs(3)] {
                    unsafe {
                        libc::kill(self.run.as_ref().unwrap().id() as i32, libc::SIGINT);
                    }
                    let until = Instant::now() + grace;
                    while Instant::now() < until {
                        if self.run.as_mut().unwrap().try_wait().unwrap().is_some() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    if self.run.as_mut().unwrap().try_wait().unwrap().is_some() {
                        break;
                    }
                }
                panic!(
                    "Beam smoke test exceeded 180s; inspect {}",
                    self.root.join("live-smoke-run.log").display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        let _ = rpc(&self.socket, json!({"action":"force_stop"}));
        if let Some(run) = self.run.as_mut()
            && run.try_wait().ok().flatten().is_none()
        {
            let _ = run.kill();
            let _ = run.wait();
        }
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if self.engine.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        let _ = self.engine.kill();
        let _ = self.engine.wait();
    }
}

fn host_files(repo: &Path, state: &Path, root: &Path) -> BTreeMap<PathBuf, String> {
    let mut pending = vec![repo.to_owned()];
    let mut hashes = BTreeMap::new();
    let mut total = 0u64;
    while let Some(path) = pending.pop() {
        if path.starts_with(state)
            || path.starts_with(root.join(".chuggin/global"))
            || path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("live-smoke-"))
        {
            continue;
        }
        let m = fs::symlink_metadata(&path).unwrap();
        if m.is_dir() {
            pending.extend(fs::read_dir(&path).unwrap().flatten().map(|e| e.path()));
            continue;
        }
        total += m.len();
        assert!(
            total <= 16 * 1024 * 1024 && hashes.len() < 1000,
            "Use a small disposable fixture"
        );
        let bytes = if m.file_type().is_symlink() {
            fs::read_link(&path)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            fs::read(&path).unwrap()
        };
        hashes.insert(path, format!("{:x}", Sha256::digest(bytes)));
    }
    hashes
}

fn successful_calls(state: &Path) -> (usize, BTreeSet<String>) {
    let mut requests = 0;
    let mut successful = BTreeSet::new();
    for cycle in fs::read_dir(state)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
    {
        for entry in fs::read_dir(cycle.path()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name
                .strip_prefix("request-")
                .and_then(|s| s.strip_suffix(".json"))
                .is_some_and(|s| s.parse::<u64>().is_ok())
            {
                requests += 1;
            }
            let Some(step) = name
                .strip_prefix("implementation-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            let response = json_file(&entry.path());
            for (index, call) in response["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let result = cycle.path().join(format!("tool-{step}-{index}.json"));
                if result.is_file() && json_file(&result)["ok"] == true {
                    successful.insert(call["function"]["name"].as_str().unwrap_or("").to_owned());
                }
            }
        }
    }
    (requests, successful)
}

#[test]
#[ignore = "Live Beam inference; requires explicit URL and a marked, preprepared disposable private computer"]
fn beam_reads_mail_and_uses_general_computer_tools() {
    let root = fs::canonicalize(
        std::env::var_os("CHUGGIN_LIVE_COMPUTER_ROOT")
            .expect("Set CHUGGIN_LIVE_COMPUTER_ROOT to a NEW prepared disposable fixture"),
    )
    .unwrap();
    assert!(
        root.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()),
        "Only a marked temporary fixture is allowed"
    );
    assert_eq!(
        fs::read_to_string(root.join(".live-fixture")).unwrap(),
        "disposable private computer\n"
    );
    let endpoint = std::env::var("CHUGGIN_LIVE_OLLAMA_URL")
        .expect("Set the authorized CHUGGIN_LIVE_OLLAMA_URL; no provider is selected implicitly");
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none(),
        "Use an HTTP(S) Ollama endpoint without URL credentials"
    );
    let config_path = fs::canonicalize(root.join("chuggin.json")).unwrap();
    assert!(config_path.starts_with(&root));
    let mut config = json_file(&config_path);
    let repo = inside(&root, &config["repo"]);
    let state = inside(&root, &config["state_dir"]);
    assert_eq!(config["execution"]["kind"], "podman");
    assert!(matches!(
        config["activity_mode"].as_str(),
        Some("type2" | "chaos")
    ));
    assert!(
        !state.join("conversation.json").exists() && !state.join("state.json").exists(),
        "Use a NEW fixture, never a user's existing conversation"
    );
    let ready = json_file(&state.join("computer/ready.json"));
    let expected = format!(
        "chuggin-{}",
        &format!("{:x}", Sha256::digest(repo.as_os_str().as_encoded_bytes()))[..24]
    );
    assert_eq!(
        ready["name"], expected,
        "Prepared computer must belong to this disposable repository"
    );
    config["model"] = json!("beam:cloud");
    config["ollama_url"] = json!(endpoint);
    config["context_tokens"] = json!(32768);
    config["output_tokens"] = json!(2048);
    config["implementation_calls"] = json!(8);
    config["request_timeout_seconds"] = json!(60);
    config["retry_seconds"] = json!(1);
    config["run_duration_seconds"] = json!(0);
    config["helpers"] = json!({"enabled":false});
    config["active_hours"] = json!({"mode":"always"});
    config["reasoning"] = json!({"mode":"default"});
    config["checks"] = json!([]);
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let before = host_files(&repo, &state, &root);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let browser_marker = format!("browser-{nonce}");
    let document_marker = format!("document-{nonce}");
    let receipt = format!("mail-{nonce}");
    let mut live = Live::start(&root, &config_path);
    assert_eq!(live.tool("begin_edit", json!({}))["granted"], true);
    live.tool("write_file",json!({"path":"/root/chuggin-live.html","content":format!("<!doctype html><title>Private smoke fixture</title><h1>Browser fixture</h1><p>{browser_marker}</p>")}));
    live.tool("write_file",json!({"path":"/root/chuggin-live.txt","content":format!("Private document marker: {document_marker}\nUnicode note: 🌱\n")}));
    live.tool(
        "end_edit",
        json!({"summary":"Prepared disposable computer smoke-test fixtures."}),
    );
    live.tool("message_loop",json!({"body":format!("Please do this one-off tools check in the private computer. Inspect installed applications with computer_info. Use browser to read file:///root/chuggin-live.html and inspect_document to read /root/chuggin-live.txt. Save a short notebook note tagged live-smoke. Then send_message with a brief result quoting the browser marker, document marker, and this mailbox receipt exactly: {receipt}. This is a disposable test: do not install packages, edit system files, launch commands, or contact websites. This request is not a permanent goal. You can record this small task complete after replying.")}));
    let status = live.run_cycle(&config_path);
    assert_eq!(
        host_files(&repo, &state, &root),
        before,
        "The model's private computer must not change host project files"
    );
    let (requests, successful) = successful_calls(&state);
    let mail = fs::read_dir(state.join("messenger/messages"))
        .unwrap()
        .flatten()
        .map(|e| json_file(&e.path()))
        .collect::<Vec<_>>();
    let delivered = mail.iter().any(|m| {
        m["sender"] == "model"
            && m["body"].as_str().is_some_and(|s| {
                s.contains(&receipt) && s.contains(&browser_marker) && s.contains(&document_marker)
            })
    });
    let report = json!({"model":"beam:cloud","exit_success":status.success(),"model_requests":requests,"successful_tools":successful,"grounded_mail_reply":delivered,"host_project_unchanged":true});
    fs::write(
        root.join("live-smoke-report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    eprintln!("{}\nArtifacts: {}", report, root.display());
    assert!(
        status.success(),
        "Live run failed; inspect live-smoke-run.log and saved request/response artifacts"
    );
    for required in [
        "read_messages",
        "computer_info",
        "browser",
        "inspect_document",
        "notebook",
        "send_message",
    ] {
        assert!(
            successful.contains(required),
            "Missing successful {required}: {report}"
        );
    }
    assert!(
        delivered,
        "Reply must quote observations obtained from the mailbox and private files: {report}"
    );
    assert!(requests > 0, "No real native Ollama request was recorded");
}
