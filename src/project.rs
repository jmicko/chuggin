static ACTIVE_CHECK: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub fn kill_active_check() {
    let pid = ACTIVE_CHECK.load(std::sync::atomic::Ordering::SeqCst);
    #[cfg(unix)]
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("Cannot execute git; install Git and put it on PATH")?;
    anyhow::ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    anyhow::ensure!(!relative.is_empty(), "Empty path");
    for c in path.components() {
        match c {
            Component::Normal(n) => {
                anyhow::ensure!(n != ".git" && n != ".chuggin", "Reserved path")
            }
            _ => bail!("Only relative paths without traversal are allowed"),
        }
    }
    let mut target = root.to_path_buf();
    for c in path.components() {
        target.push(c);
        if let Ok(m) = fs::symlink_metadata(&target) {
            anyhow::ensure!(
                !m.file_type().is_symlink(),
                "Symlinks are not available to file tools"
            );
        }
    }
    Ok(target)
}
pub fn read(root: &Path, path: &str) -> Result<String> {
    let p = safe_path(root, path)?;
    Ok(fs::read_to_string(p)?)
}
pub fn write(root: &Path, path: &str, content: &str) -> Result<bool> {
    use std::io::Write;
    let p = safe_path(root, path)?;
    match fs::read(&p) {
        Ok(previous) if previous == content.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp =
        tempfile::NamedTempFile::new_in(p.parent().context("Missing parent directory")?)?;
    if let Ok(metadata) = fs::metadata(&p) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(&p)?;
    Ok(true)
}
pub fn inventory(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    fn visit(root: &Path, prefix: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(root.join(prefix))? {
            let e = entry?;
            let name = e.file_name();
            let n = name.to_string_lossy();
            if [
                ".git",
                ".chuggin",
                "target",
                "node_modules",
                ".venv",
                "__pycache__",
                "dist",
                "build",
            ]
            .contains(&n.as_ref())
            {
                continue;
            }
            let ty = e.file_type()?;
            let rel = prefix.join(name);
            if ty.is_dir() {
                visit(root, &rel, out)?;
            } else if ty.is_file() {
                out.push(rel.to_string_lossy().into());
            }
            anyhow::ensure!(
                out.len() <= 10_000,
                "Project inventory exceeds 10,000 files"
            );
        }
        Ok(())
    }
    visit(root, Path::new(""), &mut files)?;
    files.sort();
    Ok(files)
}
pub fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    120
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub argv: Vec<String>,
    pub passed: bool,
    pub timed_out: bool,
    pub output: String,
}
pub fn check(root: &Path, c: &Check, log: &Path, stop: &AtomicBool) -> Result<CheckResult> {
    anyhow::ensure!(!c.argv.is_empty(), "Check argv must not be empty");
    let file = fs::File::create(log)?;
    crate::events::send(crate::events::Event::Check {
        command: c.argv.join(" "),
        path: log.into(),
    });
    let err = file.try_clone()?;
    let mut command = Command::new(&c.argv[0]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .args(&c.argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err))
        .spawn()
        .with_context(|| format!("Cannot run check {:?}", c.argv))?;
    ACTIVE_CHECK.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    let start = Instant::now();
    let mut timed_out = false;
    let mut exit_code = None;
    let passed = loop {
        if let Some(status) = child.try_wait()? {
            exit_code = status.code();
            break status.success();
        }
        if start.elapsed() > Duration::from_secs(c.timeout_seconds) || stop.load(Ordering::SeqCst) {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        thread::sleep(Duration::from_millis(100));
    };
    // Terminate remaining subprocesses, including children left by a finished check.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    ACTIVE_CHECK.store(0, std::sync::atomic::Ordering::SeqCst);
    // Read only a bounded tail. Full output remains in the artifact log.
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(log)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(12000)))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    crate::events::send(crate::events::Event::CheckDone(passed));
    Ok(CheckResult {
        exit_code,
        argv: c.argv.clone(),
        passed,
        timed_out,
        output: String::from_utf8_lossy(&bytes).into(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_cannot_escape() {
        let d = tempfile::tempdir().unwrap();
        for p in ["../x", "/tmp/x", ".git/config", "src/../../x"] {
            assert!(safe_path(d.path(), p).is_err());
        }
        assert!(safe_path(d.path(), "src/a.rs").is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/tmp", d.path().join("escape")).unwrap();
        assert!(safe_path(d.path(), "escape/file").is_err());
    }
    #[test]
    fn unicode_excerpt() {
        assert!(excerpt("ééé", 3).starts_with('é'));
    }
}

pub fn read_lines(root: &Path, path: &str, start: usize, count: usize) -> Result<String> {
    use std::io::BufRead;
    anyhow::ensure!(
        start > 0 && count > 0,
        "start_line and line_count must be positive"
    );
    let mut reader = std::io::BufReader::new(fs::File::open(safe_path(root, path)?)?);
    let mut line = String::new();
    let mut total = 0;
    let mut byte_offset = 0;
    let mut shown = String::new();
    let mut continuation = None;
    let end = start.saturating_add(count.min(120));
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        total += 1;
        if total >= start && continuation.is_none() {
            if total >= end || (!shown.is_empty() && shown.len() + line.len() > 9000) {
                continuation = Some(format!("Continue with start_line={total}"));
            } else if line.len() > 9000 {
                let mut cut = 8000;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                shown.push_str(&format!("{total}: {}\n", &line[..cut]));
                continuation = Some(format!(
                    "Long line continues: call read_file with byte_offset={} for exact text chunks",
                    byte_offset + cut
                ));
            } else {
                shown.push_str(&format!(
                    "{total}: {}\n",
                    line.trim_end_matches(['\r', '\n'])
                ));
            }
        }
        byte_offset += bytes;
    }
    let mut out = format!("{path}: {total} lines total; showing from line {start}\n{shown}");
    if let Some(continuation) = continuation {
        out.push_str(&continuation);
    }
    Ok(out)
}
pub fn read_bytes(root: &Path, path: &str, offset: u64) -> Result<serde_json::Value> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(safe_path(root, path)?)?;
    let total = file.metadata()?.len();
    anyhow::ensure!(offset <= total, "byte_offset exceeds file size");
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(9000).read_to_end(&mut bytes)?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()])?,
        Err(e) => {
            return Err(e)
                .context("Use the next_byte_offset returned by read_file; file must be UTF-8");
        }
    };
    let next = offset + text.len() as u64;
    anyhow::ensure!(
        next > offset || offset == total,
        "File contains incomplete UTF-8"
    );
    Ok(
        serde_json::json!({"path":path,"text":text,"byte_offset":offset,"total_bytes":total,"next_byte_offset":if next<total{Some(next)}else{None}}),
    )
}
pub fn edit(root: &Path, path: &str, old: &str, new: &str) -> Result<String> {
    anyhow::ensure!(!old.is_empty(), "old_text must not be empty");
    let text = read(root, path)?;
    let matches = text.matches(old).count();
    anyhow::ensure!(
        matches == 1,
        "old_text matched {matches} locations; expected exactly one. Read the current text and include enough surrounding context to identify a unique match."
    );
    let line = text[..text.find(old).context("Missing match")?]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1;
    let changed = write(root, path, &text.replacen(old, new, 1))?;
    Ok(serde_json::json!({"path":path,"changed":changed,"line":line,"message":if changed {"Replacement applied"} else {"No bytes changed: old_text and new_text are identical. Inspect the result or choose a different action."}}).to_string())
}

