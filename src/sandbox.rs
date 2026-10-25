//! A private Linux computer. Only Podman's CLI and a read-only tool runtime are
//! available to the host controller; no host home, config, or control socket is
//! mounted into the computer. Missing/failed isolation never falls back to host.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Settings {
    #[default]
    Host,
    Podman {
        #[serde(default = "default_memory")]
        memory_mb: u32,
        #[serde(default = "default_cpus")]
        cpus: u32,
        #[serde(default = "default_network")]
        internet: bool,
    },
}
fn default_memory() -> u32 {
    4096
}
fn default_cpus() -> u32 {
    4
}
fn default_network() -> bool {
    true
}
impl Settings {
    pub fn private() -> Self {
        Self::Podman {
            memory_mb: default_memory(),
            cpus: default_cpus(),
            internet: true,
        }
    }
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Podman { .. })
    }
    pub fn label(&self) -> &'static str {
        if self.is_private() {
            "Private Linux computer · Podman"
        } else {
            "This computer · project folder"
        }
    }
    pub fn validate(&self) -> Result<()> {
        if let Self::Podman {
            memory_mb, cpus, ..
        } = self
        {
            ensure!(
                (512..=1_048_576).contains(memory_mb) && (1..=1024).contains(cpus),
                "Choose 512–1,048,576 MiB memory and 1–1024 CPUs"
            );
        }
        Ok(())
    }
}
#[derive(Clone)]
pub(crate) struct Target {
    pub name: String,
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub internet: bool,
}
static TARGETS: LazyLock<Mutex<BTreeMap<PathBuf, Target>>> = LazyLock::new(Mutex::default);
pub fn target(c: &crate::runner::Config) -> Result<Target> {
    let root = fs::canonicalize(&c.repo)?;
    let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    Ok(Target {
        name: format!("chuggin-{}", &hash[..24]),
        state: c.state_dir.clone(),
        runtime: c.state_dir.join("computer/tool-runtime"),
        internet: matches!(c.execution, Settings::Podman { internet: true, .. }),
    })
}
pub fn register(c: &crate::runner::Config) -> Result<()> {
    let root = fs::canonicalize(&c.repo)?;
    let mut all = TARGETS.lock().unwrap();
    if c.execution.is_private() {
        all.insert(root, target(c)?);
    } else {
        all.remove(&root);
    }
    Ok(())
}
pub fn for_root(root: &Path) -> Option<Target> {
    TARGETS.lock().unwrap().get(root).cloned()
}

