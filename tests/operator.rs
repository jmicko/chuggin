#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const BIN: &str = env!("CARGO_BIN_EXE_chuggin");
struct Fixture {
    dir: tempfile::TempDir,
    child: Child,
    socket: PathBuf,
    serial: AtomicU64,
}
impl Fixture {
    fn new(url: &str) -> Self {
        Self::with_context(url, 8192)
    }
    fn with_context(url: &str, context_tokens: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        fs::write(
            dir.path().join(".git/info/exclude"),
            "config/\nengine.log\n.chuggin/\nchuggin.json\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "baseline"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let config = json!({"repo":dir.path(),"state_dir":dir.path().join(".chuggin"),"goal":"Improve the fixture","ollama_url":url,"model":"test-model","context_tokens":context_tokens,"output_tokens":512,"implementation_calls":6,"checks":[{"argv":["git","rev-parse","HEAD"],"timeout_seconds":10}],"retry_seconds":1});
        fs::write(
            dir.path().join("chuggin.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let log = fs::File::create(dir.path().join("engine.log")).unwrap();
        let child = Command::new(BIN)
            .arg("engine")
            .arg("--config")
            .arg(dir.path().join("chuggin.json"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let runtime = dir.path().join("config/chuggin/runtime");
        let until = Instant::now() + Duration::from_secs(8);
        let socket = loop {
            if let Ok(files) = fs::read_dir(&runtime)
                && let Some(path) = files
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .find(|p| p.extension().is_some_and(|e| e == "sock"))
            {
                break path;
            }
            assert!(
                Instant::now() < until,
                "{}",
                fs::read_to_string(dir.path().join("engine.log")).unwrap()
            );
            thread::sleep(Duration::from_millis(25));
        };
        Self {
            dir,
            child,
            socket,
            serial: AtomicU64::new(0),
        }
    }
    fn rpc(&self, v: Value) -> Value {
        let mut s = UnixStream::connect(&self.socket).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        writeln!(s, "{v}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["ok"], true, "{reply}");
        reply["result"].clone()
    }
    fn session(&self) -> String {
        self.rpc(json!({"action":"open_session"}))["session_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn call(&self, session: &str, name: &str, args: Value) -> Value {
        self.call_id(
            session,
            name,
            args,
            &format!("op{}", self.serial.fetch_add(1, Ordering::SeqCst)),
        )
    }
    fn call_id(&self, session: &str, name: &str, args: Value, id: &str) -> Value {
        self.rpc(json!({"action":"call","session_id":session,"name":name,"arguments":args,"operation_id":id}))
    }
    fn wait(&self, mut predicate: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(8);
        while !predicate() {
            assert!(
                Instant::now() < end,
                "timed out: {} history: {}",
                self.rpc(json!({"action":"status"})),
                fs::read_to_string(self.dir.path().join(".chuggin/conversation.json"))
                    .unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(30));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut s) = UnixStream::connect(&self.socket) {
            let _ = writeln!(s, "{}", json!({"action":"force_stop"}));
        }
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Model {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    done: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Model {
    fn new(mut reply: impl FnMut(&Value, usize) -> Value + Send + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        break;
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut b = vec![0; length];
                if reader.read_exact(&mut b).is_err() {
                    continue;
                }
                let body: Value = serde_json::from_slice(&b).unwrap();
                let n = req.lock().unwrap().len();
                req.lock().unwrap().push(body.clone());
                let message = reply(&body, n);
                let response=json!({"model":"test-model","message":message,"done":true,"prompt_eval_count":100,"eval_count":10,"eval_duration":100000000}).to_string()+"\n";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
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
impl Drop for Model {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn task_and_tool(name: &str, args: Value) -> Value {
    let mut reply = tool(name, args);
    reply["tool_calls"].as_array_mut().unwrap().insert(0,json!({"id":"select-task","function":{"name":"set_task","arguments":{"title":"Fixture work","objective":"Apply requested changes","acceptance":["Requested work saved"],"files":["new.txt","file.txt"]}}}));
    reply
}
fn answer() -> Value {
    json!({"role":"assistant","content":"Done for now."})
}
fn tool(name: &str, args: Value) -> Value {
    json!({"role":"assistant","content":"","tool_calls":[{"id":"tool","function":{"name":name,"arguments":args}}]})
}
#[test]
fn controller_is_idle_and_sessions_serialize_versioned_edits_and_deduplicate() {
    let f = Fixture::new("http://127.0.0.1:1");
    assert_eq!(f.rpc(json!({"action":"status"}))["running"], false);
    let a = f.session();
    let b = f.session();
    assert_eq!(
        f.call(&a, "begin_edit", json!({}))["result"]["granted"],
        true
    );
    assert_eq!(f.call(&b, "begin_edit", json!({}))["status"], "failed");
    let read = f.call(&a, "read_file", json!({"path":"file.txt"}));
    assert!(read["result"]["version"].is_string(), "{read}");
    let args = json!({"path":"file.txt","old_text":"before","new_text":"after","expected_version":read["result"]["version"]});
    let first = f.call_id(&a, "edit_file", args.clone(), "same");
    assert_eq!(first["status"], "complete", "{first}");
    assert_eq!(
        f.call_id(&a, "edit_file", args, "same")["status"],
        "complete"
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("file.txt")).unwrap(),
        "after\n"
    );
    let stale=f.call(&a,"edit_file",json!({"path":"file.txt","old_text":"after","new_text":"bad","expected_version":read["result"]["version"]}));
    assert_eq!(stale["status"], "failed");
    assert_eq!(
        f.call(&a, "end_edit", json!({"summary":"Updated fixture"}))["status"],
        "complete"
    );
    assert_eq!(
        f.call(&b, "begin_edit", json!({}))["result"]["granted"],
        true
    );
    assert!(f.dir.path().join(".chuggin/operator-handoff.json").exists());
}
#[test]
fn commands_finish_while_paused_and_control_changes_do_not_need_editing() {
    let f = Fixture::new("http://127.0.0.1:1");
    let a = f.session();
    let b = f.session();
    let job = f.call_id(
        &a,
        "run_command",
        json!({"argv":["sh","-c","sleep 0.2; echo once >> count"]}),
        "launch",
    );
    assert_eq!(job["status"], "complete", "{job}");
    assert_eq!(
        f.call(&b, "set_nudge", json!({"request":"Keep this useful"}))["status"],
        "complete"
    );
    assert_eq!(
        f.call(&a, "end_edit", json!({"summary":"pending"}))["status"],
        "failed"
    );
    f.wait(|| {
        f.call(
            &a,
            "command_status",
            json!({"command_id":job["result"]["command_id"]}),
        )["result"]["running"]
            == false
    });
    f.call_id(
        &a,
        "run_command",
        json!({"argv":["sh","-c","sleep 0.2; echo once >> count"]}),
        "launch",
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("count")).unwrap(),
        "once\n"
    );
    assert_eq!(
        f.call(&a, "end_edit", json!({"summary":"Command completed"}))["status"],
        "complete"
    );
}
#[test]
fn schedule_holds_only_loop_and_manual_resume_overrides_it() {
    let model = Model::new(|_, _| answer());
    let f = Fixture::new(&model.url);
    let session = f.session();
    let now = chrono::Utc::now();
    let start = (now + chrono::Duration::hours(1))
        .format("%H:%M")
        .to_string();
    let end = (now + chrono::Duration::hours(2))
        .format("%H:%M")
        .to_string();
    let settings = f.call(&session, "get_settings", json!({}));
    let set=f.call(&session,"update_settings",json!({"expected_revision":settings["result"]["revision"],"settings":{"active_hours":{"mode":"custom","window":{"start":start,"end":end,"timezone":"UTC","days":[0,1,2,3,4,5,6],"closing":"call"}}}}));
    assert_eq!(set["status"], "complete", "{set}");
    f.rpc(json!({"action":"start"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["paused"] == true);
    assert!(model.requests.lock().unwrap().is_empty());
    f.rpc(json!({"action":"chat_send","session_id":session,"message":"Say hello without changing the project"}));
    f.wait(|| f.rpc(json!({"action":"chat_status","session_id":session}))["busy"] == false);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    f.rpc(json!({"action":"resume","mode":"until_close"}));
    f.wait(|| model.requests.lock().unwrap().len() > 1);
    f.rpc(json!({"action":"stop"}));
}
#[test]
fn operator_changes_invalidate_a_loop_response_waiting_to_edit() {
    let model = Model::new(|_, n| {
        if n == 0 {
            thread::sleep(Duration::from_millis(500));
            tool(
                "write_file",
                json!({"path":"file.txt","content":"stale loop overwrite"}),
            )
        } else {
            answer()
        }
    });
    let f = Fixture::new(&model.url);
    let session = f.session();
    f.rpc(json!({"action":"start"}));
    f.wait(|| !model.requests.lock().unwrap().is_empty());
    let pending = f.call(&session, "begin_edit", json!({}));
    assert_eq!(pending["result"]["granted"], false, "{pending}");
    f.wait(|| f.call(&session, "begin_edit", json!({}))["result"]["granted"] == true);
    let read = f.call(&session, "read_file", json!({"path":"file.txt"}));
    let edit=f.call(&session,"edit_file",json!({"path":"file.txt","old_text":"before","new_text":"operator","expected_version":read["result"]["version"]}));
    assert_eq!(edit["status"], "complete", "{edit}");
    f.call(&session, "end_edit", json!({"summary":"Operator fixed it"}));
    f.wait(|| model.requests.lock().unwrap().len() > 1);
    f.rpc(json!({"action":"stop"}));
    assert_eq!(
        fs::read_to_string(f.dir.path().join("file.txt")).unwrap(),
        "operator\n"
    );
    assert!(
        model.requests.lock().unwrap()[1]
            .to_string()
            .contains("Not executed: an operator")
    );
}
#[test]
fn goal_update_is_revision_checked_and_reaches_the_loop() {
    let model = Model::new(|_, _| answer());
    let f = Fixture::new(&model.url);
    let session = f.session();
    let settings = f.call(&session, "get_settings", json!({}));
    assert_eq!(f.call(&session,"set_goal",json!({"goal":"Write useful research notes","expected_revision":settings["result"]["revision"]}))["status"],"complete");
    assert_eq!(
        f.call(
            &session,
            "set_goal",
            json!({"goal":"stale","expected_revision":settings["result"]["revision"]})
        )["status"],
        "failed"
    );
    f.rpc(json!({"action":"start","mode":"one_cycle"}));
    f.wait(|| !model.requests.lock().unwrap().is_empty());
    f.wait(|| f.rpc(json!({"action":"status"}))["running"] == false);
    assert!(
        model.requests.lock().unwrap()[0]
            .to_string()
            .contains("Write useful research notes")
    );
    let state: Value =
        serde_json::from_slice(&fs::read(f.dir.path().join(".chuggin/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["goal"], "Write useful research notes");
}
#[test]
fn chat_history_is_durable_and_does_not_start_the_loop() {
    let model = Model::new(|_, _| answer());
    let f = Fixture::new(&model.url);
    let session = f.session();
    f.rpc(json!({"action":"chat_send","session_id":session,"message":"Remember violet giraffe"}));
    f.wait(|| f.rpc(json!({"action":"chat_status","session_id":session}))["busy"] == false);
    f.rpc(json!({"action":"open_session","session_id":session}));
    f.rpc(json!({"action":"chat_send","session_id":session,"message":"What did I say?"}));
    f.wait(|| f.rpc(json!({"action":"chat_status","session_id":session}))["busy"] == false);
    assert_eq!(f.rpc(json!({"action":"status"}))["running"], false);
    assert!(
        model.requests.lock().unwrap()[1]
            .to_string()
            .contains("violet giraffe")
    );
    assert!(!f.dir.path().join(".chuggin/conversation.json").exists());
}
#[test]
fn mcp_stdio_exposes_same_tools_without_starting_inference() {
    let model = Model::new(|_, _| answer());
    let f = Fixture::new(&model.url);
    let mut child = Command::new(BIN)
        .args(["mcp", "--project"])
        .arg(f.dir.path())
        .env("XDG_CONFIG_HOME", f.dir.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if let Ok(line) = line {
                let _ = tx.send(line);
            } else {
                break;
            }
        }
    });
    let send = |input: &mut std::process::ChildStdin, v: Value| {
        writeln!(input, "{v}").unwrap();
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
    );
    let init: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(8)).unwrap()).unwrap();
    assert!(init.get("result").is_some(), "{init}");
    send(
        &mut input,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let tools: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(8)).unwrap()).unwrap();
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "set_goal")
    );
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"open_operator_session","arguments":{}}}),
    );
    let opened: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(8)).unwrap()).unwrap();
    let opened: Value =
        serde_json::from_str(opened["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let session = opened["session_id"].clone();
    for id in [4, 5] {
        send(
            &mut input,
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"set_nudge","arguments":{"session_id":session,"operation_id":"same-nudge","request":"Inspect research notes"}}}),
        );
        let reply: Value =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(8)).unwrap()).unwrap();
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        let result: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(result["status"], "complete", "{result}");
    }
    assert_eq!(f.rpc(json!({"action":"status"}))["nudges"]["revision"], 1);
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"chuggin://project/status"}}),
    );
    let resource: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(8)).unwrap()).unwrap();
    assert!(
        resource["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Inspect research notes")
    );
    assert!(model.requests.lock().unwrap().is_empty());
    drop(input);
    let _ = child.kill();
    let _ = child.wait();
}

