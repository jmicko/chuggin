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
const BIN: &str = env!("CARGO_BIN_EXE_chuggin");

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    done: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(wizard: bool, delay: bool) -> Self {
        Self::serve(wizard, delay, None, false)
    }
    fn serve(wizard: bool, delay: bool, tools_mode: Option<u8>, timeout_followup: bool) -> Self {
        Self::serve_with_edit(wizard, delay, tools_mode, timeout_followup, None)
    }
    fn serve_with_edit(
        wizard: bool,
        delay: bool,
        tools_mode: Option<u8>,
        timeout_review: bool,
        edit: Option<String>,
    ) -> Self {
        Self::serve_with_repetition(wizard, delay, tools_mode, timeout_review, edit, None)
    }
    fn serve_with_repetition(
        wizard: bool,
        delay: bool,
        tools_mode: Option<u8>,
        timeout_review: bool,
        edit: Option<String>,
        repetition_stage: Option<usize>,
    ) -> Self {
        let mut cycle = 0;
        let mut awaiting_completion = false;
        Self::custom(delay, timeout_review, repetition_stage, move |body, n| {
            if wizard {
                return (
                    json!({"goal":format!("Build a useful editor. Draft {}.", n+1)}).to_string(),
                    json!([]),
                );
            }
            if let Some(mode) = tools_mode {
                return dev_tool_reply(body, mode);
            }
            if awaiting_completion {
                awaiting_completion = false;
                return ("PRIVATE_IMPLEMENTATION_TRANSCRIPT".into(), json!([]));
            }
            awaiting_completion = true;
            cycle += 1;
            (
                String::new(),
                json!([
                    task_tool(&format!("Increment {cycle}")),
                    {"function":{"name":"save_progress_note","arguments":{"note":format!("Task-local observation for cycle {cycle}")}}},
                    {"function":{"name":"write_file","arguments":{"path":"value.txt","content":edit.clone().unwrap_or_else(|| cycle.to_string())}}}
                ]),
            )
        })
    }
    fn custom(
        delay: bool,
        timeout_review: bool,
        repetition_request: Option<usize>,
        mut reply: impl FnMut(&Value, usize) -> (String, Value) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = requests.clone();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let handle = thread::spawn(move || {
            let mut review_timed_out = false;
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
                        thread::sleep(Duration::from_millis(800));
                    }
                    if timeout_review && n == 1 && !review_timed_out {
                        review_timed_out = true;
                        thread::sleep(Duration::from_millis(1200));
                        continue;
                    }
                    if repetition_request == Some(n) || repetition_request == Some(usize::MAX) {
                        // A stream containing a loop and an unexecuted tool call. It never
                        // completes: recovery must happen while reading, not after done=true.
                        let prefix = json!({"message":{"content":"The relevant file is value.txt.\n"},"done":false}).to_string()+"\n";
                        prefix + &(0..20).map(|_| json!({"message":{"content":"I'll try that now. Wait, let me think about it. ","tool_calls":if n == 1 {json!([{"function":{"name":"write_file","arguments":{"path":"value.txt","content":"MUST_NOT_EXECUTE"}}}])} else {json!([])}},"done":false}).to_string()+"\n").collect::<String>()
                    } else {
                        let (content, tools) = reply(&body, n);
                        if matches!(
                            content.as_str(),
                            "__FAIL_REQUEST__" | "__HTTP_LIMIT__" | "__CREDIT_LIMIT__"
                        ) {
                            content
                        } else if content == "__STREAM_LIMIT__" {
                            json!({"error":"weekly usage limit reached"}).to_string() + "\n"
                        } else {
                            json!({"message":{"role":"assistant","content":content,"tool_calls":tools},"done":true,"done_reason":"stop"}).to_string()+"\n"
                        }
                    }
                };
                let status = match data.as_str() {
                    "__FAIL_REQUEST__" => "400 Bad Request",
                    "__HTTP_LIMIT__" => "429 Too Many Requests\nRetry-After: 1",
                    "__CREDIT_LIMIT__" => "402 Payment Required",
                    _ => "200 OK",
                };
                let response = format!(
                    "HTTP/1.1 {status}
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
fn is_work(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "write_file")
    })
}
fn task_tool(title: &str) -> Value {
    json!({"function":{"name":"set_task","arguments":{"title":title,"objective":"Improve value","acceptance":["Value updated"],"files":["value.txt"]}}})
}
fn state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("state/state.json")).unwrap()).unwrap()
}
fn run_cycles(root: &Path, cycles: u64) {
    let out = command(root)
        .args(["run", "--cycles", &cycles.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
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
    let config = root.join("chuggin.json");
    fs::write(&config,json!({"repo":repo,"goal":"Improve value incrementally","ollama_url":url,"model":"fake",
        "context_tokens":32768,"output_tokens":4096,"implementation_calls":16,
        "checks":[{"argv":argv,"timeout_seconds":5}],"state_dir":root.join("state"),"retry_seconds":1}).to_string()).unwrap();
    config
}
fn command(root: &Path) -> Command {
    fs::create_dir_all(root.join(".chuggin")).unwrap();
    let identity = root.join(".chuggin/test-gitconfig");
    fs::write(
        &identity,
        "[user]\nname = Test Operator\nemail = operator@example.com\n",
    )
    .unwrap();
    let mut c = Command::new(BIN);
    c.current_dir(root)
        .env("GIT_CONFIG_GLOBAL", identity)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", root.join(".chuggin/global"));
    c
}

#[test]
fn missing_git_identity_blocks_before_model_requests() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    for key in ["user.name", "user.email"] {
        let mut cmd = command(root.path());
        let identity = root.path().join(".chuggin/test-gitconfig");
        let remaining = if key == "user.name" {
            "[user]\nemail = operator@example.com\n"
        } else {
            "[user]\nname = Test Operator\n"
        };
        fs::write(identity, remaining).unwrap();
        let out = cmd.args(["run", "--cycles", "1"]).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("missing {key}")));
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(!root.path().join("state/state.json").exists());
    }
}

