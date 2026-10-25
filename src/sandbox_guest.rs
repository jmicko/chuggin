//! Internal tool worker. Only launched inside the private computer. Host code
//! never enables the absolute-path/symlink semantics used by this worker.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
};
thread_local! { static GUEST: Cell<bool> = const {Cell::new(false)}; }
pub fn active() -> bool {
    GUEST.get()
}
fn enter() -> Result<()> {
    ensure!(
        Path::new("/opt/session-runtime/tool-runner").exists()
            && fs::read_to_string("/proc/self/uid_map")
                .ok()
                .is_some_and(|m| m.lines().any(|line| {
                    let p: Vec<_> = line.split_whitespace().collect();
                    p.len() == 3 && p[0] == "0" && p[1] != "0"
                })),
        "This internal command must run inside the private computer"
    );
    GUEST.set(true);
    Ok(())
}
fn job_path(id: &str) -> Result<PathBuf> {
    ensure!(
        !id.is_empty()
            && id.len() < 200
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid command ID"
    );
    let dir = Path::new("/run/session-jobs");
    fs::create_dir_all(dir)?;
    Ok(dir.join(id))
}
pub fn command(id: &str, argv: &[String]) -> Result<()> {
    enter()?;
    ensure!(!argv.is_empty(), "Empty command");
    let path = job_path(id)?;
    enable_command_reaper()?;
    let mut cmd = Command::new(&argv[0]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .args(&argv[1..])
        .current_dir("/root")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    fs::write(&path, child.id().to_string())?;
    let status = wait_for_command(&mut child);
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(target_os = "linux")]
    {
        let remaining = finish_command_children(std::time::Duration::from_secs(1));
        if remaining > 0 {
            eprintln!(
                "Command cleanup: {remaining} descendant processes outlived the cleanup grace period. No additional process groups were stopped."
            );
        }
    }
    let _ = fs::remove_file(path);
    let status = status?;
    std::process::exit(status.code().unwrap_or(1));
}

fn enable_command_reaper() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // This worker owns one command. Adopt orphaned grandchildren here so
        // older computers with an Xfce PID 1 do not accumulate their zombies.
        let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        ensure!(
            result == 0,
            "Could not enable private-command child cleanup: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn wait_for_command(child: &mut Child) -> std::io::Result<ExitStatus> {
    #[cfg(target_os = "linux")]
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        // Reap adopted children while a GUI is open, without consuming the
        // managed child's status before std::process::Child can observe it.
        reap_finished_children(Some(child.id() as libc::pid_t));
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    #[cfg(not(target_os = "linux"))]
    child.wait()
}

#[cfg(target_os = "linux")]
fn command_children(exclude: Option<libc::pid_t>) -> Vec<libc::pid_t> {
    let mut children = Vec::new();
    if let Ok(tasks) = fs::read_dir("/proc/self/task") {
        for task in tasks.filter_map(std::result::Result::ok) {
            if let Ok(pids) = fs::read_to_string(task.path().join("children")) {
                children.extend(
                    pids.split_whitespace()
                        .filter_map(|pid| pid.parse::<libc::pid_t>().ok())
                        .filter(|pid| *pid > 1 && Some(*pid) != exclude),
                );
            }
        }
    }
    children.sort_unstable();
    children.dedup();
    children
}

