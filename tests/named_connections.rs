#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_chuggin");
const RAW_MODEL: &str = "org/shared-model:latest";
const TEST_KEY: &str = "named-connection-test-private-credential";

struct Request {
    path: String,
    body: Value,
    authorized: bool,
}

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    done: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(
        key: Option<&'static str>,
        mut reply: impl FnMut(&Request) -> (&'static str, String) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let handle = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let request = read_request(&socket, key);
                let (content_type, body) = reply(&request);
                captured.lock().unwrap().push(request);
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
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

    fn ollama(label: &'static str, prefix: &'static str) -> Self {
        Self::new(None, move |request| {
            let body = if request.path == format!("{prefix}/api/chat") {
                json!({"message":{"role":"assistant","content":label},"done":true,
                    "prompt_eval_count":100,"eval_count":5})
            } else if request.path == format!("{prefix}/api/tags") {
                json!({"models":[{"name":RAW_MODEL},{"name":format!("only-on-{label}")}]})
            } else {
                json!({"capabilities":["completion","tools"],
                    "model_info":{"general.architecture":"fixture","fixture.context_length":32768}})
            };
            ("application/x-ndjson", body.to_string() + "\n")
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn read_request(socket: &TcpStream, key: Option<&str>) -> Request {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = 0;
    let mut authorized = key.is_none();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            } else if name.eq_ignore_ascii_case("authorization") {
                // Retain only the match result; failure output must never print
                // the credential or a captured Authorization header.
                authorized = key.is_some_and(|key| value.trim() == format!("Bearer {key}"));
            }
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    Request {
        path: first.split_whitespace().nth(1).unwrap().to_owned(),
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
        authorized,
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    project: PathBuf,
    global: PathBuf,
    registry: PathBuf,
}

impl Fixture {
    fn new(connections: Value, route: &str, calls: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let global = directory.path().join("global");
        let connection_dir = global.join("chuggin/connections");
        fs::create_dir_all(project.join("repo")).unwrap();
        fs::create_dir_all(&connection_dir).unwrap();
        fs::set_permissions(&connection_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = connection_dir.join("registry.json");
        fs::write(
            &registry,
            json!({"next_id":3,"connections":connections}).to_string(),
        )
        .unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            project.join("chuggin.json"),
            json!({"repo":project.join("repo"),"state_dir":project.join("state"),
                "activity_mode":"chaos","model":route,"ollama_url":"",
                "context_tokens":32768,"output_tokens":1024,"implementation_calls":calls,
                "retry_seconds":0,"request_timeout_seconds":3,"checks":[]})
            .to_string(),
        )
        .unwrap();
        Self {
            _directory: directory,
            project,
            global,
            registry,
        }
    }

    fn select(&self, route: &str) {
        let path = self.project.join("chuggin.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["model"] = json!(route);
        fs::write(path, config.to_string()).unwrap();
    }

    fn run(&self) {
        let log_path = self.project.join("run.log");
        let log = fs::File::create(&log_path).unwrap();
        let mut child = Command::new(BIN)
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", &self.global)
            .args(["run", "--cycles", "1"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let output = fs::read_to_string(log_path).unwrap();
        assert!(
            !output.contains(TEST_KEY),
            "Credential appeared in CLI output"
        );
        assert!(
            status.is_some_and(|status| status.success()),
            "Named connection run failed or exceeded its deadline: {}",
            output.replace(TEST_KEY, "[redacted]")
        );
    }

    fn conversation(&self) -> Value {
        serde_json::from_slice(&fs::read(self.project.join("state/conversation.json")).unwrap())
            .unwrap()
    }
}

fn assert_no_credential(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_credential(&path);
        } else {
            let bytes = fs::read(&path).unwrap();
            assert!(
                !bytes
                    .windows(TEST_KEY.len())
                    .any(|part| part == TEST_KEY.as_bytes()),
                "Credential appeared in a project file: {}",
                path.display()
            );
        }
    }
}

#[test]
fn switching_named_ollama_routes_preserves_other_connections_and_raw_model_identity() {
    let first = Server::ollama("FIRST_NAMED_OLLAMA_REPLY", "/first");
    let second = Server::ollama("SECOND_NAMED_OLLAMA_REPLY", "/second");
    let fixture = Fixture::new(
        json!([
            {"id":1,"name":"Desk GPU","kind":"ollama","base_url":format!("{}/first",first.url)},
            {"id":2,"name":"Other GPU","kind":"ollama","base_url":format!("{}/second",second.url)}
        ]),
        &format!("connection/1/{RAW_MODEL}"),
        1,
    );
    let registry = fs::read(&fixture.registry).unwrap();

    fixture.run();
    assert!(
        fixture
            .conversation()
            .to_string()
            .contains("FIRST_NAMED_OLLAMA_REPLY")
    );
    assert!(second.requests.lock().unwrap().is_empty());
    fixture.select(&format!("connection/2/{RAW_MODEL}"));
    fixture.run();

    assert!(
        fixture
            .conversation()
            .to_string()
            .contains("SECOND_NAMED_OLLAMA_REPLY")
    );
    for (server, prefix) in [(&first, "/first"), (&second, "/second")] {
        let requests = server.requests.lock().unwrap();
        let calls: Vec<_> = requests
            .iter()
            .filter(|r| r.path.ends_with("/api/chat"))
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body["model"], RAW_MODEL);
        assert!(
            requests
                .iter()
                .all(|r| r.path.starts_with(prefix) && r.authorized)
        );
    }
    assert_eq!(fs::read(&fixture.registry).unwrap(), registry);
    let config: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("chuggin.json")).unwrap()).unwrap();
    assert_eq!(config["model"], format!("connection/2/{RAW_MODEL}"));
    assert_eq!(config["ollama_url"], "");
}

#[test]
fn custom_openai_route_streams_tools_and_continuation_without_exposing_its_key() {
    let mut generation = 0;
    let server = Server::new(Some(TEST_KEY), move |request| {
        if request.path.ends_with("/models") {
            return (
                "application/json",
                json!({"data":[{"id":RAW_MODEL,
                "context_length":32768,"max_output_tokens":8192,
                "supported_parameters":["temperature"]}]})
                .to_string(),
            );
        }
        generation += 1;
        let chunks = if generation == 1 {
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"named_write",
                    "type":"function","function":{"name":"write_file","arguments":"{\"path\":\"named-result.txt\","}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{
                    "arguments":"\"content\":\"Written through the custom connection\"}"}}]},"finish_reason":"tool_calls"}]}),
            ]
        } else {
            vec![
                json!({"choices":[{"delta":{"content":"CUSTOM_NAMED_TOOL_FINISHED"},"finish_reason":"stop"}]}),
            ]
        };
        (
            "text/event-stream",
            chunks
                .iter()
                .map(|chunk| format!("data: {chunk}\n\n"))
                .collect::<String>()
                + "data: [DONE]\n\n",
        )
    });
    let fixture = Fixture::new(
        json!([{"id":1,"name":"Custom Gateway","kind":"openai_compatible",
            "base_url":format!("{}/proxy/v1",server.url)}]),
        &format!("connection/1/{RAW_MODEL}"),
        2,
    );
    let key_path = fixture.global.join("chuggin/connections/1.key");
    fs::write(&key_path, TEST_KEY).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    // An explicit override must discover the endpoint's advertised controls;
    // provider-default inference intentionally skips metadata requests.
    let config_path = fixture.project.join("chuggin.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["sampling"] = json!({"mode":"focused"});
    fs::write(config_path, config.to_string()).unwrap();

    fixture.run();

    assert_eq!(
        fs::read_to_string(fixture.project.join("repo/named-result.txt")).unwrap(),
        "Written through the custom connection"
    );
    assert!(
        fixture
            .conversation()
            .to_string()
            .contains("CUSTOM_NAMED_TOOL_FINISHED")
    );
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().all(|request| request.authorized));
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/proxy/v1/models")
    );
    let calls: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/proxy/v1/chat/completions")
        .collect();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.body["model"], RAW_MODEL);
        assert_eq!(call.body["stream"], true);
        assert_eq!(call.body["max_tokens"], 1024);
        assert_eq!(call.body["temperature"], 0.4);
        for absent in ["provider", "options", "max_completion_tokens"] {
            assert!(call.body.get(absent).is_none());
        }
    }
    let messages = calls[1].body["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find(|message| message["tool_calls"][0]["id"] == "named_write")
        .unwrap();
    assert_eq!(call["tool_calls"][0]["function"]["name"], "write_file");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "named_write")
        .unwrap();
    let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["ok"], true);
    assert_no_credential(&fixture.project);
    assert!(
        !fs::read_to_string(&fixture.registry)
            .unwrap()
            .contains(TEST_KEY)
    );
}