#[test]
fn checkpoints_advance_in_one_workspace_and_keep_the_user_checkout_isolated() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    let initial = git(&repo, &["rev-parse", "HEAD"]);
    run_cycles(root.path(), 1);
    let first = state(root.path());
    run_cycles(root.path(), 1);
    let second = state(root.path());
    assert_eq!(second["schema_version"], 2);
    assert_eq!(first["working_workspace"], second["working_workspace"]);
    assert_ne!(first["working_ref"], second["working_ref"]);
    assert_eq!(second["last_checks_passed_ref"], second["working_ref"]);
    assert_eq!(
        fs::read_to_string(
            Path::new(second["working_workspace"].as_str().unwrap()).join("value.txt")
        )
        .unwrap(),
        "2"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), initial);
    assert_eq!(
        fs::read_to_string(repo.join("value.txt")).unwrap(),
        "USER_EDIT"
    );
    assert_eq!(
        git(
            &repo,
            &[
                "log",
                "-1",
                "--format=%an <%ae>|%cn <%ce>",
                second["working_ref"].as_str().unwrap()
            ]
        ),
        "Test Operator <operator@example.com>|Test Operator <operator@example.com>"
    );
    let requests = server.requests.lock().unwrap();
    let original_history = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        &requests[2]["messages"].as_array().unwrap()[..original_history.len()],
        original_history
    );
    assert!(requests.iter().all(
        |request| request["messages"][0] == requests[0]["messages"][0]
            && request["tools"] == requests[0]["tools"]
    ));
    assert!(
        requests[2]
            .to_string()
            .contains("PRIVATE_IMPLEMENTATION_TRANSCRIPT")
    );
    drop(requests);
    let mut changed: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    changed["goal"] = json!("Changed goal");
    fs::write(&config, changed.to_string()).unwrap();
    assert!(
        !command(root.path())
            .args(["run", "--cycles", "1"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn failing_work_is_checkpointed_then_repaired_across_cycles_and_process_restarts() {
    for restart in [false, true] {
        let mut attempt = 0;
        let mut edited = false;
        let server = Server::custom(false, false, None, move |_, _| {
            if edited {
                attempt += 1;
                edited = false;
                return ("The current attempt is ready for checks.".into(), json!([]));
            }
            edited = true;
            let script = if attempt == 0 {
                "printf BROKEN > value.txt"
            } else {
                "test \"$(cat value.txt)\" = BROKEN && printf FIXED > value.txt"
            };
            (
                String::new(),
                json!([task_tool("Repair the value"),{"function":{"name":"run_command","arguments":{"argv":["sh","-c",script]}}}]),
            )
        });
        let root = tempfile::tempdir().unwrap();
        let config = fixture(root.path(), &server.url, true);
        let mut settings: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        settings["implementation_calls"] = json!(2);
        settings["checks"] =
            json!([{"argv":["sh","-c","test \"$(cat value.txt)\" = FIXED"],"timeout_seconds":5}]);
        fs::write(config, settings.to_string()).unwrap();
        if restart {
            run_cycles(root.path(), 1);
            let failed = state(root.path());
            assert_eq!(
                failed["recent"][0]["disposition"],
                "checkpoint/checks-failing"
            );
            assert!(failed["last_checks_passed_ref"].is_null());
            assert_eq!(
                fs::read_to_string(
                    Path::new(failed["working_workspace"].as_str().unwrap()).join("value.txt")
                )
                .unwrap(),
                "BROKEN"
            );
            assert_eq!(
                git(
                    &root.path().join("repo"),
                    &[
                        "show",
                        &format!("{}:value.txt", failed["working_ref"].as_str().unwrap())
                    ]
                ),
                "BROKEN"
            );
            run_cycles(root.path(), 1);
        } else {
            run_cycles(root.path(), 2);
        }
        let saved = state(root.path());
        let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
        assert_eq!(
            fs::read_to_string(workspace.join("value.txt")).unwrap(),
            "FIXED"
        );
        assert_eq!(saved["last_checks_passed_ref"], saved["working_ref"]);
        let requests = server.requests.lock().unwrap();
        assert!(
            requests.iter().all(is_work),
            "Planning, coding and refinement share the same tools"
        );
        assert!(
            requests
                .iter()
                .filter(|body| is_work(body))
                .any(|body| body.to_string().contains("BROKEN"))
        );
        let first_history = requests[1]["messages"].as_array().unwrap();
        assert_eq!(
            &requests[2]["messages"].as_array().unwrap()[..first_history.len()],
            first_history,
            "The repair retains the working conversation across a cycle or process restart"
        );
    }
}

#[test]
fn unfinished_and_failed_attempts_preserve_deletions_renames_and_new_files() {
    for failing_checks in [false, true] {
        let mut edited = false;
        let server = Server::custom(false, false, None, move |_, _| {
            if edited {
                return (
                    "This change is unfinished and needs further work.".into(),
                    json!([]),
                );
            }
            edited = true;
            (
                String::new(),
                json!([task_tool("Reorganize document files"),{"function":{"name":"run_command","arguments":{"argv":["sh","-c","mv value.txt renamed.txt && printf 'new notes' > notes.txt"]}}}]),
            )
        });
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, !failing_checks);
        run_cycles(root.path(), 1);
        let saved = state(root.path());
        let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
        assert!(!workspace.join("value.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.join("renamed.txt")).unwrap(),
            "USER_EDIT"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("notes.txt")).unwrap(),
            "new notes"
        );
        assert_eq!(git(workspace, &["status", "--porcelain"]), "");
        assert_eq!(
            git(workspace, &["ls-tree", "-r", "--name-only", "HEAD"]),
            "notes.txt\nrenamed.txt"
        );
        assert!(
            saved["current_task"].is_object(),
            "An unfinished task remains available for follow-up"
        );
    }
}

#[test]
fn model_error_after_editing_still_checkpoints_completed_tool_changes() {
    let mut edited = false;
    let server = Server::custom(false, false, None, move |_, _| {
        if edited {
            return ("__FAIL_REQUEST__".into(), json!([]));
        }
        edited = true;
        (
            String::new(),
            json!([task_tool("Keep unfinished work"), {"function":{"name":"write_file","arguments":{"path":"value.txt","content":"KEEP_AFTER_NETWORK_ERROR"}}}]),
        )
    });
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    run_cycles(root.path(), 1);
    let saved = state(root.path());
    let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "KEEP_AFTER_NETWORK_ERROR"
    );
    assert_eq!(
        git(workspace, &["show", "HEAD:value.txt"]),
        "KEEP_AFTER_NETWORK_ERROR"
    );
    assert!(saved["current_task"].is_object());
    assert!(
        fs::read_dir(root.path().join("state/cycle-000001"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("request-failure-"))
    );
}

#[test]
fn finish_task_records_completion_only_when_checks_pass_and_always_saves_edits() {
    for pass in [false, true] {
        let server = Server::custom(false, false, None, |_, _| {
            (
                String::new(),
                json!([task_tool("Finish the current task"),{"function":{"name":"write_file","arguments":{"path":"value.txt","content":"COMPLETED_EDIT"}}},{"function":{"name":"finish_task","arguments":{"summary":"The value was updated and is ready for validation."}}}]),
            )
        });
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, pass);
        run_cycles(root.path(), 1);
        let saved = state(root.path());
        assert_eq!(saved["current_task"].is_null(), pass);
        assert_eq!(
            fs::read_to_string(
                Path::new(saved["working_workspace"].as_str().unwrap()).join("value.txt")
            )
            .unwrap(),
            "COMPLETED_EDIT"
        );
        assert_eq!(saved["last_checks_passed_ref"].is_string(), pass);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn legacy_migration_recovers_the_entire_interrupted_workspace_including_staged_changes() {
    let mut edited = false;
    let server = Server::custom(false, false, None, move |_, _| {
        if edited {
            return (
                "Recovered unfinished files and continued.".into(),
                json!([]),
            );
        }
        edited = true;
        (
            String::new(),
            json!([task_tool("Continue interrupted reorganization"), {"function":{"name":"run_command","arguments":{"argv":["sh","-c","test ! -e value.txt && test \"$(cat renamed.txt)\" = DIRTY_CANDIDATE && test \"$(cat staged.txt)\" = STAGED_CANDIDATE && printf resumed > resumed.txt"]}}}]),
        )
    });
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    let initial = git(&repo, &["rev-parse", "HEAD"]);
    let state_dir = root.path().join("state");
    let art = state_dir.join("cycle-000001");
    fs::create_dir_all(&art).unwrap();
    let baseline = state_dir.join("baseline");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            baseline.to_str().unwrap(),
            &initial,
        ],
    );
    let candidate = art.join("workspace");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "codex/legacy-interrupted",
            candidate.to_str().unwrap(),
            &initial,
        ],
    );
    fs::rename(candidate.join("value.txt"), candidate.join("renamed.txt")).unwrap();
    fs::write(candidate.join("renamed.txt"), "DIRTY_CANDIDATE").unwrap();
    fs::write(candidate.join("staged.txt"), "STAGED_CANDIDATE").unwrap();
    git(&candidate, &["add", "staged.txt"]);
    let original_state = json!({"run_id":"legacy-run","goal":"Improve value incrementally","repo":repo,"cycle":1,"accepted_ref":initial,"accepted_branch":"","accepted_workspace":baseline,"recent":[]});
    fs::write(state_dir.join("state.json"), original_state.to_string()).unwrap();
    fs::write(
        art.join("attempt.json"),
        json!({"base":initial,"branch":"codex/legacy-interrupted","workspace":candidate})
            .to_string(),
    )
    .unwrap();
    fs::write(art.join("task.json"), json!({"title":"Continue interrupted reorganization","objective":"Retain unfinished work","acceptance":["Files reorganized"],"files":["value.txt","renamed.txt","staged.txt"],"out_of_scope":[]}).to_string()).unwrap();
    run_cycles(root.path(), 1);
    let saved = state(root.path());
    assert_eq!(saved["schema_version"], 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(state_dir.join("state-v1-backup.json")).unwrap())
            .unwrap(),
        original_state
    );
    let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
    assert!(!workspace.join("value.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("renamed.txt")).unwrap(),
        "DIRTY_CANDIDATE"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("staged.txt")).unwrap(),
        "STAGED_CANDIDATE"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("resumed.txt")).unwrap(),
        "resumed"
    );
    assert_eq!(git(workspace, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(repo.join("value.txt")).unwrap(),
        "USER_EDIT"
    );
}