pub fn install_command(os: &str, release: &str) -> Option<&'static str> {
    let mut fields = BTreeMap::new();
    for line in release.lines() {
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k, v.trim_matches(['\"', '\'']));
        }
    }
    let family = format!(
        "{} {}",
        fields.get("ID").unwrap_or(&""),
        fields.get("ID_LIKE").unwrap_or(&"")
    );
    let command = if os == "linux" {
        if family
            .split_whitespace()
            .any(|s| ["ubuntu", "debian", "linuxmint", "pop"].contains(&s))
        {
            "sudo apt install podman"
        } else if family
            .split_whitespace()
            .any(|s| ["fedora", "rhel", "centos", "rocky", "almalinux"].contains(&s))
        {
            "sudo dnf install podman"
        } else if family
            .split_whitespace()
            .any(|s| ["arch", "manjaro", "endeavouros"].contains(&s))
        {
            "sudo pacman -S podman"
        } else if family
            .split_whitespace()
            .any(|s| ["suse", "opensuse", "opensuse-tumbleweed", "opensuse-leap"].contains(&s))
        {
            "sudo zypper install podman"
        } else if family.split_whitespace().any(|s| s == "alpine") {
            "sudo apk add podman"
        } else {
            return None;
        }
    } else if os == "macos" {
        "brew install podman"
    } else if os == "windows" {
        "winget install RedHat.Podman"
    } else {
        return None;
    };
    Some(command)
}
pub fn install_help(os: &str, release: &str) -> String {
    let command = install_command(os, release)
        .unwrap_or("See https://podman.io/docs/installation for your operating system.");
    format!(
        "Podman is not installed or could not be started. Chuggin has installed nothing.\n\nRun this yourself if you want to use a private computer:\n\n{command}\n\nThen return here and choose Check again. Package installation may also require your distribution's rootless-container prerequisites.\n\nThis first private-desktop backend runs on Linux. Podman on macOS/Windows additionally needs a Linux VM; installing the CLI alone does not enable this backend.\n\nOfficial instructions: https://podman.io/docs/installation"
    )
}
pub fn current_install_command() -> Option<&'static str> {
    install_command(
        std::env::consts::OS,
        &fs::read_to_string("/etc/os-release").unwrap_or_default(),
    )
}
pub fn missing_help() -> String {
    install_help(
        std::env::consts::OS,
        &fs::read_to_string("/etc/os-release").unwrap_or_default(),
    )
}
fn podman() -> Command {
    let mut c = Command::new("podman");
    c.arg("--remote=false");
    c.env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION");
    c
}
pub fn installed() -> bool {
    let mut cmd = podman();
    cmd.arg("--version");
    bounded(cmd, None, Duration::from_secs(5), 4096).is_ok()
}
fn checked(output: std::process::Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "Podman: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}
fn run(args: &[&str]) -> Result<Vec<u8>> {
    let mut command = podman();
    command.args(args);
    bounded(command, None, Duration::from_secs(30), 64 * 1024 * 1024)
}
fn info() -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux"),
        "Private computers currently require Linux"
    );
    let v: Value = serde_json::from_slice(&run(&["info", "--format=json"])?)?;
    ensure!(
        v["host"]["security"]["rootless"] == true,
        "Chuggin requires rootless Podman. Do not run Chuggin with sudo."
    );
    ensure!(
        v["host"]["cgroupVersion"] == "v2",
        "Private computers require cgroup v2 to enforce memory, CPU and process limits"
    );
    Ok(v)
}

pub fn prerequisite_report() -> Result<String> {
    let v = info()?;
    let controllers = v["host"]["cgroupControllers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Ok(format!(
        "Podman {}\n✓ Rootless containers enabled\n✓ Linux cgroup v2 available\nSession manager: {}\nReported controllers: {}\n\nChuggin configures CPU, memory and process limits itself and verifies them inside the new computer. Controllers can become available when a limited container starts. You do not need to configure resource limits manually.\n\nNext: choose Set up private computer. Chuggin will download/build the desktop, create its isolated installation, and check that it works. This does not start the model.",
        v["version"]["Version"].as_str().unwrap_or("detected"),
        v["host"]["cgroupManager"].as_str().unwrap_or("unknown"),
        controllers
    ))
}

pub fn repair_commands(error: &str) -> Option<String> {
    let e = error.to_lowercase();
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    if (e.contains("newuidmap") || e.contains("newgidmap"))
        && install_command("linux", &release) == Some("sudo apt install podman")
    {
        return Some("sudo apt install uidmap".into());
    }
    if e.contains("controller")
        && (e.contains("not available") || e.contains("delegate") || e.contains("not enforce"))
    {
        return Some("sudo mkdir -p /etc/systemd/system/user@.service.d\nprintf '[Service]\\nDelegate=cpu cpuset io memory pids\\n' | sudo tee /etc/systemd/system/user@.service.d/90-chuggin-delegation.conf\nsudo systemctl daemon-reload\n# Save your work, then log out and back in before checking again.".into());
    }
    None
}