fn closed_schedule() -> Value {
    let now = chrono::Utc::now();
    json!({"mode":"custom","window":{"start":(now+chrono::Duration::hours(1)).format("%H:%M").to_string(),"end":(now+chrono::Duration::hours(2)).format("%H:%M").to_string(),"timezone":"UTC","days":[0,1,2,3,4,5,6],"closing":"call"}})
}
fn settings(f: &Fixture, s: &str, patch: Value) {
    let current = f.call(s, "get_settings", json!({}));
    let response = f.call(
        s,
        "update_settings",
        json!({"expected_revision":current["result"]["revision"],"settings":patch}),
    );
    assert_eq!(response["status"], "complete", "{response}");
}

#[test]
fn million_token_projects_open_and_settings_reject_impossible_response_budgets() {
    // Startup loads the project through runner::load, not just the settings parser.
    let f = Fixture::with_context("http://127.0.0.1:1", 1_048_576);
    let session = f.session();
    let current = f.call(&session, "get_settings", json!({}));
    assert_eq!(current["result"]["settings"]["context_tokens"], 1_048_576);
    settings(&f, &session, json!({"context_tokens":1_000_000}));
    let path = f.dir.path().join("chuggin.json");
    let saved = fs::read(&path).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&saved).unwrap()["context_tokens"],
        1_000_000
    );
    for patch in [
        json!({"context_tokens":4095}),
        json!({"output_tokens":0}),
        json!({"output_tokens":1_000_000}),
        json!({"context_tokens":4096,"output_tokens":8192}),
    ] {
        let current = f.call(&session, "get_settings", json!({}));
        let reply = f.call(
            &session,
            "update_settings",
            json!({"expected_revision":current["result"]["revision"],"settings":patch}),
        );
        assert_ne!(reply["status"], "complete", "{reply}");
        assert_eq!(fs::read(&path).unwrap(), saved);
    }
}
#[test]
fn closing_after_a_response_parks_its_tools_and_one_cycle_override_preserves_manual_hold() {
    let model = Model::new(|_, n| {
        if n == 0 {
            thread::sleep(Duration::from_millis(350));
            task_and_tool("write_file", json!({"path":"new.txt","content":"safe"}))
        } else {
            answer()
        }
    });
    let f = Fixture::new(&model.url);
    let s = f.session();
    f.rpc(json!({"action":"start"}));
    f.wait(|| !model.requests.lock().unwrap().is_empty());
    settings(&f, &s, json!({"active_hours":closed_schedule()}));
    f.wait(|| f.rpc(json!({"action":"status"}))["paused"] == true);
    assert!(!f.dir.path().join("new.txt").exists());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    f.rpc(json!({"action":"pause"}));
    settings(&f, &s, json!({"active_hours":{"mode":"always"}}));
    assert_eq!(f.rpc(json!({"action":"status"}))["paused"], true);
    settings(&f, &s, json!({"active_hours":closed_schedule()}));
    f.rpc(json!({"action":"resume","mode":"one_cycle"}));
    f.wait(|| f.dir.path().join("new.txt").exists());
    f.wait(|| f.rpc(json!({"action":"status"}))["running"] == false);
    let state = f.rpc(json!({"action":"status"}));
    assert_eq!(state["state"]["cycle"], 1, "{state}");
    f.rpc(json!({"action":"stop"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["running"] == false);
}
#[test]
fn all_actors_can_inspect_and_explicitly_stop_a_running_command() {
    let model = Model::new(|_, n| {
        if n == 0 {
            task_and_tool(
                "run_command",
                json!({"argv":["sh","-c","echo started; sleep 30"]}),
            )
        } else {
            thread::sleep(Duration::from_millis(100));
            answer()
        }
    });
    let f = Fixture::new(&model.url);
    let s = f.session();
    f.rpc(json!({"action":"start"}));
    f.wait(|| {
        f.call(&s, "list_commands", json!({}))["result"]["commands"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    });
    f.rpc(json!({"action":"pause"}));
    let commands = f.call(&s, "list_commands", json!({}));
    let id = commands["result"]["commands"][0]["command_id"].clone();
    let status = f.call(&s, "command_status", json!({"command_id":id}));
    assert_eq!(status["result"]["actor"], "loop", "{status}");
    let stopped = f.call(
        &s,
        "stop_command",
        json!({"command_id":id,"reason":"User requested test cancellation"}),
    );
    assert_eq!(stopped["status"], "complete", "{stopped}");
    f.wait(|| f.call(&s, "command_status", json!({"command_id":id}))["result"]["running"] == false);
    assert!(
        f.rpc(json!({"action":"status"}))["active_commands"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.rpc(json!({"action":"stop"}));
}
#[test]
fn operator_check_batch_finishes_in_order_and_keeps_editing_until_all_commands_finish() {
    let f = Fixture::new("http://127.0.0.1:1");
    let s = f.session();
    // Fixtures may configure validation before any run; this is not an operator-permitted setting.
    let path = f.dir.path().join("chuggin.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    c["checks"] = json!([{"argv":["sh","-c","sleep 0.2; echo first >> order"],"timeout_seconds":10},{"argv":["sh","-c","sleep 0.2; echo second >> order"],"timeout_seconds":10}]);
    fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
    let start = f.call(&s, "run_checks", json!({}));
    assert_eq!(start["status"], "complete", "{start}");
    assert_eq!(
        f.call(&s, "end_edit", json!({"summary":"too soon"}))["status"],
        "failed"
    );
    f.wait(|| {
        f.call(
            &s,
            "command_status",
            json!({"command_id":start["result"]["command_id"]}),
        )["result"]["running"]
            == false
    });
    assert_eq!(
        fs::read_to_string(f.dir.path().join("order")).unwrap(),
        "first\nsecond\n"
    );
    assert_eq!(
        f.call(&s, "end_edit", json!({"summary":"Both checks done"}))["status"],
        "complete"
    );
}
#[test]
fn chat_edits_release_ownership_but_preserve_manual_pause_and_keep_complete_history() {
    let model = Model::new(|_, n| {
        if n == 0 {
            tool(
                "write_file",
                json!({"path":"chat.txt","content":"User requested"}),
            )
        } else {
            answer()
        }
    });
    let f = Fixture::new(&model.url);
    let s = f.session();
    f.rpc(json!({"action":"pause"}));
    f.rpc(json!({"action":"chat_send","session_id":s,"message":"Create chat.txt"}));
    f.wait(|| f.rpc(json!({"action":"chat_status","session_id":s}))["busy"] == false);
    f.wait(|| f.rpc(json!({"action":"status"}))["editing"].is_null());
    assert_eq!(
        fs::read_to_string(f.dir.path().join("chat.txt")).unwrap(),
        "User requested"
    );
    assert_eq!(f.rpc(json!({"action":"status"}))["manual_pause"], true);
    assert_eq!(f.rpc(json!({"action":"status"}))["running"], false);
    assert!(
        f.rpc(json!({"action":"chat_status","session_id":s}))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
    );
}

#[test]
fn restart_preserves_action_ids_and_requires_review_of_uncertain_effects() {
    let mut f = Fixture::new("http://127.0.0.1:1");
    let s = f.session();
    let args = json!({"path":"saved.txt","content":"once"});
    assert_eq!(
        f.call_id(&s, "write_file", args.clone(), "save-once")["status"],
        "complete"
    );
    let op = f
        .dir
        .path()
        .join(format!(".chuggin/operator/operations/{s}-uncertain.json"));
    fs::write(op,json!({"status":"running","name":"run_command","arguments":{"argv":["sh","-c","echo NOT REPLAYED"]},"operation_id":"uncertain"}).to_string()).unwrap();
    f.rpc(json!({"action":"force_stop"}));
    f.child.wait().unwrap();
    f.child = Command::new(BIN)
        .arg("engine")
        .arg("--config")
        .arg(f.dir.path().join("chuggin.json"))
        .env("XDG_CONFIG_HOME", f.dir.path().join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while UnixStream::connect(&f.socket).is_err() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    }
    f.rpc(json!({"action":"open_session","session_id":s}));
    assert_eq!(
        f.call_id(&s, "write_file", args, "save-once")["status"],
        "complete"
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("saved.txt")).unwrap(),
        "once"
    );
    let record = f.call(&s, "operation_status", json!({"id":"uncertain"}));
    assert_eq!(record["result"]["status"], "uncertain");
    assert!(
        !f.rpc(json!({"action":"status"}))["holds"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.call(
            &s,
            "resolve_interrupted_action",
            json!({"id":"uncertain","summary":"Verified effects and logs; no replay needed"})
        )["status"],
        "complete"
    );
    let new = f.session();
    assert_eq!(f.call(&new,"recover_edit_session",json!({"previous_session":s,"external_writers_stopped":true,"summary":"Verified file contents and stopped external writers"}))["status"],"complete");
    assert!(
        f.rpc(json!({"action":"status"}))["holds"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.rpc(json!({"action":"status"}))["running"], false);
}
#[test]
fn restoration_requires_an_unchanged_preview_and_preserves_a_recovery_save() {
    let f = Fixture::new("http://127.0.0.1:1");
    let s = f.session();
    f.call(
        &s,
        "write_file",
        json!({"path":"file.txt","content":"changed","expected_content":"before\n"}),
    );
    let preview = f.call(&s, "preview_restore", json!({"target":"HEAD"}));
    assert_eq!(preview["status"], "complete", "{preview}");
    let args = json!({"target":preview["result"]["target"],"expected_tree":preview["result"]["expected_tree"],"confirmed":true});
    fs::write(f.dir.path().join("file.txt"), "human edit").unwrap();
    assert_eq!(f.call(&s, "restore_checkpoint", args)["status"], "failed");
    let preview = f.call(&s, "preview_restore", json!({"target":"HEAD"}));
    let restored=f.call(&s,"restore_checkpoint",json!({"target":preview["result"]["target"],"expected_tree":preview["result"]["expected_tree"],"confirmed":true}));
    assert_eq!(restored["status"], "complete", "{restored}");
    assert!(restored["result"]["backup"].is_string());
    assert_eq!(
        fs::read_to_string(f.dir.path().join("file.txt")).unwrap(),
        "before\n"
    );
}

#[test]
fn stopping_a_held_response_does_not_execute_its_pending_tools() {
    let model = Model::new(|_, _| {
        thread::sleep(Duration::from_millis(200));
        task_and_tool(
            "write_file",
            json!({"path":"new.txt","content":"must not run"}),
        )
    });
    let f = Fixture::new(&model.url);
    f.rpc(json!({"action":"start"}));
    f.wait(|| !model.requests.lock().unwrap().is_empty());
    f.rpc(json!({"action":"pause"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["paused"] == true);
    f.rpc(json!({"action":"stop"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["running"] == false);
    assert!(!f.dir.path().join("new.txt").exists());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[test]
fn helper_permissions_require_human_settings_and_preserve_other_settings() {
    let f = Fixture::new("http://127.0.0.1:1");
    let session = f.session();
    let current = f.call(&session, "get_settings", json!({}));
    let original = fs::read(f.dir.path().join("chuggin.json")).unwrap();
    let refused = f.call(
        &session,
        "update_settings",
        json!({"expected_revision":current["result"]["revision"],"settings":{"helpers":{"enabled":true,"model":"groq/UNAUTHORIZED","max_calls":100}}}),
    );
    assert_eq!(refused["status"], "failed");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("human connection setup")
    );
    assert_eq!(
        fs::read(f.dir.path().join("chuggin.json")).unwrap(),
        original
    );
    let granted = f.rpc(json!({"action":"helper_settings","expected_revision":current["result"]["revision"],"helpers":{"enabled":true,"model":" groq/USER_APPROVED ","max_calls":24}}));
    assert_eq!(granted["status"], "complete");
    assert_eq!(
        granted["result"]["settings"]["helpers"]["model"],
        "groq/USER_APPROVED"
    );
    assert_eq!(granted["result"]["settings"]["helpers"]["max_calls"], 24);
    assert_eq!(
        granted["result"]["settings"]["model"],
        current["result"]["settings"]["model"]
    );
    assert_eq!(f.rpc(json!({"action":"status"}))["running"], false);
    assert_ne!(granted["result"]["revision"], current["result"]["revision"]);
    let mut stream = UnixStream::connect(&f.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    writeln!(stream, "{}", json!({"action":"helper_settings","expected_revision":current["result"]["revision"],"helpers":{"enabled":false,"model":"","max_calls":12}})).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    let stale: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(stale["ok"], false);
    let latest = f.call(&session, "get_settings", json!({}));
    assert_eq!(
        latest["result"]["settings"]["helpers"],
        granted["result"]["settings"]["helpers"]
    );
}

#[test]
fn stopping_a_paused_investigation_retains_partial_work_without_extra_requests() {
    let model = Model::new(|body, _| {
        let helper = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|schema| schema["function"]["name"] == "report_investigation");
        if helper {
            thread::sleep(Duration::from_millis(400));
            json!({"role":"assistant","content":"PARTIAL_HELPER_OBSERVATION","tool_calls":[{"id":"helper-read","function":{"name":"read_file","arguments":{"path":"file.txt"}}}]})
        } else {
            task_and_tool(
                "delegate_investigation",
                json!({"question":"Inspect the fixture value","evidence":["file.txt"]}),
            )
        }
    });
    let f = Fixture::new(&model.url);
    let session = f.session();
    let current = f.call(&session, "get_settings", json!({}));
    f.rpc(json!({"action":"helper_settings","expected_revision":current["result"]["revision"],"helpers":{"enabled":true,"model":"","max_calls":8}}));
    f.rpc(json!({"action":"start","mode":"one_cycle"}));
    f.wait(|| model.requests.lock().unwrap().len() >= 2);
    f.rpc(json!({"action":"pause"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["paused"] == true);
    f.rpc(json!({"action":"stop"}));
    f.wait(|| f.rpc(json!({"action":"status"}))["running"] == false);
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "Stopping a held investigation must not launch another inference request"
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("file.txt")).unwrap(),
        "before\n"
    );
    let jobs: Vec<_> = fs::read_dir(f.dir.path().join(".chuggin/agents"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(jobs.len(), 1);
    let job: Value = serde_json::from_slice(&fs::read(jobs[0].join("job.json")).unwrap()).unwrap();
    assert_eq!(job["status"], "interrupted");
    assert_eq!(job["calls_used"], 1);
    assert!(
        job["messages"]
            .to_string()
            .contains("PARTIAL_HELPER_OBSERVATION")
    );
    assert!(job["result"].is_null());
}