#[test]
fn legacy_migration_prefers_edited_work_over_a_newer_empty_attempt() {
    for only_untracked in [false, true] {
        let server = Server::new(false, false);
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, true);
        let repo = root.path().join("repo");
        let initial = git(&repo, &["rev-parse", "HEAD"]);
        let state_dir = root.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let baseline = state_dir.join("baseline");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                baseline.to_str().unwrap(),
                &initial,
            ],
        );
        for cycle in [1, 2] {
            let artifact = state_dir.join(format!("cycle-{cycle:06}"));
            fs::create_dir_all(&artifact).unwrap();
            let workspace = artifact.join("workspace");
            let branch = format!("codex/legacy-attempt-{cycle}");
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    workspace.to_str().unwrap(),
                    &initial,
                ],
            );
            fs::write(
                artifact.join("attempt.json"),
                json!({"base":initial,"branch":branch,"workspace":workspace}).to_string(),
            )
            .unwrap();
            fs::write(artifact.join("task.json"), json!({"title":format!("Attempt {cycle}"),"objective":"Improve project content","acceptance":["Content updated"],"files":["value.txt"],"out_of_scope":[]}).to_string()).unwrap();
        }
        let edited = state_dir.join("cycle-000001/workspace");
        let empty = state_dir.join("cycle-000002/workspace");
        fs::write(
            edited.join("unfinished-notes.txt"),
            "Retain these untracked findings",
        )
        .unwrap();
        if !only_untracked {
            fs::write(edited.join("value.txt"), "STAGED_PROGRESS").unwrap();
            fs::write(edited.join("new-module.txt"), "Staged new project content").unwrap();
            git(&edited, &["add", "value.txt", "new-module.txt"]);
        }
        let edited_status = git(&edited, &["status", "--porcelain"]);
        let edited_index = git(&edited, &["write-tree"]);
        let legacy = json!({"run_id":"legacy-empty-last","goal":"Improve value incrementally","repo":repo,"cycle":2,"accepted_ref":initial,"accepted_branch":"","accepted_workspace":baseline,"recent":[]});
        fs::write(state_dir.join("state.json"), legacy.to_string()).unwrap();
        run_cycles(root.path(), 0);
        let saved = state(root.path());
        assert_eq!(
            saved["working_workspace"],
            edited.to_str().unwrap(),
            "A newer empty workspace must not hide actual unfinished work"
        );
        assert_eq!(saved["current_task"]["title"], "Attempt 1");
        assert_eq!(
            fs::read_to_string(edited.join("unfinished-notes.txt")).unwrap(),
            "Retain these untracked findings"
        );
        if !only_untracked {
            assert_eq!(
                fs::read_to_string(edited.join("value.txt")).unwrap(),
                "STAGED_PROGRESS"
            );
            assert_eq!(
                fs::read_to_string(edited.join("new-module.txt")).unwrap(),
                "Staged new project content"
            );
        }
        assert_eq!(git(&edited, &["status", "--porcelain"]), edited_status);
        assert_eq!(git(&edited, &["write-tree"]), edited_index);
        assert_eq!(fs::read_to_string(empty.join("value.txt")).unwrap(), "0");
        assert_eq!(git(&empty, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(repo.join("value.txt")).unwrap(),
            "USER_EDIT"
        );
        assert!(server.requests.lock().unwrap().is_empty());
    }
}