pub fn setup_error(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    let next=repair_commands(&detail).map(|cmd|format!("Suggested commands (not run):\n\n{cmd}")).unwrap_or_else(||
        "No automatic host configuration change was made. The exact error is shown above. You can inspect Podman's environment with:\n\npodman --remote=false info --debug\n\nDesktop build errors are recorded in the session's computer/build.log.".into());
    format!(
        "Private computer setup could not finish:\n\n{detail}\n\n{next}\n\nRootless setup reference: https://github.com/containers/podman/blob/main/troubleshooting.md"
    )
}
fn exec_base(t: &Target) -> Command {
    let mut cmd = podman();
    cmd.args([
        "exec",
        "--interactive",
        "--workdir",
        "/root",
        "--env",
        "DISPLAY=:1",
        "--env",
        "HOME=/root",
        "--env",
        "NO_PROXY=localhost,127.0.0.1,::1",
        "--env",
        "no_proxy=localhost,127.0.0.1,::1",
    ]);
    if t.internet {
        for key in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
            cmd.args(["--env", &format!("{key}=http://127.0.0.1:3128")]);
        }
    }
    cmd.arg(&t.name);
    cmd
}
fn runtime_args(t: &Target) -> Result<Vec<String>> {
    let loader = fs::read_to_string(t.runtime.join("loader"))?;
    let mut args = Vec::new();
    if !loader.trim().is_empty() {
        args.extend([
            format!("/opt/session-runtime/{}", loader.trim()),
            "--library-path".into(),
            "/opt/session-runtime".into(),
        ]);
    }
    args.push("/opt/session-runtime/tool-runner".into());
    Ok(args)
}
pub fn command(root: &Path, argv: &[String], id: &str) -> Result<Command> {
    if let Some(t) = for_root(root) {
        let mut cmd = exec_base(&t);
        cmd.args(runtime_args(&t)?)
            .args(["guest-command", "--id", id, "--"])
            .args(argv);
        Ok(cmd)
    } else {
        let mut cmd = Command::new(argv.first().context("Empty command")?);
        cmd.args(&argv[1..]);
        Ok(cmd)
    }
}
pub fn stop_job(t: &Target, id: &str) -> Result<()> {
    let reply = rpc(t, &json!({"name":"stop_job","args":{"id":id}}))?;
    ensure!(
        reply["ok"] == true,
        "Container command cleanup failed: {}",
        reply["error"]
    );
    Ok(())
}
fn rpc(t: &Target, input: &Value) -> Result<Value> {
    let mut cmd = exec_base(t);
    cmd.args(["timeout", "--kill-after=2", "55"])
        .args(runtime_args(t)?)
        .arg("guest");
    let out = bounded(
        cmd,
        Some(serde_json::to_vec(input)?),
        Duration::from_secs(60),
        64 * 1024 * 1024,
    )?;
    serde_json::from_slice(&out).context("Private computer returned an invalid tool response")
}
pub fn bounded(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: u64,
) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(missing_help)?;
    let stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            stdin.write_all(&input)
        } else {
            Ok(())
        }
    });
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = out.take(limit + 1).read_to_end(&mut bytes);
        (result, bytes)
    });
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.take(16_000).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(unix)]
    unsafe {
        // Bound pipe lifetimes even if the CLI left a local descendant behind.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = writer.join();
    let (read, bytes) = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Tool reader stopped"))?;
    read?;
    let errors = errors.join().unwrap_or_default();
    ensure!(
        status.is_some(),
        "Private-computer operation timed out; no host action was attempted"
    );
    ensure!(
        status.unwrap().success(),
        "Private computer: {}",
        String::from_utf8_lossy(&errors)
    );
    ensure!(
        bytes.len() as u64 <= limit,
        "Private-computer response exceeds its byte limit"
    );
    Ok(bytes)
}

