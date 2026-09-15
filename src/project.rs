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
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
            Component::Normal(n) => anyhow::ensure!(
                n != ".git" && n != ".chuggin" && n != ".lupin",
                "Reserved path"
            ),
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
    anyhow::ensure!(
        fs::metadata(&p)?.len() <= 100_000,
        "File exceeds 100KB; narrow the task"
    );
    Ok(fs::read_to_string(p)?)
}
pub fn write(root: &Path, path: &str, content: &str) -> Result<()> {
    anyhow::ensure!(content.len() <= 100_000, "Write exceeds 100KB");
    let p = safe_path(root, path)?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(p, content)?;
    Ok(())
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
                ".lupin",
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
pub fn snapshot(root: &Path) -> Result<BTreeMap<String, String>> {
    inventory(root)?
        .into_iter()
        .map(|p| {
            let data = fs::read(safe_path(root, &p)?)?;
            Ok((p, format!("{:x}", Sha256::digest(data))))
        })
        .collect()
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
pub fn context(root: &Path, files: &[String], limit: usize) -> String {
    let mut out = String::new();
    for name in files {
        if out.len() >= limit {
            break;
        }
        if let Ok(s) = read(root, name) {
            out.push_str(&format!(
                "\n--- {name} ---\n{}",
                excerpt(&s, (limit - out.len()).min(20_000))
            ));
        }
    }
    out
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
    anyhow::ensure!(
        start > 0 && count > 0,
        "start_line and line_count must be positive"
    );
    let text = read(root, path)?;
    let lines: Vec<_> = text.lines().collect();
    let end = start
        .saturating_sub(1)
        .saturating_add(count.min(120))
        .min(lines.len());
    let mut out = format!(
        "{path}: {} lines total; showing from line {start}\n",
        lines.len()
    );
    for (i, line) in lines.iter().enumerate().take(end).skip(start - 1) {
        if out.len() + line.len() > 10000 {
            out.push_str(&format!("\nContinue with start_line={}", i + 1));
            return Ok(out);
        }
        out.push_str(&format!("{}: {line}\n", i + 1));
    }
    if end < lines.len() {
        out.push_str(&format!("Continue with start_line={}", end + 1));
    }
    Ok(out)
}
pub fn edit(root: &Path, path: &str, old: &str, new: &str) -> Result<String> {
    anyhow::ensure!(!old.is_empty(), "old_text must not be empty");
    let text = read(root, path)?;
    anyhow::ensure!(
        text.matches(old).count() == 1,
        "old_text must match exactly once; read the relevant lines first"
    );
    write(root, path, &text.replacen(old, new, 1))?;
    Ok(format!("Edited {path}"))
}

#[cfg(test)]
mod editing_tests {
    use super::*;
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