fn dirty_checkout_fixture(root: &Path, url: &str) -> (String, String, String) {
    fixture(root, url, true);
    let repo = root.join("repo");
    fs::write(repo.join("deleted.txt"), "Delete this tracked file").unwrap();
    fs::write(repo.join("old-name.txt"), "Preserve the renamed file").unwrap();
    fs::write(repo.join("tracked.bin"), [0, 1, 2, 255]).unwrap();
    fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    git(
        &repo,
        &[
            "add",
            "deleted.txt",
            "old-name.txt",
            "tracked.bin",
            ".gitignore",
        ],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Tracked project artifacts",
        ],
    );
    fs::remove_file(repo.join("deleted.txt")).unwrap();
    fs::rename(repo.join("old-name.txt"), repo.join("new-name.txt")).unwrap();
    fs::write(repo.join("tracked.bin"), [255, 0, 13, 10, 128]).unwrap();
    fs::write(repo.join("new.bin"), [0, 254, 252, 10]).unwrap();
    fs::write(repo.join("new-document.txt"), "Untracked project work").unwrap();
    fs::write(repo.join("staged.txt"), "Staged project work").unwrap();
    git(&repo, &["add", "staged.txt"]);
    fs::write(
        repo.join("staged.txt"),
        "Further unstaged work after staging",
    )
    .unwrap();
    fs::write(repo.join("ignored.txt"), "Do not import ignored files").unwrap();
    fs::create_dir_all(repo.join(".chuggin")).unwrap();
    fs::write(repo.join(".chuggin/runtime.txt"), "Harness runtime state").unwrap();
    fs::write(repo.join("chuggin.json"), "Harness configuration").unwrap();
    (
        git(&repo, &["rev-parse", "HEAD"]),
        git(&repo, &["status", "--porcelain"]),
        git(&repo, &["write-tree"]),
    )
}

fn assert_imported_dirty_checkout(root: &Path, original: &(String, String, String)) {
    let repo = root.join("repo");
    let saved = state(root);
    assert_eq!(
        saved["cycle"], 0,
        "Preparing a workspace must not start a model cycle"
    );
    let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
    assert_ne!(workspace, repo);
    assert!(workspace.starts_with(root.join("state")));
    assert_eq!(
        fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "USER_EDIT"
    );
    assert!(!workspace.join("deleted.txt").exists());
    assert!(!workspace.join("old-name.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("new-name.txt")).unwrap(),
        "Preserve the renamed file"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("new-document.txt")).unwrap(),
        "Untracked project work"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("staged.txt")).unwrap(),
        "Further unstaged work after staging"
    );
    assert_eq!(
        fs::read(workspace.join("tracked.bin")).unwrap(),
        [255, 0, 13, 10, 128]
    );
    assert_eq!(
        fs::read(workspace.join("new.bin")).unwrap(),
        [0, 254, 252, 10]
    );
    assert!(!workspace.join("ignored.txt").exists());
    assert!(!workspace.join(".chuggin/runtime.txt").exists());
    assert!(!workspace.join("chuggin.json").exists());
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), original.0);
    assert_eq!(git(&repo, &["status", "--porcelain"]), original.1);
    assert_eq!(
        git(&repo, &["write-tree"]),
        original.2,
        "Import must leave the user's staging area intact"
    );
    assert_eq!(
        fs::read(repo.join("tracked.bin")).unwrap(),
        [255, 0, 13, 10, 128]
    );
    assert_eq!(
        fs::read_to_string(repo.join("staged.txt")).unwrap(),
        "Further unstaged work after staging"
    );
}