#[cfg(target_os = "linux")]
fn reap_finished_children(exclude: Option<libc::pid_t>) {
    for pid in command_children(exclude) {
        loop {
            let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if result >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn finish_command_children(grace: std::time::Duration) -> usize {
    let deadline = std::time::Instant::now() + grace;
    loop {
        reap_finished_children(None);
        let remaining = command_children(None).len();
        if remaining == 0 || std::time::Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
pub fn serve() -> Result<()> {
    enter()?;
    let input: Value = serde_json::from_reader(std::io::stdin().lock().take(64 * 1024 * 1024))?;
    let name = input["name"].as_str().context("Missing tool name")?;
    let result = call(name, &input["args"]);
    let reply = match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(e) => json!({"ok":false,"error":format!("{e:#}")}),
    };
    serde_json::to_writer(std::io::stdout().lock(), &reply)?;
    Ok(())
}
fn path(args: &Value) -> Result<PathBuf> {
    crate::project::safe_path(
        Path::new("/root"),
        args["path"].as_str().context("path must be a separate string argument naming a file inside this computer. No file was changed.")?,
    )
}
fn call(name: &str, args: &Value) -> Result<Value> {
    if crate::computer_tools::is_tool(name) {
        return crate::computer_tools::call(name, args);
    }
    match name {
        "ping" => Ok(json!({"protocol":1})),
        "resource_limits" => Ok(json!({
            "memory":fs::read_to_string("/sys/fs/cgroup/memory.max")?,
            "cpu":fs::read_to_string("/sys/fs/cgroup/cpu.max")?,
            "pids":fs::read_to_string("/sys/fs/cgroup/pids.max")?
        })),
        "stop_job" => {
            let path = job_path(args["id"].as_str().context("Missing command ID")?)?;
            if let Ok(pid) = fs::read_to_string(&path) {
                let pid: i32 = pid.trim().parse()?;
                ensure!(pid > 1, "Invalid process group");
                #[cfg(unix)]
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = fs::remove_file(path);
            }
            Ok(json!({"stopped":true}))
        }
        "write_file" => {
            let p = path(args)?;
            let previous = if p.exists() {
                Some(fs::read_to_string(&p)
                .context("Existing file is not readable UTF-8 text; use a shell command for binary files")?)
            } else {
                None
            };
            if let Some(expected) = args.get("expected_content").and_then(Value::as_str) {
                ensure!(
                    previous.as_deref() == Some(expected),
                    "File changed; read it before replacing it"
                );
            }
            if previous.is_some() {
                let expected = args["expected_version"].as_str();
                let exact = args["expected_content"].as_str();
                ensure!(
                    exact.is_some() || expected.is_some(),
                    "Read the existing file and supply expected_version before replacing it"
                );
                if let Some(expected) = expected {
                    ensure!(
                        expected == crate::operator::config_revision(&p)?,
                        "File changed; read its current version before replacing it"
                    );
                }
            }
            let content = args["content"].as_str().context("Missing content")?;
            let changed = crate::project::write(
                Path::new("/root"),
                p.to_str().context("Non-UTF8 path")?,
                content,
            )?;
            Ok(json!({"changed":changed,"path":p,"version":crate::operator::config_revision(&p)?}))
        }
        "edit_file" => {
            let p = path(args)?;
            let current = fs::read_to_string(&p)?;
            if let Some(expected) = args["expected_version"].as_str() {
                ensure!(
                    expected == crate::operator::config_revision(&p)?,
                    "File changed; read its current version"
                );
            }
            let old = args["old_text"].as_str().context("Missing old text")?;
            let new = args["new_text"].as_str().context("Missing new text")?;
            ensure!(
                !old.is_empty() && current.matches(old).count() == 1,
                "Old text must occur exactly once"
            );
            let changed = crate::project::write(
                Path::new("/root"),
                p.to_str().context("Non-UTF8 path")?,
                &current.replacen(old, new, 1),
            )?;
            Ok(json!({"changed":changed,"path":p,"version":crate::operator::config_revision(&p)?}))
        }
        "list_files" => {
            let p = crate::project::safe_path(
                Path::new("/root"),
                args["path"].as_str().unwrap_or("/root"),
            )?;
            let mut entries = Vec::new();
            for item in fs::read_dir(&p)?.take(1001) {
                let item = item?;
                entries.push(json!({"path":item.path(),"directory":item.file_type()?.is_dir(),"symlink":item.file_type()?.is_symlink()}));
            }
            let truncated = entries.len() > 1000;
            entries.truncate(1000);
            Ok(
                json!({"path":p,"entries":entries,"truncated":truncated,"instruction":"Directory listing only. Select a subdirectory or use a shell command for recursive discovery."}),
            )
        }
        "desktop_action" => desktop(args),
        "observe" => {
            let mut result = BTreeMap::new();
            let mut cache = BTreeMap::new();
            for p in args["paths"].as_array().context("Missing changed paths")? {
                let p = p.as_str().context("Invalid file path")?;
                if p.starts_with("/proc/")
                    || p.starts_with("/sys/")
                    || p.starts_with("/dev/")
                    || p.starts_with("/run/")
                    || p.starts_with("/tmp/")
                {
                    continue;
                }
                match fs::symlink_metadata(p) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        result.insert(
                            p.to_owned(),
                            format!("link:{}", fs::read_link(p)?.display()),
                        );
                    }
                    Ok(meta) if meta.is_file() => {
                        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
                        let stamp = json!([
                            meta.dev(),
                            meta.ino(),
                            meta.len(),
                            meta.mtime(),
                            meta.mtime_nsec(),
                            meta.ctime(),
                            meta.ctime_nsec(),
                            meta.mode()
                        ]);
                        if args["cache"][p]["stamp"] == stamp
                            && let Some(hash) = args["cache"][p]["hash"].as_str()
                        {
                            result.insert(p.to_owned(), hash.to_owned());
                            cache.insert(p.to_owned(), json!({"stamp":stamp,"hash":hash}));
                            continue;
                        }
                        let mut hash = Sha256::new();
                        hash.update(meta.mode().to_le_bytes());
                        let mut f = fs::OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                            .open(p)?;
                        ensure!(f.metadata()?.is_file(), "Changed file is no longer regular");
                        let mut buffer = [0u8; 64 * 1024];
                        loop {
                            let n = f.read(&mut buffer)?;
                            if n == 0 {
                                break;
                            }
                            hash.update(&buffer[..n]);
                        }
                        let hash = format!("{:x}", hash.finalize());
                        result.insert(p.to_owned(), hash.clone());
                        cache.insert(p.to_owned(), json!({"stamp":stamp,"hash":hash}));
                    }
                    Ok(meta) => {
                        result.insert(p.to_owned(), format!("directory:{}", meta.len()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        result.insert(p.to_owned(), "deleted".into());
                    }
                    Err(e) => {
                        result.insert(p.to_owned(), format!("unreadable:{:?}", e.kind()));
                    }
                }
            }
            Ok(json!({"files":result,"cache":cache}))
        }
        "read_file" => {
            let p = path(args)?;
            let meta = fs::metadata(&p)?;
            ensure!(
                meta.is_file() && meta.len() <= 32 * 1024 * 1024,
                "Read a regular file up to 32 MiB with this tool; use a bounded shell command for devices, streams or larger files"
            );
            let mut value: Value = if args.get("byte_offset").is_some() {
                crate::project::read_bytes(
                    Path::new("/root"),
                    p.to_str().unwrap(),
                    args["byte_offset"].as_u64().unwrap_or(0),
                )?
            } else {
                json!({"text":crate::project::read_lines(
                    Path::new("/root"),
                    p.to_str().unwrap(),
                    args["start_line"].as_u64().unwrap_or(1) as usize,
                    args["line_count"].as_u64().unwrap_or(80) as usize,
                )?})
            };
            value["version"] = json!(crate::operator::config_revision(&p)?);
            Ok(value)
        }
        "search" | "project_map" | "lookup_symbol" => {
            let scope = crate::project::safe_path(
                Path::new("/root"),
                args["path"].as_str().unwrap_or("/root"),
            )?;
            let mut a = args.clone();
            if let Some(a) = a.as_object_mut() {
                a.remove("path");
            }
            let mut research = crate::web_tools::Research::default();
            let result = crate::runner::inspect_tool(&scope, name, &a, &mut research)?;
            Ok(serde_json::from_str(&result).unwrap_or(json!(result)))
        }
        _ => anyhow::bail!("Unknown private-computer tool: {name}"),
    }
}
fn desktop(args: &Value) -> Result<Value> {
    let coordinate = |key: &str| -> Result<String> {
        let n = args[key].as_u64().context(format!("Missing {key}"))?;
        ensure!(n < 8192, "Coordinate exceeds desktop bounds");
        Ok(n.to_string())
    };
    let mut cmd = Command::new("xdotool");
    cmd.env("DISPLAY", ":1");
    match args["action"].as_str().unwrap_or("") {
        "move" => {
            cmd.args(["mousemove", &coordinate("x")?, &coordinate("y")?]);
        }
        "click" => {
            if args.get("x").is_some() {
                cmd.args(["mousemove", &coordinate("x")?, &coordinate("y")?]);
            }
            let b = args["button"].as_u64().unwrap_or(1);
            ensure!((1..=3).contains(&b), "Mouse button must be 1–3");
            cmd.args(["click", &b.to_string()]);
        }
        "drag" => {
            cmd.args([
                "mousemove",
                &coordinate("x")?,
                &coordinate("y")?,
                "mousedown",
                "1",
                "mousemove",
                "--sync",
                &coordinate("to_x")?,
                &coordinate("to_y")?,
                "mouseup",
                "1",
            ]);
        }
        "type" => {
            let text = args["text"].as_str().context("Missing text")?;
            ensure!(text.len() <= 16000, "Text exceeds 16000 bytes");
            cmd.args(["type", "--clearmodifiers", "--", text]);
        }
        "key" => {
            let key = args["key"].as_str().context("Missing key")?;
            ensure!(
                key.len() <= 100
                    && !key.starts_with('-')
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+_-".contains(&b)),
                "Invalid key combination"
            );
            cmd.args(["key", "--clearmodifiers", key]);
        }
        "scroll" => {
            let n = args["steps"].as_i64().unwrap_or(1);
            ensure!(
                n != 0 && (-20..=20).contains(&n),
                "Scroll must be -20–20 and nonzero"
            );
            cmd.args([
                "click",
                "--repeat",
                &n.abs().to_string(),
                if n > 0 { "5" } else { "4" },
            ]);
        }
        _ => anyhow::bail!("Unknown desktop action"),
    }
    let output = cmd.output()?;
    ensure!(
        output.status.success(),
        "Desktop: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(json!({"performed":true,"instruction":"Capture the private desktop to inspect the result."}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn private_command_cleanup_reaps_nested_children_without_waiting_on_escaped_work() {
        // Subreaper is process-wide: isolate this test from the shared test
        // runner and its other subprocesses.
        let mut probe = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox_guest::tests::private_command_reaper_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("CHUGGIN_PRIVATE_REAPER_PROBE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if probe.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = probe.kill();
                let _ = probe.wait();
                panic!("Private child cleanup exceeded its test deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let result = probe.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "Subprocess helper for private-command cleanup; never set subreaper on the main test runner"]
    fn private_command_reaper_probe() {
        use std::{
            os::unix::process::CommandExt,
            time::{Duration, Instant},
        };
        assert_eq!(
            std::env::var("CHUGGIN_PRIVATE_REAPER_PROBE").as_deref(),
            Ok("1")
        );
        enable_command_reaper().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut managed = Command::new("sh");
        managed
            .args([
                "-c",
                r#"
            sh -c 'sleep 2 & printf "%s\n" "$!" > "$PROBE_DIR/grandchild"; wait' &
            printf '%s\n' "$!" > "$PROBE_DIR/intermediate"
            while [ ! -s "$PROBE_DIR/grandchild" ]; do sleep 0.01; done
            exit 7
        "#,
            ])
            .env("PROBE_DIR", dir.path())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = managed.spawn().unwrap();
        let group = child.id() as libc::pid_t;
        let status = wait_for_command(&mut child).unwrap();
        assert_eq!(
            status.code(),
            Some(7),
            "The main command's status must not be consumed by orphan reaping"
        );
        let intermediate: libc::pid_t = fs::read_to_string(dir.path().join("intermediate"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            command_children(None).contains(&intermediate),
            "The orphaned intermediate must be adopted by this command worker"
        );
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        assert_eq!(finish_command_children(Duration::from_secs(1)), 0);
        assert_eq!(
            unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );

        let mut escaped = Command::new("sleep");
        escaped
            .arg("2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            escaped.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut escaped = escaped.spawn().unwrap();
        let started = Instant::now();
        assert_eq!(finish_command_children(Duration::from_millis(50)), 1);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            escaped.try_wait().unwrap().is_none(),
            "Cleanup must leave a live process outside the stopped command group alone"
        );
        escaped.kill().unwrap();
        escaped.wait().unwrap();
        assert_eq!(finish_command_children(Duration::from_secs(1)), 0);
    }
    #[test]
    fn guest_files_use_absolute_paths_and_require_current_replacement_evidence() {
        std::thread::spawn(|| {
            let dir=tempfile::tempdir().unwrap();
            let p=dir.path().join("note.txt");
            assert!(crate::project::safe_path(Path::new("/root"), p.to_str().unwrap()).is_err());
            GUEST.set(true);
            let written=call("write_file",&json!({"path":p,"content":"first"})).unwrap();
            assert_eq!(written["changed"],true);
            let read=call("read_file",&json!({"path":p})).unwrap();
            assert!(read["text"].as_str().unwrap().contains("first"));
            assert_eq!(read["version"],written["version"]);
            assert!(call("write_file",&json!({"path":p,"content":"second"})).is_err());
            let edited=call("edit_file",&json!({"path":p,"old_text":"first","new_text":"second","expected_version":read["version"]})).unwrap();
            assert_ne!(edited["version"],read["version"]);
            assert!(call("write_file",&json!({"path":p,"content":"lost edit","expected_version":read["version"]})).is_err());
            assert_eq!(fs::read_to_string(&p).unwrap(),"second");
            #[cfg(unix)] {
                let link=dir.path().join("link");
                std::os::unix::fs::symlink(&p,&link).unwrap();
                assert!(call("read_file",&json!({"path":link})).unwrap()["text"].as_str().unwrap().contains("second"));
            }
            let binary=dir.path().join("binary"); fs::write(&binary,[0xff]).unwrap();
            assert!(call("write_file",&json!({"path":binary,"content":"replacement"})).is_err());
            assert!(call("read_file",&json!({"path":"/dev/zero"})).is_err());
            assert!(desktop(&json!({"action":"scroll","steps":i64::MIN})).is_err());
            GUEST.set(false);
        }).join().unwrap();
        assert!(
            !active(),
            "The host thread must never inherit guest path rules"
        );
    }
    #[test]
    fn observations_hash_changes_reuse_only_matching_metadata_and_keep_deletions() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap().join("target")).unwrap();
        let p = dir.path().join("file");
        fs::write(&p, "first").unwrap();
        let initial = call("observe", &json!({"paths":[p]})).unwrap();
        let same = call("observe", &json!({"paths":[p],"cache":initial["cache"]})).unwrap();
        assert_eq!(initial, same);
        fs::write(&p, "other").unwrap();
        let changed = call("observe", &json!({"paths":[p],"cache":same["cache"]})).unwrap();
        assert_ne!(initial["files"], changed["files"]);
        fs::remove_file(&p).unwrap();
        assert_eq!(
            call("observe", &json!({"paths":[p]})).unwrap()["files"][p.to_str().unwrap()],
            "deleted"
        );
    }
}