pub fn tool(root: &Path, name: &str, args: &Value, art: &Path) -> Option<Result<String>> {
    let t = for_root(root)?;
    if ![
        "read_file",
        "write_file",
        "edit_file",
        "list_files",
        "search",
        "project_map",
        "lookup_symbol",
        "view_image",
        "capture_screenshot",
        "desktop_action",
    ]
    .contains(&name)
    {
        return None;
    }
    Some((|| -> Result<String> {
        if name == "view_image" && args["image_id"].as_str().is_some() {
            return Ok(crate::image_tools::inspect(root, art, args)?.to_string());
        }
        if name == "capture_screenshot" || name == "view_image" {
            let mut cmd = exec_base(&t);
            if name == "capture_screenshot" {
                if args["list_windows"] == true {
                    cmd.args(["timeout", "--kill-after=2", "8", "wmctrl", "-l"]);
                    return Ok(String::from_utf8_lossy(&bounded(
                        cmd,
                        None,
                        Duration::from_secs(10),
                        100_000,
                    )?)
                    .into_owned());
                }
                let window = args["window_id"].as_str().unwrap_or("root");
                ensure!(
                    window == "root" || window.bytes().all(|b| b.is_ascii_hexdigit() || b == b'x'),
                    "Invalid window ID"
                );
                cmd.args([
                    "timeout",
                    "--kill-after=2",
                    "18",
                    "import",
                    "-window",
                    window,
                    "png:-",
                ]);
            } else {
                let path = args["path"].as_str().context("Missing image path")?;
                cmd.args(["timeout", "--kill-after=2", "18", "cat", "--", path]);
            }
            let bytes = bounded(
                cmd,
                None,
                Duration::from_secs(20),
                crate::image_tools::MAX_IMAGE_BYTES,
            )?;
            return Ok(crate::image_tools::import_bytes(
                art,
                &bytes,
                json!({"path":args["path"],"desktop":name=="capture_screenshot"}),
            )?
            .to_string());
        }
        let reply = rpc(&t, &json!({"name":name,"args":args}))?;
        ensure!(
            reply["ok"] == true,
            "{}",
            reply["error"].as_str().unwrap_or("Private tool failed")
        );
        Ok(reply["result"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| reply["result"].to_string()))
    })())
}
pub fn tree(c: &crate::runner::Config) -> Result<String> {
    let t = target(c)?;
    let diff: Value = serde_json::from_slice(&run(&["diff", "--format=json", &t.name])?)?;
    let paths = diff_paths(&diff)?;
    let paths: Vec<_> = paths
        .into_iter()
        .filter(|p| {
            !p.starts_with("/run/")
                && !p.starts_with("/tmp/")
                && !p.starts_with("/opt/session-runtime")
        })
        .collect();
    let cached: Value = fs::read(t.state.join("computer/file-cache.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let reply = rpc(
        &t,
        &json!({"name":"observe","args":{"paths":paths,"cache":cached}}),
    )?;
    ensure!(
        reply["ok"] == true,
        "Cannot observe private computer: {}",
        reply["error"]
    );
    crate::setup::save(
        &t.state.join("computer/file-cache.json"),
        &reply["result"]["cache"],
    )?;
    let files: BTreeMap<String, String> = serde_json::from_value(reply["result"]["files"].clone())?;
    crate::file_state::record(&c.state_dir, files)
}

fn diff_paths(diff: &Value) -> Result<Vec<&str>> {
    if let Some(object) = diff.as_object() {
        let mut paths = Vec::new();
        for (kind, items) in object {
            ensure!(
                ["changed", "added", "deleted"].contains(&kind.as_str()),
                "Unknown Podman change category"
            );
            for path in items.as_array().context("Invalid Podman file changes")? {
                paths.push(path.as_str().context("Invalid Podman change path")?);
            }
        }
        Ok(paths)
    } else if let Some(items) = diff.as_array() {
        items
            .iter()
            .map(|v| v["Path"].as_str().context("Invalid Podman change path"))
            .collect()
    } else {
        anyhow::bail!("Invalid Podman file changes")
    }
}

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"desktop_action","description":"Control this computer's private desktop: move/click/drag the pointer, type text, press a key combination, or scroll. Use capture_screenshot to inspect the result. Does not control the human desktop.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["move","click","drag","type","key","scroll"]},"x":{"type":"integer"},"y":{"type":"integer"},"to_x":{"type":"integer"},"to_y":{"type":"integer"},"button":{"type":"integer"},"text":{"type":"string"},"key":{"type":"string"},"steps":{"type":"integer"}},"required":["action"]}}}),
    ]
}