#[test]
fn first_run_imports_current_dirty_project_files_including_binaries_and_deletions() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let original = dirty_checkout_fixture(root.path(), &server.url);
    run_cycles(root.path(), 0);
    assert_imported_dirty_checkout(root.path(), &original);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn legacy_root_workspace_migrates_all_dirty_files_without_starting_a_cycle() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let original = dirty_checkout_fixture(root.path(), &server.url);
    let state_dir = root.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let legacy = json!({"run_id":"legacy-root","goal":"Improve value incrementally","repo":root.path().join("repo"),"cycle":0,"accepted_ref":original.0,"accepted_branch":"","accepted_workspace":root.path().join("repo"),"recent":[]});
    fs::write(state_dir.join("state.json"), legacy.to_string()).unwrap();
    run_cycles(root.path(), 0);
    assert_imported_dirty_checkout(root.path(), &original);
    assert_eq!(state(root.path())["schema_version"], 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(state_dir.join("state-v1-backup.json")).unwrap())
            .unwrap(),
        legacy
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn orphan_working_checkout_retains_its_actual_head_and_unfinished_files() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let original = dirty_checkout_fixture(root.path(), &server.url);
    let repo = root.path().join("repo");
    let state_dir = root.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let workspace = state_dir.join("working");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "codex/orphan-work",
            workspace.to_str().unwrap(),
            &original.0,
        ],
    );
    fs::write(workspace.join("value.txt"), "ORPHAN_COMMITTED").unwrap();
    git(&workspace, &["add", "value.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Checkpoint whose state save was interrupted",
        ],
    );
    let orphan_head = git(&workspace, &["rev-parse", "HEAD"]);
    fs::write(workspace.join("value.txt"), "ORPHAN_UNFINISHED").unwrap();
    fs::write(workspace.join("orphan-staged.bin"), [0, 255, 4]).unwrap();
    git(&workspace, &["add", "orphan-staged.bin"]);
    fs::write(
        workspace.join("orphan-notes.txt"),
        "Keep these untracked findings",
    )
    .unwrap();
    let orphan_status = git(&workspace, &["status", "--porcelain"]);
    let orphan_index = git(&workspace, &["write-tree"]);
    run_cycles(root.path(), 0);
    let saved = state(root.path());
    assert_eq!(saved["working_ref"], orphan_head);
    assert_eq!(saved["working_workspace"], workspace.to_str().unwrap());
    assert_eq!(saved["working_branch"], "codex/orphan-work");
    assert_eq!(saved["seed_from_repo"], false);
    assert_eq!(
        fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "ORPHAN_UNFINISHED"
    );
    assert_eq!(
        fs::read(workspace.join("orphan-staged.bin")).unwrap(),
        [0, 255, 4]
    );
    assert_eq!(
        fs::read_to_string(workspace.join("orphan-notes.txt")).unwrap(),
        "Keep these untracked findings"
    );
    assert!(
        !workspace.join("new-document.txt").exists(),
        "Do not overwrite an orphan workspace with the original checkout"
    );
    assert_eq!(git(&workspace, &["status", "--porcelain"]), orphan_status);
    assert_eq!(git(&workspace, &["write-tree"]), orphan_index);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), original.0);
    assert_eq!(git(&repo, &["status", "--porcelain"]), original.1);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn interrupted_initial_import_finishes_before_any_model_request() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let original = dirty_checkout_fixture(root.path(), &server.url);
    let repo = root.path().join("repo");
    let state_dir = root.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let workspace = state_dir.join("working");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "codex/interrupted-import",
            workspace.to_str().unwrap(),
            &original.0,
        ],
    );
    // The initial import copied only part of one file before the process ended.
    fs::write(workspace.join("value.txt"), "PARTIAL_IMPORT").unwrap();
    fs::write(state_dir.join("state.json"), json!({"schema_version":2,"run_id":"interrupted-import","goal":"Improve value incrementally","repo":repo,"cycle":0,"working_ref":original.0,"working_branch":"codex/interrupted-import","working_workspace":workspace,"seed_from_repo":true,"recent":[]}).to_string()).unwrap();
    run_cycles(root.path(), 0);
    assert_imported_dirty_checkout(root.path(), &original);
    assert_eq!(state(root.path())["seed_from_repo"], false);
    assert!(server.requests.lock().unwrap().is_empty());
    fs::write(workspace.join("value.txt"), "LATER_WORKSPACE_CHANGE").unwrap();
    run_cycles(root.path(), 0);
    assert_eq!(
        fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "LATER_WORKSPACE_CHANGE",
        "A completed import is never replayed on resume"
    );
}

