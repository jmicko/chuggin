#![cfg(unix)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_chuggin");

struct Fixture {
    root: tempfile::TempDir,
    project: PathBuf,
    bin: PathBuf,
    first: String,
    second: String,
    first_id: String,
    second_id: String,
    removed: PathBuf,
    calls: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("fresh-project");
        let bin = root.path().join("bin");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&bin).unwrap();
        let old_project = |label: &str| root.path().join(format!("deleted-{label}"));
        let computer_name = |path: &Path| {
            let hash = format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()));
            format!("chuggin-{}", &hash[..24])
        };
        let first = computer_name(&old_project("alpha"));
        let second = computer_name(&old_project("beta"));
        let first_id = "a".repeat(64);
        let second_id = "b".repeat(64);
        let mut instances = Vec::new();
        for (name, container_id, label) in
            [(&first, &first_id, "alpha"), (&second, &second_id, "beta")]
        {
            let state = old_project(label).join(".chuggin");
            let instance = json!([{
                "Id":container_id, "Name":name,
                "State":{"Running":false,"Status":"exited"},
                "Config":{"Labels":{"io.chuggin.managed":"true","io.chuggin.session":name}},
                "HostConfig":{"Privileged":false,"NetworkMode":"none","SecurityOpt":["no-new-privileges"]},
                "Mounts":[
                    {"Source":state.join("computer/tool-runtime"),"Destination":"/opt/session-runtime","RW":false},
                    {"Source":state.join("computer/channel"),"Destination":"/run/public-web","RW":false}
                ]
            }]);
            fs::write(
                root.path().join(format!("{label}.json")),
                instance.to_string(),
            )
            .unwrap();
            instances.push(instance[0].clone());
        }
        fs::write(
            root.path().join("inspect-both.json"),
            json!(instances).to_string(),
        )
        .unwrap();
        let entry =
            |name: &str| json!({"Id":name,"ID":name,"Names":[name],"Name":name,"State":"exited"});
        fs::write(
            root.path().join("both.json"),
            json!([entry(&first), entry(&second)]).to_string(),
        )
        .unwrap();
        fs::write(
            root.path().join("remaining.json"),
            json!([entry(&second)]).to_string(),
        )
        .unwrap();
        let removed = root.path().join("alpha-removed");
        let calls = root.path().join("podman-calls.log");
        fs::write(root.path().join("info.json"), json!({"host":{"security":{"rootless":true},"cgroupVersion":"v2"},"store":{"graphRoot":root.path().join("podman-storage")}}).to_string()).unwrap();
        let quote = |path: &Path| shell_words::quote(path.to_str().unwrap()).into_owned();
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {calls}
test "$1" = --remote=false || exit 81
shift
case "$1" in
  --version) echo 'podman version TEST';;
  info) cat {info};;
  ps)
    if test -e {removed}; then cat {remaining}; else cat {both}; fi;;
  inspect)
    if test "$#" -gt 2; then cat {inspect_both}; exit 0; fi
    case "$2" in
      {first}) cat {alpha};;
      {second}) cat {beta};;
      *) exit 82;;
    esac;;
  rm)
    test "$2" = --force || exit 83
    test "$3" = {first_id} || exit 84
    test "$#" = 3 || exit 85
    : > {removed};;
  *) echo 'Unexpected mock operation' >&2; exit 86;;