pub fn customize_tools(tools: &mut Value) {
    let Some(all) = tools.as_array_mut() else {
        return;
    };
    all.retain(|t| {
        ![
            "compiler_diagnostics",
            "run_checks",
            "project_map",
            "lookup_symbol",
        ]
        .contains(&t["function"]["name"].as_str().unwrap_or(""))
    });
    for tool in all.iter_mut() {
        let name = tool["function"]["name"].as_str().unwrap_or("").to_owned();
        if let Some(path) = tool["function"]["parameters"]["properties"].get_mut("path") {
            path["description"] = json!(
                "Path inside this Linux computer. Absolute paths are supported; relative paths start in /root."
            );
        }
        if name == "list_files" || name == "project_map" || name == "lookup_symbol" {
            tool["function"]["parameters"]["properties"]["path"] = json!({"type":"string","description":"Directory inside this computer; default /root."});
        }
        if name == "run_command" {
            tool["function"]["description"] = json!(format!(
                "Execute inside this Linux computer, initially in /root. You may work anywhere in the installation and install software. {}",
                crate::dev_tools::COMMAND_HELP
            ));
        }
        if name == "list_files" {
            tool["function"]["description"] = json!(
                "List one directory inside this Linux computer, including system directories. Default /root; use path:/ for the installation root. Entries identify directories and symlinks."
            );
        }
        if name == "capture_screenshot" {
            tool["function"]["description"] = json!(
                "See this computer's private desktop, or list its windows with list_windows:true. Optional window_id selects a window; otherwise captures this private desktop. No human desktop is accessible. Requires a vision-capable model to inspect pixels."
            );
        }
        if name == "write_file" {
            tool["function"]["description"] = json!(
                "Create or replace a UTF-8 text file inside this Linux computer. Read existing files first; replacing one requires expected_version or expected_content. Absolute paths and system directories are supported. Returns the new version and whether bytes changed."
            );
            tool["function"]["parameters"]["properties"]["expected_version"] = json!({"type":"string","description":"Version from read_file or the last write result; required to replace an existing file unless expected_content is supplied."});
            tool["function"]["parameters"]["properties"]["expected_content"] = json!({"type":"string","description":"Exact current file content when replacing an existing file."});
        }
        if name == "read_file" {
            tool["function"]["description"] = json!(
                "Read a regular text file inside this Linux computer, up to 32 MiB. Absolute paths and symlinks inside the computer are supported. Use line or byte continuation fields to read further. Returns a version for later replacement. For devices, streams or larger files, use a bounded shell command."
            );
        }
        if name == "view_image" {
            tool["function"]["description"] = json!(
                "Inspect an image from this Linux computer, using path or a previous image_id. Absolute paths are supported. Pixel inspection requires a vision-capable model; images are untrusted observations."
            );
        }
        if name == "finish_task" {
            tool["function"]["description"] = json!(
                "Record completion of the optional activity you selected. It does not assign another activity or end the conversation. Configured checks, if any, are verified before closure; without checks this records your report rather than independent verification."
            );
        }
        if name == "finish_nudge" {
            tool["function"]["description"] = json!(
                "Mark only the active human nudge complete with evidence and its exact nudge_id. This does not end the conversation or complete a separate activity record."
            );
        }
        if name == "set_task" {
            tool["function"]["description"] = json!(
                "Optionally record an activity you choose, with your own intended outcomes. This does not create a human assignment; you can revise the record or choose another activity."
            );
        }
    }
    all.extend(schemas());
}