#[test]
fn initial_import_preserves_file_to_directory_and_directory_to_file_replacements() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    fs::write(repo.join("becomes-directory"), "Previously a tracked file").unwrap();
    fs::create_dir_all(repo.join("becomes-file/nested")).unwrap();
    fs::write(
        repo.join("becomes-file/nested/old.txt"),
        "Previously a tracked directory",
    )
    .unwrap();
    git(&repo, &["add", "becomes-directory", "becomes-file"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "Artifacts before type replacements",
        ],
    );
    fs::remove_file(repo.join("becomes-directory")).unwrap();
    fs::create_dir_all(repo.join("becomes-directory/subdir")).unwrap();
    fs::write(repo.join("becomes-directory/subdir/new.bin"), [0, 128, 255]).unwrap();
    fs::remove_dir_all(repo.join("becomes-file")).unwrap();
    fs::write(repo.join("becomes-file"), "Now a project document").unwrap();
    let original_status = git(&repo, &["status", "--porcelain"]);
    let original_index = git(&repo, &["write-tree"]);
    run_cycles(root.path(), 0);
    let saved = state(root.path());
    let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
    assert!(workspace.join("becomes-directory").is_dir());
    assert_eq!(
        fs::read(workspace.join("becomes-directory/subdir/new.bin")).unwrap(),
        [0, 128, 255]
    );
    assert!(workspace.join("becomes-file").is_file());
    assert_eq!(
        fs::read_to_string(workspace.join("becomes-file")).unwrap(),
        "Now a project document"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), original_status);
    assert_eq!(git(&repo, &["write-tree"]), original_index);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn dirty_persistent_workspace_survives_a_restart_without_resetting_to_checkpoint() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    run_cycles(root.path(), 1);
    let before = state(root.path());
    let workspace = Path::new(before["working_workspace"].as_str().unwrap());
    fs::write(
        workspace.join("uncommitted.txt"),
        "unfinished work from interrupted process",
    )
    .unwrap();
    fs::remove_file(workspace.join("value.txt")).unwrap();
    git(workspace, &["add", "-A"]);
    let server2 = Server::custom(false, false, None, |_, _| {
        (
            "Continue checking the interrupted work; do not change files.".into(),
            json!([]),
        )
    });
    let config = root.path().join("chuggin.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    settings["ollama_url"] = json!(server2.url);
    fs::write(config, settings.to_string()).unwrap();
    run_cycles(root.path(), 1);
    let after = state(root.path());
    assert_eq!(before["working_workspace"], after["working_workspace"]);
    assert!(!workspace.join("value.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("uncommitted.txt")).unwrap(),
        "unfinished work from interrupted process"
    );
    assert_eq!(git(workspace, &["status", "--porcelain"]), "");
    assert_eq!(
        git(workspace, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "uncommitted.txt"
    );
}

#[test]
fn explicit_checkpoint_restoration_preserves_the_abandoned_work_in_history() {
    let restore_ref = Arc::new(Mutex::new(String::new()));
    let target = restore_ref.clone();
    let mut edited = false;
    let server = Server::custom(false, false, None, move |_, _| {
        if edited {
            edited = false;
            return ("Changes saved for verification.".into(), json!([]));
        }
        edited = true;
        let reference = target.lock().unwrap().clone();
        let tools = if reference.is_empty() {
            json!([task_tool("Improve the value"), {"function":{"name":"write_file","arguments":{"path":"value.txt","content":"USEFUL_WORK"}}}])
        } else {
            json!([{"function":{"name":"write_file","arguments":{"path":"value.txt","content":"ABANDONED_APPROACH"}}},{"function":{"name":"restore_checkpoint","arguments":{"commit":reference,"reason":"The new approach is incorrect; retain the earlier implementation."}}}])
        };
        (String::new(), tools)
    });
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    run_cycles(root.path(), 1);
    *restore_ref.lock().unwrap() = state(root.path())["working_ref"].as_str().unwrap().into();
    run_cycles(root.path(), 1);
    let saved = state(root.path());
    let workspace = Path::new(saved["working_workspace"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "USEFUL_WORK"
    );
    assert_eq!(
        git(workspace, &["show", "HEAD^:value.txt"]),
        "ABANDONED_APPROACH"
    );
    let artifact = root.path().join("state/cycle-000002/restore-0-1.json");
    let record: Value = serde_json::from_slice(&fs::read(artifact).unwrap()).unwrap();
    assert!(record["reason"].as_str().unwrap().contains("incorrect"));
}

#[test]
fn plain_text_project_uses_its_own_validation_without_automatic_language_checks() {
    let server = Server::new(false, false);
    let root = tempfile::tempdir().unwrap();
    let config_path = fixture(root.path(), &server.url, true);
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["goal"] = json!("Update the numeric value in a plain text artifact.");
    config["checks"] =
        json!([{"argv":["sh","-c","test \"$(cat value.txt)\" = 1"],"timeout_seconds":5}]);
    fs::write(&config_path, config.to_string()).unwrap();
    let out = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["recent"][0]["disposition"], "checkpoint");
    assert_eq!(
        fs::read_to_string(
            Path::new(state["working_workspace"].as_str().unwrap()).join("value.txt")
        )
        .unwrap(),
        "1"
    );
    assert!(
        !root
            .path()
            .join("state/cycle-000001/probe-outcome.json")
            .exists()
    );
    let artifact = root.path().join("state/cycle-000001");
    assert!(
        !fs::read_dir(artifact)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("diagnostics-"))
    );
    assert!(
        !Path::new(state["working_workspace"].as_str().unwrap())
            .join("Cargo.toml")
            .exists()
    );
}

#[test]
fn duration_limit_finishes_cycle_and_resets_on_resume() {
    let server = Server::new(false, true);
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), &server.url, true);
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["run_duration_seconds"] = json!(1);
    fs::write(&path, config.to_string()).unwrap();
    for cycle in [1, 2] {
        let output = command(root.path())
            .args(["run", "--forever"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Run duration reached"));
        let state: Value =
            serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap())
                .unwrap();
        assert_eq!(state["cycle"], cycle);
        assert_eq!(
            state["recent"].as_array().unwrap().last().unwrap()["disposition"],
            "checkpoint"
        );
    }
}

#[test]
fn timed_out_followup_retries_without_repeating_edits() {
    let server = Server::serve(false, false, None, true);
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), &server.url, true);
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["request_timeout_seconds"] = json!(1);
    fs::write(&path, config.to_string()).unwrap();
    let output = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["recent"][0]["disposition"], "checkpoint");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1], requests[2],
        "Retry exactly the failed follow-up, preserving completed tool results"
    );
}

#[test]
fn conversation_preserves_long_tool_arguments_beyond_former_byte_limit() {
    let content = format!(
        "{}\nDIFF_END_MARKER\n",
        "Useful project content.\n".repeat(3000)
    );
    let server = Server::serve_with_edit(false, false, None, false, Some(content));
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), &server.url, true);
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["context_tokens"] = json!(65536);
    fs::write(&path, config.to_string()).unwrap();
    let output = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["recent"][0]["disposition"], "checkpoint");
    let requests = server.requests.lock().unwrap();
    // Long tool arguments/results remain in the next implementation request.
    let continuation = &requests[1];
    assert!(continuation["messages"].to_string().len() > 64000);
    assert!(
        continuation["messages"]
            .to_string()
            .contains("DIFF_END_MARKER")
    );
    assert_eq!(continuation["options"]["num_ctx"], 65536);
    let actual = git(
        Path::new(state["working_workspace"].as_str().unwrap()),
        &["diff", "HEAD^", "HEAD", "--no-ext-diff", "--no-textconv"],
    );
    assert!(actual.len() > 64000);
    assert!(actual.ends_with("+DIFF_END_MARKER"));
}