esac
"#,
            calls = quote(&calls),
            removed = quote(&removed),
            remaining = quote(&root.path().join("remaining.json")),
            both = quote(&root.path().join("both.json")),
            first = first,
            second = second,
            first_id = first_id,
            alpha = quote(&root.path().join("alpha.json")),
            beta = quote(&root.path().join("beta.json")),
            info = quote(&root.path().join("info.json")),
            inspect_both = quote(&root.path().join("inspect-both.json"))
        );
        fs::write(bin.join("podman"), script).unwrap();
        fs::set_permissions(bin.join("podman"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            project,
            bin,
            first,
            second,
            first_id,
            second_id,
            removed,
            calls,
        }
    }

    fn ui(&self) -> Ui {
        Ui::start(&self.project, &self.root.path().join("config"), &self.bin)
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

struct Ui {
    master: fs::File,
    _slave: fs::File,
    child: Child,
    parser: vt100::Parser,
}
impl Ui {
    fn start(project: &Path, config: &Path, bin: &Path) -> Self {
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
        let mut command = Command::new(BIN);
        command
            .current_dir(project)
            .env("XDG_CONFIG_HOME", config)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Self {
            master,
            _slave: slave,
            child,
            parser: vt100::Parser::new(36, 110, 0),
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
        self.wait_for(text, Duration::from_secs(12));
    }

    fn wait_for(&mut self, text: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.read();
            if self.parser.screen().contents().contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Missing {text:?}:\n{}",
                self.parser.screen().contents()
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn choose(&mut self, label: &str) {
        self.wait(label);
        for _ in 0..30 {
            self.read();
            if self.parser.screen().contents().lines().any(|line| {
                line.split_once('›')
                    .is_some_and(|(_, item)| item.trim_start().contains(label))
            }) {
                self.send(b"\r");
                thread::sleep(Duration::from_millis(100));
                return;
            }
            self.send(b"j");
            thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "Could not select {label:?}:\n{}",
            self.parser.screen().contents()
        );
    }

    fn send(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
    }
}
impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn orphan_notice_is_nonblocking_and_deletion_defaults_to_keeping_the_computer() {
    let fixture = Fixture::new();
    let mut ui = fixture.ui();
    ui.wait("Set up this project");
    ui.wait("2 private computers");
    assert!(!fixture.removed.exists());
    ui.choose("Global settings");
    ui.choose("Private computers");
    ui.choose("deleted-alpha");
    ui.wait(&fixture.first);
    ui.choose("Delete private computer");
    ui.wait("Keep private computer");
    ui.send(b"\r");
    ui.wait("Reconnect moved project");
    ui.choose("Reconnect moved project");
    ui.wait("New project folder or chuggin.json");
    ui.send(fixture.project.to_str().unwrap().as_bytes());
    ui.send(b"\r");
    ui.wait("Reconnect and keep files");
    ui.send(b"\r"); // Cancel is the default for the path repair as well.
    ui.wait("Reconnect moved project");
    assert!(!fixture.removed.exists());
    let calls = fixture.calls();
    assert!(!calls.lines().any(|line| line.contains(" rm ")));
    assert!(!calls.contains(" stop "));
    assert!(!calls.contains(" commit "));
    assert!(!calls.contains(" create "));
    assert!(!calls.contains("prune"));
}

#[test]
fn explicit_orphan_deletion_removes_only_the_selected_managed_computer() {
    let fixture = Fixture::new();
    let mut ui = fixture.ui();
    ui.wait("2 private computers");
    ui.choose("Global settings");
    ui.choose("Private computers");
    ui.choose("deleted-alpha");
    ui.wait(&fixture.first);
    ui.choose("Delete private computer");
    ui.choose("Delete private computer permanently");
    let deadline = Instant::now() + Duration::from_secs(8);
    while !fixture.removed.exists() {
        ui.read();
        assert!(
            Instant::now() < deadline,
            "Container deletion did not happen:\n{}\n{}",
            fixture.calls(),
            ui.parser.screen().contents()
        );
        thread::sleep(Duration::from_millis(25));
    }
    ui.wait("Private computer deleted");
    let calls = fixture.calls();
    let deletes: Vec<_> = calls.lines().filter(|line| line.contains(" rm ")).collect();
    assert_eq!(
        deletes,
        vec![format!("--remote=false rm --force {}", fixture.first_id)]
    );
    assert!(!deletes.iter().any(|line| line.contains(&fixture.second)));
    assert!(!deletes.iter().any(|line| line.contains(&fixture.second_id)));
    assert!(!calls.contains("prune"));
    assert!(!calls.contains(" rmi "));
    assert!(
        fixture.project.exists(),
        "Deleting a computer must preserve its project/history folders"
    );
}

#[test]
fn reconnecting_to_an_unrelated_folder_preserves_the_original_computer() {
    let fixture = Fixture::new();
    let mut ui = fixture.ui();
    ui.wait("2 private computers");
    ui.choose("Global settings");
    ui.choose("Private computers");
    ui.choose("deleted-alpha");
    ui.choose("Reconnect moved project");
    ui.wait("New project folder or chuggin.json");
    ui.send(fixture.project.to_str().unwrap().as_bytes());
    ui.send(b"\r");
    ui.choose("Reconnect and keep files");
    ui.wait("Could not update private computer");
    assert!(!fixture.removed.exists());
    let calls = fixture.calls();
    for mutation in [" rm ", " stop ", " commit ", " create ", " rename "] {
        assert!(
            !calls.contains(mutation),
            "Unexpected mutation {mutation:?}: {calls}"
        );
    }
}

struct LiveComputer {
    podman: PathBuf,
    system_config: PathBuf,
    name: String,
    config: PathBuf,
    armed: bool,
}
impl LiveComputer {
    fn command(&self, args: &[&str]) -> std::process::Output {
        Command::new(&self.podman)
            .arg("--remote=false")
            .args(args)
            .env("XDG_CONFIG_HOME", &self.system_config)
            .env_remove("CONTAINER_HOST")
            .env_remove("CONTAINER_CONNECTION")
            .output()
            .unwrap()
    }
    fn checked(&self, args: &[&str]) -> Vec<u8> {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "Podman {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    fn inspect(&self) -> Value {
        serde_json::from_slice::<Value>(&self.checked(&["inspect", &self.name])).unwrap()[0].clone()
    }
}
impl Drop for LiveComputer {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Only this fixture's session label and exact identity are eligible.
        let list = self.command(&[
            "ps",
            "-a",
            "--no-trunc",
            "--filter",
            &format!("label=io.chuggin.session={}", self.name),
            "--format=json",
        ]);
        if list.status.success()
            && let Ok(values) = serde_json::from_slice::<Vec<Value>>(&list.stdout)
        {
            for value in values {
                let candidate = value["Names"]
                    .as_array()
                    .and_then(|names| names.first())
                    .and_then(Value::as_str)
                    .or(value["Names"].as_str());
                if let Some(name) = candidate
                    && (name == self.name || name.starts_with(&format!("{}-previous-", self.name)))
                {
                    let result = self.command(&["inspect", name]);
                    if let Ok(values) = serde_json::from_slice::<Vec<Value>>(&result.stdout)
                        && let Some(instance) = values.first()
                        && instance["Config"]["Labels"]["io.chuggin.managed"] == "true"
                        && instance["Config"]["Labels"]["io.chuggin.session"] == self.name
                        && let Some(id) = instance["Id"].as_str()
                    {
                        let _ = self.command(&["rm", "--force", "--time=0", id]);
                    }
                }
            }
        }
        let registry = fs::read(self.config.join("chuggin/computers/registry.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let Some(images) = registry
            .as_ref()
            .and_then(|registry| registry["computers"][&self.name]["snapshots"].as_array())
        {
            for image in images.iter().filter_map(Value::as_str) {
                if image.strip_prefix("sha256:").unwrap_or(image).len() == 64
                    && image
                        .strip_prefix("sha256:")
                        .unwrap_or(image)
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    let _ = self.command(&["image", "rm", image]);
                }
            }
        }
    }
}

#[test]
#[ignore = "Explicit disposable Podman lifecycle test; requires CHUGGIN_LIVE_RECONNECT=1, delegated controllers, and the cached desktop image"]
fn live_private_computer_reconnect_preserves_files_and_deletes_only_its_fixture() {
    if std::env::var("CHUGGIN_LIVE_RECONNECT").as_deref() != Ok("1") {
        eprintln!(
            "Skipping: set CHUGGIN_LIVE_RECONNECT=1 to authorize this disposable live fixture."
        );
        return;
    }
    let Some(podman) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("podman"))
        .find(|path| path.is_file())
    else {
        eprintln!("Skipping: Podman is not installed.");
        return;
    };
    let output = Command::new(&podman)
        .args(["--remote=false", "info", "--format=json"])
        .output()
        .unwrap();
    let Ok(info) = serde_json::from_slice::<Value>(&output.stdout) else {
        eprintln!("Skipping: rootless Podman info is unavailable.");
        return;
    };
    if info["host"]["security"]["rootless"] != true || info["host"]["cgroupVersion"] != "v2" {
        eprintln!("Skipping: rootless Podman with cgroup v2 is required.");
        return;
    }
    let controllers = info["host"]["cgroupControllers"].as_array();
    if ["cpu", "memory", "pids"].iter().any(|controller| {
        !controllers.is_some_and(|values| values.iter().any(|value| value == controller))
    }) {
        eprintln!(
            "Skipping: Podman does not report all required delegated controllers (cpu, memory, pids); found {}. No container was created.",
            info["host"]["cgroupControllers"]
        );
        return;
    }
    let mut hash = Sha256::new();
    for (name, bytes) in [
        (
            "Containerfile",
            include_bytes!("../assets/computer/Containerfile").as_slice(),
        ),
        (
            "desktop-packages",
            include_bytes!("../assets/computer/desktop-packages").as_slice(),
        ),
        (
            "start-desktop",
            include_bytes!("../assets/computer/start-desktop").as_slice(),
        ),
        (
            "start-session",
            include_bytes!("../assets/computer/start-session").as_slice(),
        ),
        (
            "start-audio",
            include_bytes!("../assets/computer/start-audio").as_slice(),
        ),
    ] {
        hash.update(name);
        hash.update(b"\0");
        hash.update(bytes);
    }
    let digest = format!("{:x}", hash.finalize());
    let image = format!("localhost/chuggin-computer:desktop-{}", &digest[..16]);
    if !Command::new(&podman)
        .args(["--remote=false", "image", "exists", &image])
        .status()
        .unwrap()
        .success()
    {
        eprintln!(
            "Skipping: required desktop image {image} is not cached. Nothing was installed or pulled."
        );
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("chuggin-reconnect-live-")
        .tempdir()
        .unwrap();
    let suffix = root.path().file_name().unwrap().to_str().unwrap();
    let project = root.path().join(format!("origin-{suffix}"));
    let moved = root.path().join(format!("moved-{suffix}"));
    let manager = root.path().join("manager");
    let config = root.path().join("config");
    let bin = root.path().join("bin");
    for path in [&project, &manager, &bin] {
        fs::create_dir(path).unwrap();
    }
    fs::write(project.join("host-note.txt"), "Keep host project files\n").unwrap();
    fs::write(project.join("chuggin.json"),json!({"repo":".","state_dir":".chuggin","activity_mode":"type2","execution":{"kind":"podman","memory_mb":1024,"cpus":1,"internet":false},"model":"unused","ollama_url":"http://127.0.0.1:1","context_tokens":8192,"output_tokens":1024,"implementation_calls":4,"retry_seconds":0}).to_string()).unwrap();
    let system_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config"));
    fs::write(
        bin.join("podman"),
        format!(
            "#!/bin/sh\nexport XDG_CONFIG_HOME={}\nexec {} \"$@\"\n",
            shell_words::quote(system_config.to_str().unwrap()),
            shell_words::quote(podman.to_str().unwrap())
        ),
    )
    .unwrap();
    fs::write(bin.join("xdg-open"), "#!/bin/sh\nexit 0\n").unwrap();
    for command in ["podman", "xdg-open"] {
        fs::set_permissions(bin.join(command), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let hash = format!(
        "{:x}",
        Sha256::digest(
            fs::canonicalize(&project)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
        )
    );
    let name = format!("chuggin-{}", &hash[..24]);
    let mut computer = LiveComputer {
        podman,
        system_config,
        name,
        config: config.clone(),
        armed: false,
    };
    assert!(
        !computer
            .command(&["container", "exists", &computer.name])
            .status
            .success(),
        "A fresh fixture cannot reuse an existing computer"
    );
    computer.armed = true;
    {
        let mut ui = Ui::start(&project, &config, &bin);
        ui.wait("Resume project");
        ui.choose("Project settings");
        ui.choose("More project settings");
        ui.choose("Computer environment");
        ui.choose("Open desktop");
        ui.wait_for("Back to Chuggin", Duration::from_secs(180));
        assert!(project.join(".chuggin/computer/ready.json").is_file());
        // No Resume/Chat action is ever selected: this test performs no inference.
    }
    let original_id = computer.inspect()["Id"].as_str().unwrap().to_owned();
    computer.checked(&[
        "exec",
        &computer.name,
        "sh",
        "-c",
        "printf 'Preserved through reconnect\\n' > /root/preserved.txt",
    ]);
    let transcript = br#"{"messages":[{"role":"user","content":"Keep this saved conversation"}]}"#;
    fs::write(project.join(".chuggin/conversation.json"), transcript).unwrap();
    fs::rename(&project, &moved).unwrap();
    {
        let mut ui = Ui::start(&manager, &config, &bin);
        ui.wait("private computer");
        ui.choose("Global settings");
        ui.choose("Private computers");
        ui.choose(project.file_name().unwrap().to_str().unwrap());
        ui.choose("Reconnect moved project");
        ui.wait("New project folder or chuggin.json");
        ui.send(moved.to_str().unwrap().as_bytes());
        ui.send(b"\r");
        ui.choose("Reconnect and keep files");
        ui.wait_for("Private computer reconnected", Duration::from_secs(180));
    }
    assert_ne!(computer.inspect()["Id"], original_id);
    assert_eq!(
        computer.checked(&["exec", &computer.name, "cat", "/root/preserved.txt"]),
        b"Preserved through reconnect\n"
    );
    assert_eq!(
        fs::read(moved.join(".chuggin/conversation.json")).unwrap(),
        transcript
    );
    assert!(
        !moved.join(".chuggin/state.json").exists(),
        "No inference cycle may have started"
    );
    {
        let mut ui = Ui::start(&manager, &config, &bin);
        ui.wait("Set up this project");
        ui.choose("Global settings");
        ui.choose("Private computers");
        ui.choose(moved.file_name().unwrap().to_str().unwrap());
        ui.choose("Delete private computer");
        ui.choose("Delete private computer permanently");
        ui.wait_for("Private computer deleted", Duration::from_secs(90));
    }
    assert!(
        !computer
            .command(&["container", "exists", &computer.name])
            .status
            .success()
    );
    assert_eq!(
        fs::read(moved.join(".chuggin/conversation.json")).unwrap(),
        transcript
    );
    assert_eq!(
        fs::read_to_string(moved.join("host-note.txt")).unwrap(),
        "Keep host project files\n"
    );
    eprintln!(
        "Real Podman reconnect retained the file and conversation, changed container ID, and deleted only the disposable computer."
    );
}