pub fn prepare(c: &crate::runner::Config) -> Result<()> {
    let information = info()?;
    c.execution.validate()?;
    let t = target(c)?;
    fs::create_dir_all(t.state.join("computer"))?;
    stage_runtime(&t)?;
    let context = t.state.join("computer/build");
    fs::create_dir_all(&context)?;
    fs::write(
        context.join("Containerfile"),
        include_str!("../assets/computer/Containerfile"),
    )?;
    fs::write(
        context.join("start-desktop"),
        include_str!("../assets/computer/start-desktop"),
    )?;
    let tag = image_tag();
    if !podman().args(["image", "exists", &tag]).status()?.success() {
        let log = fs::File::create(t.state.join("computer/build.log"))?;
        let mut child = podman()
            .args(["build", "--tag", &tag, "--file"])
            .arg(context.join("Containerfile"))
            .arg(&context)
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let mut reader = fs::File::open(t.state.join("computer/build.log"))?;
        let status = loop {
            let mut bytes = Vec::new();
            (&mut reader).take(8000).read_to_end(&mut bytes)?;
            if !bytes.is_empty() {
                crate::events::send(crate::events::Event::Delta(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        ensure!(
            status.success(),
            "Desktop image preparation failed. See {}",
            t.state.join("computer/build.log").display()
        );
    }
    crate::sandbox_network::start(&t)?;
    let exists = podman()
        .args(["container", "exists", &t.name])
        .status()?
        .success();
    if !exists {
        let Settings::Podman {
            memory_mb,
            cpus,
            internet,
        } = c.execution
        else {
            anyhow::bail!("Private computer is not selected")
        };
        let mount = format!(
            "type=bind,src={},dst=/opt/session-runtime,ro",
            fs::canonicalize(&t.runtime)?.display()
        );
        let proxy = format!(
            "type=bind,src={},dst=/run/public-web,ro",
            crate::sandbox_network::socket(&t)
                .parent()
                .unwrap()
                .display()
        );
        let mut cmd = podman();
        cmd.args([
            "create",
            "--name",
            &t.name,
            "--hostname",
            "computer",
            "--network=none",
            "--security-opt=no-new-privileges",
            "--pids-limit=512",
            "--memory",
            &format!("{memory_mb}m"),
            "--cpus",
            &cpus.to_string(),
            "--shm-size=256m",
            "--mount",
            &mount,
            "--label",
            "io.chuggin.managed=true",
            "--label",
            &format!("io.chuggin.session={}", t.name),
            "--env",
            if internet {
                "COMPUTER_INTERNET=1"
            } else {
                "COMPUTER_INTERNET=0"
            },
        ]);
        cmd.args(["--mount", &proxy]);
        cmd.arg(&tag);
        checked(cmd.output()?)?;
    }
    validate_instance(
        &t,
        &serde_json::from_slice::<Value>(&run(&["inspect", &t.name])?)?[0],
    )?;
    start_checked(c, &t)?;
    crate::setup::save(
        &t.state.join("computer/ready.json"),
        &json!({"name":t.name,"image":tag,"storage":information["store"]["graphRoot"]}),
    )?;
    register(c)
}

fn image_tag() -> String {
    let mut hash = Sha256::new();
    hash.update(include_bytes!("../assets/computer/Containerfile"));
    hash.update(include_bytes!("../assets/computer/start-desktop"));
    let digest = format!("{:x}", hash.finalize());
    format!("localhost/chuggin-computer:desktop-{}", &digest[..16])
}

fn stage_runtime(t: &Target) -> Result<()> {
    fs::create_dir_all(&t.runtime)?;
    let binary = std::env::current_exe()?;
    copy_runtime(&binary, &t.runtime.join("tool-runner"))?;
    let output = Command::new("ldd").arg(&binary).output()?;
    let mut loader = String::new();
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some(path) = line.split_whitespace().find(|part| part.starts_with('/')) else {
            continue;
        };
        let path = Path::new(path);
        let name = path.file_name().context("Invalid runtime library")?;
        copy_runtime(path, &t.runtime.join(name))?;
        if line.trim_start().starts_with('/') {
            loader = name.to_string_lossy().into_owned();
        }
    }
    ensure!(
        output.status.success()
            || text.contains("statically linked")
            || text.contains("not a dynamic executable"),
        "Cannot stage native tool runtime: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    crate::setup::save(
        &t.runtime.join("version.json"),
        &json!({"version":env!("CARGO_PKG_VERSION")}),
    )?;
    let mut staged = tempfile::NamedTempFile::new_in(&t.runtime)?;
    staged.write_all(loader.as_bytes())?;
    staged.persist(t.runtime.join("loader"))?;
    Ok(())
}

fn copy_runtime(from: &Path, to: &Path) -> Result<()> {
    // Rename rather than truncate files that a running private process may map.
    let staged =
        tempfile::NamedTempFile::new_in(to.parent().context("Missing runtime directory")?)?;
    fs::copy(from, staged.path())?;
    staged.persist(to)?;
    Ok(())
}

pub fn ready(c: &crate::runner::Config) -> Result<()> {
    if !c.execution.is_private() {
        return Ok(());
    }
    ensure!(installed(), "{}", missing_help());
    let t = target(c)?;
    ensure!(
        t.state.join("computer/ready.json").exists(),
        "Prepare the private computer in Settings → More project settings → Computer environment before starting"
    );
    info()?;
    let staged: Value = fs::read(t.runtime.join("version.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if staged["version"] != env!("CARGO_PKG_VERSION") {
        stage_runtime(&t)?;
    }
    validate_instance(
        &t,
        &serde_json::from_slice::<Value>(&run(&["inspect", &t.name])?)?[0],
    )?;
    crate::sandbox_network::start(&t)?;
    start_checked(c, &t)?;
    register(c)
}

fn start_checked(c: &crate::runner::Config, t: &Target) -> Result<()> {
    run(&["start", &t.name])?;
    let result = (|| -> Result<()> {
        if let Settings::Podman {
            memory_mb, cpus, ..
        } = c.execution
        {
            run(&[
                "update",
                "--memory",
                &format!("{memory_mb}m"),
                "--cpus",
                &cpus.to_string(),
                &t.name,
            ])?;
        }
        let ready = rpc(t, &json!({"name":"ping","args":{}}))?;
        ensure!(ready["ok"] == true, "Private tool runtime could not start");
        // A successful create/update alone does not prove the rootless cgroup
        // controller actually applied resource limits on this installation.
        let limits = rpc(t, &json!({"name":"resource_limits","args":{}}))?;
        ensure!(
            limits["ok"] == true,
            "Cannot verify private computer resource limits"
        );
        if let Settings::Podman {
            memory_mb, cpus, ..
        } = c.execution
        {
            let v = &limits["result"];
            let memory = v["memory"]
                .as_str()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let pids = v["pids"]
                .as_str()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let cpu: Vec<_> = v["cpu"].as_str().unwrap_or("").split_whitespace().collect();
            let ratio = cpu
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .zip(cpu.get(1).and_then(|s| s.parse::<f64>().ok()));
            ensure!(
                memory.is_some_and(|m| m > 0 && m <= memory_mb as u64 * 1024 * 1024)
                    && pids.is_some_and(|p| p > 0 && p <= 512)
                    && ratio.is_some_and(|(quota, period)| period > 0.0
                        && quota > 0.0
                        && quota / period <= cpus as f64 + 0.01),
                "Podman did not enforce the configured memory, CPU and process limits; refusing model tools"
            );
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = run(&["stop", "--time=2", &t.name]);
    }
    result
}

fn validate_instance(t: &Target, instance: &Value) -> Result<()> {
    ensure!(
        instance["Config"]["Labels"]["io.chuggin.session"] == t.name,
        "This container is not owned by this session"
    );
    ensure!(
        instance["HostConfig"]["Privileged"] == false
            && instance["HostConfig"]["NetworkMode"] == "none"
            && instance["HostConfig"]["SecurityOpt"]
                .as_array()
                .is_some_and(|opts| opts.iter().any(|o| matches!(
                    o.as_str(),
                    Some("no-new-privileges" | "no-new-privileges=true")
                ))),
        "Unsafe private-computer configuration; refuse to start"
    );
    ensure!(
        instance["HostConfig"]["PidMode"] != "host"
            && instance["HostConfig"]["IpcMode"] != "host"
            && instance["HostConfig"]["UsernsMode"] != "host",
        "Host namespaces are unavailable to private computers"
    );
    ensure!(
        instance["HostConfig"]["CapAdd"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "Extra capabilities are unavailable to private computers"
    );
    ensure!(
        instance["HostConfig"]["Devices"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "Host devices are not enabled in this private computer backend"
    );
    ensure!(
        instance["Mounts"].as_array().is_some_and(|m| m.len() == 2),
        "Private computer must have exactly two read-only runtime mounts"
    );
    let mut destinations = std::collections::BTreeSet::new();
    for mount in instance["Mounts"]
        .as_array()
        .context("Missing container mounts")?
    {
        let destination = mount["Destination"].as_str().unwrap_or("");
        ensure!(destinations.insert(destination), "Duplicate host mount");
        let expected = match destination {
            "/opt/session-runtime" => t.runtime.clone(),
            "/run/public-web" => crate::sandbox_network::socket(t)
                .parent()
                .unwrap()
                .to_path_buf(),
            _ => anyhow::bail!("Unexpected host mount"),
        };
        let expected = fs::canonicalize(expected)?;
        ensure!(
            mount["Source"]
                .as_str()
                .is_some_and(|p| Path::new(p) == expected),
            "Unexpected host source for private computer mount"
        );
        ensure!(
            ["/opt/session-runtime", "/run/public-web"].contains(&destination)
                && mount["RW"] == false,
            "Unexpected host mount in private computer: {destination}"
        );
    }

    Ok(())
}

pub fn stop(c: &crate::runner::Config) -> Result<()> {
    ensure!(c.execution.is_private(), "No private computer selected");
    run(&["stop", "--time=2", &target(c)?.name])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_instructions_match_the_users_os_without_installing_anything() {
        for (os, release, command) in [
            (
                "linux",
                "ID=linuxmint\nID_LIKE=\"ubuntu debian\"",
                "sudo apt install podman",
            ),
            ("linux", "ID=fedora", "sudo dnf install podman"),
            ("linux", "ID=arch", "sudo pacman -S podman"),
            (
                "linux",
                "ID=opensuse-tumbleweed",
                "sudo zypper install podman",
            ),
            ("linux", "ID=alpine", "sudo apk add podman"),
            ("macos", "", "brew install podman"),
            ("windows", "", "winget install RedHat.Podman"),
        ] {
            let text = install_help(os, release);
            assert!(text.contains(command));
            assert!(text.contains("Chuggin has installed nothing"));
        }
    }
    #[test]
    fn podman_changes_support_the_documented_category_object() {
        assert_eq!(
            diff_paths(
                &json!({"added":["/root/one"],"changed":["/etc/two"],"deleted":["/root/three"]})
            )
            .unwrap(),
            vec!["/root/one", "/etc/two", "/root/three"]
        );
        assert_eq!(
            diff_paths(&json!([{"Path":"/root/one","Kind":1}])).unwrap(),
            vec!["/root/one"]
        );
        assert!(diff_paths(&json!({"added":[null]})).is_err());
        assert!(diff_paths(&json!({"surprise":[]})).is_err());
    }
    #[test]
    fn changed_container_policy_is_rejected_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let t = Target {
            name: "test-session".into(),
            state: dir.path().into(),
            runtime: dir.path().join("runtime"),
            internet: true,
        };
        fs::create_dir_all(&t.runtime).unwrap();
        fs::create_dir_all(socket_dir(&t)).unwrap();
        let v = json!({"Config":{"Labels":{"io.chuggin.session":t.name}},"HostConfig":{
            "NetworkMode":"none","Privileged":false,"SecurityOpt":["no-new-privileges"],"CapAdd":[],"Devices":[]},
            "Mounts":[{"Source":t.runtime,"Destination":"/opt/session-runtime","RW":false},
                {"Source":socket_dir(&t),"Destination":"/run/public-web","RW":false}]});
        validate_instance(&t, &v).unwrap();
        for pointer in ["/HostConfig/Privileged", "/Mounts/0/RW"] {
            let mut bad = v.clone();
            *bad.pointer_mut(pointer).unwrap() = json!(true);
            assert!(validate_instance(&t, &bad).is_err());
        }
        for (field, value) in [
            ("NetworkMode", json!("host")),
            ("PidMode", json!("host")),
            ("IpcMode", json!("host")),
            ("UsernsMode", json!("host")),
            ("SecurityOpt", json!([])),
            ("Devices", json!([{}])),
            ("CapAdd", json!(["SYS_ADMIN"])),
        ] {
            let mut bad = v.clone();
            bad["HostConfig"][field] = value;
            assert!(validate_instance(&t, &bad).is_err(), "{field}");
        }
        let mut bad = v.clone();
        bad["Mounts"][0]["Source"] = json!("/home");
        assert!(validate_instance(&t, &bad).is_err());
        let mut bad = v.clone();
        bad["Mounts"][1]["Destination"] = json!("/opt/session-runtime");
        assert!(validate_instance(&t, &bad).is_err());
    }
    fn socket_dir(t: &Target) -> PathBuf {
        crate::sandbox_network::socket(t).parent().unwrap().into()
    }
    #[test]
    fn bounded_child_reaps_descendants_and_caps_output() {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30 & printf done"]);
        let start = Instant::now();
        assert_eq!(
            bounded(c, None, Duration::from_secs(1), 100).unwrap(),
            b"done"
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        let mut c = Command::new("sh");
        c.args(["-c", "printf abcdef"]);
        assert!(bounded(c, None, Duration::from_secs(1), 3).is_err());
    }
}