#[test]
fn streaming_repetition_recovers_without_replaying_completed_tools() {
    for stage in [0, 1] {
        let server = Server::serve_with_repetition(false, false, None, false, None, Some(stage));
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, true);
        let output = command(root.path())
            .args(["run", "--cycles", "1"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let state: Value =
            serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap())
                .unwrap();
        assert_eq!(state["recent"][0]["disposition"], "checkpoint");
        assert_eq!(
            fs::read_to_string(
                Path::new(state["working_workspace"].as_str().unwrap()).join("value.txt")
            )
            .unwrap(),
            "1"
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "Completed edits are not replayed");
        let original = &requests[stage];
        let retry = &requests[stage + 1];
        let messages = original["messages"].as_array().unwrap();
        assert_eq!(
            &retry["messages"].as_array().unwrap()[..messages.len()],
            messages
        );
        assert_eq!(retry["format"], original["format"]);
        assert_eq!(retry["tools"], original["tools"]);
        assert!(
            retry["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("Recovery attempt 1")
        );
        assert!(!retry["messages"].to_string().contains("MUST_NOT_EXECUTE"));
        if stage == 1 {
            assert!(messages.iter().any(|m| m["role"] == "tool"));
        }
        if stage == 0 {
            assert_eq!(
                retry["messages"][messages.len()]["content"],
                "The relevant file is value.txt."
            );
        } else {
            assert_eq!(
                retry["messages"].as_array().unwrap().len(),
                messages.len() + 1,
                "Partial structured responses and interrupted tool calls are discarded whole"
            );
        }
        let failure: Value = serde_json::from_slice(
            &fs::read(root.path().join(format!(
                "state/cycle-000001/request-failure-{stage:03}.json"
            )))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(failure["retry_same_stage"], true);
        assert!(failure["error"].as_str().unwrap().contains("Repetitive"));
    }
}

#[test]
fn persistent_repetition_has_bounded_retries_and_records_failure() {
    let server = Server::serve_with_repetition(false, false, None, false, None, Some(usize::MAX));
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    let output = command(root.path())
        .args(["run", "--cycles", "1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(server.requests.lock().unwrap().len(), 9);
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["recent"][0]["disposition"], "checkpoint");
    assert_eq!(
        fs::read_to_string(
            Path::new(state["working_workspace"].as_str().unwrap()).join("value.txt")
        )
        .unwrap(),
        "USER_EDIT",
        "Imported user work remains saved even when every model response fails"
    );
    assert!(state["feedback"].as_str().unwrap().contains("Repetitive"));
    let failure: Value = serde_json::from_slice(
        &fs::read(
            root.path()
                .join("state/cycle-000001/request-failure-002.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(failure["retry_same_stage"], false);
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
        serde_json::from_slice(&fs::read(root.path().join("chuggin.json")).unwrap()).unwrap();
    assert!(config["goal"].as_str().unwrap().contains("Draft 2"));
    assert!(config.get("ollama_url").is_none());
    let settings = root.path().join(".chuggin/global/chuggin/settings.json");
    assert!(settings.exists());
    let calls = server.requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].to_string().contains("Include tables"));
    drop(calls);
    let second_dir = tempfile::tempdir().unwrap();
    let second = second_dir.path().to_path_buf();
    let mut c = command(&second);
    c.env("XDG_CONFIG_HOME", root.path().join(".chuggin/global"));
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
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["cycle"], 1);
    let mut child = command(root.path())
        .args(["run", "--forever"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    while server.requests.lock().unwrap().len() < 3 {
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
        fs::create_dir_all(root.path().join(".chuggin/global/chuggin")).unwrap();
        fs::write(
            root.path().join(".chuggin/global/chuggin/settings.json"),
            json!({"model":"fake","ollama_url":"http://127.0.0.1:1"}).to_string(),
        )
        .unwrap();
        fs::write(
            root.path().join(".chuggin/goal-draft.json"),
            json!({"pitch":"Editor","goal":"Build an editor","feedback":""}).to_string(),
        )
        .unwrap();
        fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.path().join("ignored.txt"), "ignored data").unwrap();
        git(root.path(), &["init"]);
        // Even previously staged runtime files must stay out of the source baseline.
        git(root.path(), &["add", ".chuggin/goal-draft.json"]);
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
            assert!(root.path().join("chuggin.json").exists());
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
            assert!(!root.path().join("chuggin.json").exists());
            assert!(root.path().join(".chuggin/goal-draft.json").exists());
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
    assert_eq!(outcome["disposition"], "checkpoint/unverified", "{outcome}");
    let state: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap()).unwrap();
    assert!(
        Path::new(state["working_workspace"].as_str().unwrap())
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
        assert_eq!(state["recent"][0]["disposition"], "checkpoint");
        // Resume directly after the worker has actually stopped.
        ui.send(b"r");
        ui.wait("cycle 2");
        ui.send(b"\x03");
        ui.wait("Finishing this cycle");
        ui.send(b"r");
        ui.wait("Stop cancelled");
        ui.wait("cycle 3");
        ui.send(b"\x03");
        ui.wait("Run saved");
        let resumed: Value =
            serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap())
                .unwrap();
        assert_eq!(resumed["cycle"], 3);
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
            &fs::read(root.path().join(".chuggin/global/chuggin/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["ollama_url"], "http://localhost:11434");
        assert!(!root.path().join("chuggin.json").exists());
    }

    #[test]
    fn observation_settings_apply_without_interrupting_active_requests() {
        let server = Server::new(false, true);
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), &server.url, true);
        let mut ui = TerminalProcess::start(root.path());
        ui.wait("Resume project");
        ui.send(b"\r");
        ui.wait("Receiving response");
        ui.send(b"5");
        ui.wait("PROJECT SETTINGS");
        ui.send(b"\r\x15fake-next\r");
        ui.send(b"\x1b[B\r\x150\r");
        // Reduce an initially unlimited run to one second from its original start.
        ui.send(b"\x1b[B\r\x150.0003\r");
        ui.wait("Time limit reached");
        ui.wait("Run saved");
        let config: Value =
            serde_json::from_slice(&fs::read(root.path().join("chuggin.json")).unwrap()).unwrap();
        assert_eq!(config["model"], "fake-next");
        assert_eq!(config["request_timeout_seconds"], 0);
        assert_eq!(config["run_duration_seconds"], 1);
        let state: Value =
            serde_json::from_slice(&fs::read(root.path().join("state/state.json")).unwrap())
                .unwrap();
        assert_eq!(state["cycle"], 1);
        assert_eq!(state["recent"][0]["disposition"], "checkpoint");
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "fake");
        assert!(requests[1..].iter().all(|r| r["model"] == "fake-next"));
        drop(requests);
        ui.send(b"q");
        ui.wait("Resume project");
        ui.send(b"q");
        ui.restored();
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
        let global = root.path().join(".chuggin/global/chuggin");
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
        let settings = root.path().join(".chuggin/global/chuggin/settings.json");
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
        ui.send(b"git rev-parse HEAD\r");
        ui.wait("Check timeout (seconds)");
        ui.send(b"\r");
        ui.wait("Resume project");
        ui.send(b"q");
        ui.restored();
        let config: Value =
            serde_json::from_slice(&fs::read(root.path().join("chuggin.json")).unwrap()).unwrap();
        assert_eq!(config["goal"], "Build a useful editor. Draft 1.");
    }
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
    exercise_project_tools(false);
}

#[test]
fn successful_command_does_not_hide_failing_checks_or_discard_work() {
    exercise_project_tools(true);
}

fn exercise_project_tools(failing: bool) {
    let server = Server::serve(false, false, Some(if failing { 4 } else { 3 }), false);
    let root = tempfile::tempdir().unwrap();
    let config = fixture(root.path(), &server.url, true);
    let repo = root.path().join("repo");
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"tools_demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n#[test] fn baseline() { assert!(value() > 0); }\n",
    )
    .unwrap();
    fs::write(repo.join(".gitignore"), "/target\n").unwrap();
    git(&repo, &["add", "Cargo.toml", "src", ".gitignore"]);
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
    let mut settings: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    settings["checks"] = json!([{"argv":[env!("CARGO"),"test","--offline"],"timeout_seconds":30}]);
    fs::write(config, settings.to_string()).unwrap();
    run_cycles(root.path(), 1);
    let saved = state(root.path());
    assert_eq!(
        saved["recent"][0]["disposition"],
        if failing {
            "checkpoint/checks-failing"
        } else {
            "checkpoint"
        }
    );
    assert!(
        fs::read_to_string(
            Path::new(saved["working_workspace"].as_str().unwrap()).join("src/lib.rs")
        )
        .unwrap()
        .contains("{ 2 }")
    );
    assert!(
        fs::read_to_string(repo.join("src/lib.rs"))
            .unwrap()
            .contains("{ 1 }")
    );
    let artifact = root.path().join("state/cycle-000001");
    for name in [
        "command-0-2.log",
        "diagnostics-0-1.log",
        "tool-1-0.json",
        "verification.json",
    ] {
        assert!(artifact.join(name).exists(), "Missing {name}");
    }
    let log: Value =
        serde_json::from_slice(&fs::read(artifact.join("tool-1-0.json")).unwrap()).unwrap();
    assert_eq!(log["ok"], true);
    assert!(log["result"].as_str().unwrap().contains("COMMAND_FINISHED"));
    assert!(
        !artifact.join("probe-outcome.json").exists(),
        "Rust probes should not run automatically"
    );
}

#[test]
fn provider_rate_limit_retries_identical_conversation_without_replaying_edits() {
    let server = Server::custom(false, false, None, |_, n| match n {
        0 => (
            String::new(),
            json!([task_tool("Provider recovery"),{"function":{"name":"write_file","arguments":{"path":"value.txt","content":"KEPT"}}}]),
        ),
        1 => ("__HTTP_LIMIT__".into(), json!([])),
        _ => ("Recovered".into(), json!([])),
    });
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    run_cycles(root.path(), 1);
    let calls = server.requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1], calls[2]);
    let saved = state(root.path());
    assert_eq!(
        git(
            Path::new(saved["working_workspace"].as_str().unwrap()),
            &["show", "HEAD:value.txt"]
        ),
        "KEPT"
    );
    assert!(!root.path().join("state/provider-wait.json").exists());
    assert!(
        root.path()
            .join("state/cycle-000001/provider-error-001.json")
            .exists()
    );
    let session: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/conversation.json")).unwrap())
            .unwrap();
    assert_eq!(session["response_errors"], 0);
}
#[test]
fn provider_credits_pause_once_and_checkpoint_edits() {
    let server = Server::custom(false, false, None, |_, n| {
        if n == 0 {
            (
                String::new(),
                json!([{"function":{"name":"write_file","arguments":{"path":"value.txt","content":"SAVED_BEFORE_QUOTA"}}}]),
            )
        } else {
            ("__CREDIT_LIMIT__".into(), json!([]))
        }
    });
    let root = tempfile::tempdir().unwrap();
    fixture(root.path(), &server.url, true);
    run_cycles(root.path(), 10);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let saved = state(root.path());
    assert_eq!(saved["cycle"], 1);
    assert_eq!(
        git(
            Path::new(saved["working_workspace"].as_str().unwrap()),
            &["show", "HEAD:value.txt"]
        ),
        "SAVED_BEFORE_QUOTA"
    );
    assert!(
        saved["feedback"]
            .as_str()
            .unwrap()
            .contains("No automatic retries")
    );
}
#[test]
fn provider_stream_limit_timer_and_restart_preserve_cooldown() {
    let server = Server::custom(false, false, None, |_, _| {
        ("__STREAM_LIMIT__".into(), json!([]))
    });
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), &server.url, true);
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["run_duration_seconds"] = json!(1);
    fs::write(&path, config.to_string()).unwrap();
    let start = std::time::Instant::now();
    run_cycles(root.path(), 10);
    assert!(start.elapsed() < Duration::from_secs(5));
    let wait = fs::read(root.path().join("state/provider-wait.json")).unwrap();
    run_cycles(root.path(), 10);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(
        wait,
        fs::read(root.path().join("state/provider-wait.json")).unwrap()
    );
    let session: Value =
        serde_json::from_slice(&fs::read(root.path().join("state/conversation.json")).unwrap())
            .unwrap();
    assert_eq!(session["response_errors"], 0);
    assert!(
        !fs::read_dir(root.path().join("state/cycle-000001"))
            .unwrap()
            .flatten()
            .any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("conversation-before-refresh"))
    );
}
#[test]
fn provider_wait_allows_soft_stop_and_model_change() {
    let server = Server::custom(false, false, None, |body, _| {
        if body["model"] == "fake" {
            ("__STREAM_LIMIT__".into(), json!([]))
        } else {
            ("Recovered on selected model".into(), json!([]))
        }
    });
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path(), &server.url, true);
    let mut child = command(root.path())
        .args(["run", "--cycles", "1"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.path().join("state/provider-wait.json").exists() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    while child.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let mut child = command(root.path())
        .args(["run", "--cycles", "1"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["model"] = json!("other");
    let temp = path.with_extension("tmp");
    fs::write(&temp, config.to_string()).unwrap();
    fs::rename(temp, &path).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    assert!(!root.path().join("state/provider-wait.json").exists());
}
