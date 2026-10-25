//! One-time application provisioning for an exclusively owned private computer.
//! Success is recorded outside the guest. Later starts respect applications the
//! model intentionally removes, rather than enforcing a package set forever.
use super::Target;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
    process::{Child, Stdio},
    time::{Duration, Instant},
};

const PACKAGES: &str = include_str!("../../assets/computer/desktop-packages");
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LOG_LIMIT: u64 = 8 * 1024 * 1024;

fn revision() -> String {
    format!("{:x}", Sha256::digest(PACKAGES.as_bytes()))
}

pub(super) fn installed(t: &Target) -> bool {
    fs::read(t.state.join("computer/desktop-apps.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|marker| marker["name"] == t.name && marker["revision"] == revision())
}

pub(super) fn pending(c: &crate::runner::Config) -> bool {
    c.execution.is_private() && super::target(c).is_ok_and(|target| !installed(&target))
}

fn manifest(text: &str) -> Result<Vec<String>> {
    let mut packages = BTreeSet::new();
    for line in text.lines() {
        let package = line.trim();
        ensure!(
            !package.is_empty()
                && package.len() <= 100
                && package.as_bytes()[0].is_ascii_alphanumeric()
                && package
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+.-".contains(&b)),
            "Invalid desktop package name in the bundled manifest"
        );
        ensure!(
            packages.insert(package.to_owned()),
            "Duplicate desktop package: {package}"
        );
    }
    ensure!(
        !packages.is_empty() && packages.len() <= 200,
        "Invalid desktop package manifest size"
    );
    Ok(packages.into_iter().collect())
}

fn missing_from_query(packages: &[String], output: &str) -> Result<Vec<String>> {
    let expected: BTreeSet<&str> = packages.iter().map(String::as_str).collect();
    let mut installed = BTreeSet::new();
    for line in output.lines() {
        let (package, status) = line
            .split_once('\t')
            .context("Invalid package inventory response")?;
        let package = package.split(':').next().unwrap_or(package);
        ensure!(
            expected.contains(package),
            "Unexpected package inventory entry: {package}"
        );
        if status.trim() == "install ok installed" {
            installed.insert(package);
        }
    }
    Ok(packages
        .iter()
        .filter(|p| !installed.contains(p.as_str()))
        .cloned()
        .collect())
}

fn missing(t: &Target, packages: &[String]) -> Result<Vec<String>> {
    let mut command = super::exec_base(t);
    // dpkg-query exits 1 when some requested names are absent. Preserve its
    // installed rows while allowing precisely that expected exit status.
    command
        .args([
            "/bin/sh",
            "-c",
            "dpkg-query -W '-f=${Package}\\t${Status}\\n' \"$@\"; status=$?; [ \"$status\" -le 1 ]",
            "desktop-package-inventory",
        ])
        .args(packages);
    let output = super::bounded(command, None, Duration::from_secs(30), 64 * 1024)?;
    missing_from_query(
        packages,
        std::str::from_utf8(&output).context("Non-UTF8 package inventory")?,
    )
}

// The outer guest-command records the shell's process group for stop_job. A
// private stdin heartbeat also kills that group if the host disappears during
// setup; timeout alone would leave an abandoned install running for half an hour.
const INSTALL_SCRIPT: &str = r#"
set -eu
exec 3<&0
group=$$
(
    while IFS= read -r heartbeat <&3; do :; done
    /bin/kill -TERM -- "-$group" 2>/dev/null || :
) <&3 &
monitor=$!
(
    sleep 1800
    /bin/kill -TERM -- "-$group" 2>/dev/null || :
) &
timer=$!
trap 'kill "$monitor" "$timer" 2>/dev/null || :' EXIT
trap 'exit 143' HUP INT TERM
export DEBIAN_FRONTEND=noninteractive APT_LISTCHANGES_FRONTEND=none
apt-get -o DPkg::Lock::Timeout=60 -o Acquire::Retries=2 update </dev/null 3<&-
apt-get -o DPkg::Lock::Timeout=60 -o Acquire::Retries=2 -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold install -y --no-install-recommends --no-upgrade --no-remove "$@" </dev/null 3<&-
"#;

fn halt(t: &Target, job: &str, child: &mut Child) -> Result<()> {
    let host_running = child.try_wait()?.is_none();
    let stopped = super::stop_job(t, job);
    if host_running {
        // Do not signal an already-reaped PID: after slow guest cleanup its
        // number could belong to an unrelated host process group.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    stopped.context("Could not confirm installer cleanup inside the private computer; its stdin/timeout watchdog remains active")
}

fn stream_log(reader: &mut fs::File) -> Result<()> {
    let mut chunk = [0; 8192];
    for _ in 0..16 {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        crate::events::send(crate::events::Event::Delta(
            String::from_utf8_lossy(&chunk[..count]).into_owned(),
        ));
    }
    Ok(())
}

fn install(t: &Target, packages: &[String], log_path: &Path) -> Result<()> {
    let log = fs::File::create(log_path)?;
    let job = format!("desktop-install-{}", crate::operator::id());
    let mut command = super::exec_base(t);
    command
        .args(super::runtime_args(t)?)
        .args([
            "guest-command",
            "--id",
            &job,
            "--",
            "/bin/sh",
            "-c",
            INSTALL_SCRIPT,
            "desktop-package-install",
        ])
        .args(packages);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .spawn()
        .context("Could not start package installation inside the private computer")?;
    let mut heartbeat = child
        .stdin
        .take()
        .context("Installer heartbeat unavailable")?;
    let mut reader = fs::File::open(log_path)?;
    let start = Instant::now();
    let mut last_heartbeat = start.checked_sub(Duration::from_secs(2)).unwrap_or(start);
    let result = (|| -> Result<()> {
        loop {
            stream_log(&mut reader)?;
            ensure!(
                log.metadata()?.len() <= LOG_LIMIT,
                "Desktop application installation exceeded its 8 MiB diagnostic log limit"
            );
            if let Some(status) = child.try_wait()? {
                stream_log(&mut reader)?;
                ensure!(
                    status.success(),
                    "Desktop application installation exited with {status}"
                );
                return Ok(());
            }
            ensure!(
                start.elapsed() < INSTALL_TIMEOUT,
                "Desktop application installation exceeded 30 minutes"
            );
            if last_heartbeat.elapsed() >= Duration::from_secs(1) {
                heartbeat
                    .write_all(b"continue\n")
                    .context("Private installer connection closed")?;
                last_heartbeat = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })();
    drop(heartbeat);
    let result = if let Err(error) = result {
        let cleanup = halt(t, &job, &mut child);
        // Fixed apt output normally fits comfortably. After stopping the writer,
        // keep the saved log bounded even if a package's maintainer script rambled.
        if log.metadata()?.len() > LOG_LIMIT {
            log.set_len(LOG_LIMIT)?;
        }
        if let Err(cleanup) = cleanup {
            Err(error.context(format!("{cleanup:#}")))
        } else {
            Err(error)
        }
    } else {
        Ok(())
    };
    result.with_context(||format!("Desktop apps could not be installed. Existing files are retained. Inspect {} for the exact apt/dpkg error, resolve any package lock or broken package state inside this private computer, then retry setup. No host package command was run.",log_path.display()))
}

pub(super) fn ensure(t: &Target) -> Result<()> {
    if installed(t) {
        return Ok(());
    }
    let packages = manifest(PACKAGES)?;
    let absent = missing(t, &packages)?;
    if !absent.is_empty() {
        ensure!(
            t.internet,
            "Desktop apps are missing and this private computer's Internet access is disabled. Enable public Internet in Computer resource limits, then retry setup. Existing files are retained."
        );
        let dir = t.state.join("computer");
        fs::create_dir_all(&dir)?;
        let log = dir.join("desktop-apps.log");
        if log.exists() {
            fs::rename(&log, dir.join("desktop-apps-previous.log"))?;
        }
        crate::events::send(crate::events::Event::Phase(
            "Installing desktop applications inside the private computer".into(),
        ));
        crate::events::log(format!(
            "Adding {} desktop packages inside this private computer; existing files and applications are retained. Output: {}",
            absent.len(),
            log.display()
        ));
        install(t, &absent, &log)?;
        let remaining = missing(t, &packages)?;
        ensure!(
            remaining.is_empty(),
            "Desktop application installation is incomplete ({}). Inspect {} and retry setup.",
            remaining.join(", "),
            log.display()
        );
    }
    crate::setup::save(
        &t.state.join("computer/desktop-apps.json"),
        &json!({"name":t.name,"revision":revision(),"packages":packages}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    struct ShellProbe(Child);
    #[cfg(unix)]
    impl Drop for ShellProbe {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[cfg(unix)]
    fn probe(root: &Path, block: bool) -> ShellProbe {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        let apt = root.join("apt-get");
        fs::write(&apt,"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$DESKTOP_TEST_LOG\"\nif [ \"${DESKTOP_TEST_BLOCK:-}\" = yes ]; then sleep 60; fi\n").unwrap();
        fs::set_permissions(&apt, fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", INSTALL_SCRIPT, "desktop-test", "gimp", "blender"])
            .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
            .env("DESKTOP_TEST_LOG", root.join("calls"))
            .env("DESKTOP_TEST_BLOCK", if block { "yes" } else { "no" })
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        ShellProbe(command.spawn().unwrap())
    }
    #[cfg(unix)]
    #[test]
    fn installer_uses_explicit_missing_packages_and_does_not_upgrade_existing_apps() {
        let dir = tempfile::tempdir().unwrap();
        let mut process = probe(dir.path(), false);
        let _heartbeat = process.0.stdin.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(calls.lines().count(), 2);
        assert!(
            calls.contains(
                "install -y --no-install-recommends --no-upgrade --no-remove gimp blender"
            )
        );
        assert!(!calls.contains("autoremove"));
    }
    #[cfg(unix)]
    #[test]
    fn losing_host_heartbeat_cancels_the_private_installer_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut process = probe(dir.path(), true);
        let heartbeat = process.0.stdin.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !dir.path().join("calls").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(heartbeat);
        let status = loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Host disappearance did not cancel the installer"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
    }
    #[test]
    fn manifest_rejects_options_shell_and_duplicate_names() {
        for text in [
            "",
            "--yes",
            "gimp\nblender;rm",
            "gimp\ngimp",
            "libreoffice $(x)",
            "../gimp",
        ] {
            assert!(manifest(text).is_err(), "{text}");
        }
        let bundled = manifest(PACKAGES).unwrap();
        for expected in [
            "gimp",
            "blender",
            "audacity",
            "libreoffice-writer",
            "libreoffice-calc",
            "libreoffice-impress",
        ] {
            assert!(bundled.iter().any(|p| p == expected));
        }
    }
    #[test]
    fn installed_inventory_requires_complete_status_and_exact_package_names() {
        let packages = vec!["gimp".into(), "blender".into(), "audacity".into()];
        let output = "gimp\tinstall ok installed\nblender:amd64\tinstall ok unpacked\naudacity\tdeinstall ok config-files\n";
        assert_eq!(
            missing_from_query(&packages, output).unwrap(),
            vec!["blender", "audacity"]
        );
        assert!(missing_from_query(&packages, "other\tinstall ok installed\n").is_err());
        assert!(missing_from_query(&packages, "gimp\n").is_err());
    }
    #[test]
    fn completion_marker_matches_both_computer_identity_and_bundle_revision() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target {
            name: "private-test".into(),
            state: dir.path().into(),
            runtime: dir.path().join("runtime"),
            internet: false,
        };
        assert!(!installed(&target));
        let marker = target.state.join("computer/desktop-apps.json");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        crate::setup::save(&marker, &json!({"name":"other","revision":revision()})).unwrap();
        assert!(!installed(&target));
        crate::setup::save(&marker, &json!({"name":target.name,"revision":"earlier"})).unwrap();
        assert!(!installed(&target));
        crate::setup::save(&marker, &json!({"name":target.name,"revision":revision()})).unwrap();
        assert!(installed(&target));
    }
}