#[cfg(test)]
mod editing_tests {
    use super::*;
    #[test]
    fn large_files_are_readable_and_edits_report_real_changes() {
        let dir = tempfile::tempdir().unwrap();
        let content = format!(
            "{}unique target\n",
            "padding padding padding\n".repeat(10000)
        );
        assert!(write(dir.path(), "large.txt", &content).unwrap());
        let before = fs::metadata(dir.path().join("large.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(!write(dir.path(), "large.txt", &content).unwrap());
        let noop: serde_json::Value = serde_json::from_str(
            &edit(dir.path(), "large.txt", "unique target", "unique target").unwrap(),
        )
        .unwrap();
        assert_eq!(noop["changed"], false);
        assert_eq!(
            fs::metadata(dir.path().join("large.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert!(
            read_lines(dir.path(), "large.txt", 10001, 1)
                .unwrap()
                .contains("10001: unique target")
        );
        let changed: serde_json::Value = serde_json::from_str(
            &edit(dir.path(), "large.txt", "unique target", "actual change").unwrap(),
        )
        .unwrap();
        assert_eq!(changed["changed"], true);
        assert_eq!(changed["line"], 10001);
        assert!(
            read(dir.path(), "large.txt")
                .unwrap()
                .ends_with("actual change\n")
        );
        assert!(
            edit(dir.path(), "large.txt", "absent", "new")
                .unwrap_err()
                .to_string()
                .contains("0 locations")
        );
        assert!(
            edit(dir.path(), "large.txt", "padding", "new")
                .unwrap_err()
                .to_string()
                .contains("30000 locations")
        );
    }

    #[test]
    fn long_unicode_lines_have_lossless_byte_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let content = "界\"".repeat(10000);
        write(dir.path(), "long.txt", &content).unwrap();
        assert!(
            read_lines(dir.path(), "long.txt", 1, 80)
                .unwrap()
                .contains("byte_offset=")
        );
        let mut offset = 0;
        let mut restored = String::new();
        loop {
            let page = read_bytes(dir.path(), "long.txt", offset).unwrap();
            restored.push_str(page["text"].as_str().unwrap());
            let Some(next) = page["next_byte_offset"].as_u64() else {
                break;
            };
            offset = next;
        }
        assert_eq!(restored, content);
        assert!(read_bytes(dir.path(), "long.txt", 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_edits_preserve_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "script", "before").unwrap();
        fs::set_permissions(dir.path().join("script"), fs::Permissions::from_mode(0o755)).unwrap();
        edit(dir.path(), "script", "before", "after").unwrap();
        assert_eq!(
            fs::metadata(dir.path().join("script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    #[test]
    fn reads_can_continue_past_the_first_page() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "large.rs",
            &(1..=200).map(|n| format!("line {n}\n")).collect::<String>(),
        )
        .unwrap();
        let first = read_lines(dir.path(), "large.rs", 1, 80).unwrap();
        assert!(first.contains("Continue with start_line=81"));
        let next = read_lines(dir.path(), "large.rs", 81, 80).unwrap();
        assert!(next.contains("81: line 81"));
        assert!(!next.contains("1: line 1\n"));
    }
    #[test]
    fn targeted_edits_require_one_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "source.rs", "one\ntwo\none\n").unwrap();
        assert!(edit(dir.path(), "source.rs", "one", "three").is_err());
        edit(dir.path(), "source.rs", "two", "three").unwrap();
        assert_eq!(read(dir.path(), "source.rs").unwrap(), "one\nthree\none\n");
    }
}
