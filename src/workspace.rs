//! Visible checkout operations. Recovery snapshots never use the user's index.
use crate::project;
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

pub fn head(root: &Path) -> String {
    project::git(root, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default()
}
pub fn branch(root: &Path) -> Result<String> {
    project::git(root, &["symbolic-ref", "--short", "HEAD"])
        .context("Choose a branch before starting Chuggin; this checkout has detached HEAD")
}
pub fn check(root: &Path, expected: &str) -> Result<()> {
    anyhow::ensure!(
        branch(root)? == expected,
        "The checkout branch changed. Work is retained. Return to the original branch or use Progress → Use current branch before resuming."
    );
    let dir = git_dir(root)?;
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "BISECT_LOG",
    ] {
        anyhow::ensure!(
            !dir.join(name).exists(),
            "Finish the Git operation ({name}) before resuming Chuggin"
        );
    }
    anyhow::ensure!(
        project::git(root, &["ls-files", "--unmerged"])?.is_empty(),
        "Resolve Git conflicts before resuming Chuggin"
    );
    Ok(())
}
pub fn git_dir(root: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(project::git(
        root,
        &["rev-parse", "--absolute-git-dir"],
    )?))
}
pub fn paths(root: &Path, state: Option<&Path>) -> Result<Vec<String>> {
    let mut paths = vec![
        ".".into(),
        ":(exclude).chuggin".into(),
        ":(exclude)chuggin.json".into(),
        ":(exclude,glob)**/.chuggin-save-*".into(),
    ];
    if let Some(state) = state
        && let Ok(relative) = state.strip_prefix(fs::canonicalize(root)?)
        && !relative.as_os_str().is_empty()
    {
        paths.push(format!(":(exclude){}", relative.to_string_lossy()));
    }
    Ok(paths)
}
fn checked(output: Output) -> Result<String> {
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
pub struct Index {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}
impl Index {
    pub fn new(root: &Path, tree: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let index = Self {
            path: dir.path().join("index"),
            _dir: dir,
        };
        if tree.is_empty() {
            index.git(root, &["read-tree", "--empty"])?;
        } else {
            index.git(root, &["read-tree", tree])?;
        }
        Ok(index)
    }
    pub fn command(&self, root: &Path) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(root).env("GIT_INDEX_FILE", &self.path);
        c
    }
    pub fn git(&self, root: &Path, args: &[&str]) -> Result<String> {
        checked(self.command(root).args(args).output()?)
    }
}
pub fn tree(root: &Path, state: Option<&Path>) -> Result<String> {
    let index = Index::new(root, &head(root))?;
    // Enumerate first: some Git versions reject ignored *exclusion* pathspecs
    // passed to `add`. Literal, NUL-separated names also preserve unusual paths.
    let files = index
        .command(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ])
        .args(paths(root, state)?)
        .output()?;
    anyhow::ensure!(
        files.status.success(),
        "{}",
        String::from_utf8_lossy(&files.stderr)
    );
    if !files.stdout.is_empty() {
        let mut list = tempfile::NamedTempFile::new()?;
        std::io::Write::write_all(&mut list, &files.stdout)?;
        let mut cmd = index.command(root);
        cmd.env("GIT_LITERAL_PATHSPECS", "1")
            .args(["add", "-A", "--pathspec-file-nul"])
            .arg(format!("--pathspec-from-file={}", list.path().display()));
        checked(cmd.output()?)?;
    }
    index.git(root, &["write-tree"])
}
pub fn commit_tree(root: &Path, tree: &str, parents: &[&str], message: &str) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(["commit-tree", tree]);
    for parent in parents.iter().filter(|p| !p.is_empty()) {
        cmd.args(["-p", parent]);
    }
    checked(cmd.args(["-m", message]).output()?)
}
pub fn autosave(root: &Path, state: &Path, id: &str, message: &str) -> Result<(String, bool)> {
    let reference = format!("refs/chuggin/autosaves/{id}");
    project::git(root, &["check-ref-format", &reference])?;
    let previous = project::git(root, &["rev-parse", "--verify", &reference]).unwrap_or_default();
    let tree = tree(root, Some(state))?;
    if !previous.is_empty()
        && project::git(root, &["rev-parse", &format!("{previous}^{{tree}}")])? == tree
    {
        return Ok((previous, false));
    }
    let parent = if previous.is_empty() {
        head(root)
    } else {
        previous.clone()
    };
    let commit = commit_tree(root, &tree, &[&parent], message)?;
    project::git(
        root,
        &[
            "update-ref",
            &reference,
            &commit,
            if previous.is_empty() { "" } else { &previous },
        ],
    )?;
    Ok((commit, true))
}
pub fn staged(root: &Path, state: &Path) -> Result<bool> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--name-only", "--"])
        .args(paths(root, Some(state))?);
    Ok(!checked(cmd.output()?)?.is_empty())
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Promotion {
    root: PathBuf,
    state: PathBuf,
    old_head: String,
    tree: String,
    message: String,
    lock_id: u64,
    had_index: bool,
}
fn file_id(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0)
    }
}
fn normalized_commit_message(root: &Path, message: &str) -> Result<String> {
    // Git's commit cleanup removes trailing whitespace and repeated blank lines.
    // Use Git's own rules for both messages, including journals from older runs.
    let mut input = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(&mut input, message.as_bytes())?;
    checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("stripspace")
            .stdin(input.reopen()?)
            .output()?,
    )
}
/// Complete an interrupted commit without replaying hooks or modifying project files.
pub fn recover_promotion(root: &Path) -> Result<()> {
    let dir = git_dir(root)?.join("chuggin-promotion");
    let record = dir.join("record.json");
    if !record.exists() {
        return Ok(());
    }
    let journal: Promotion = serde_json::from_slice(&fs::read(&record)?)?;
    anyhow::ensure!(
        fs::canonicalize(root)? == fs::canonicalize(&journal.root)?,
        "Pending commit belongs to another project scope"
    );
    let target = git_dir(root)?.join("index");
    let lock = target.with_extension("lock");
    let owned_lock = lock.exists() && journal.lock_id != 0 && file_id(&lock)? == journal.lock_id;
    anyhow::ensure!(
        !lock.exists() || owned_lock,
        "Git index is busy. Pending commit recovery is retained at {}",
        dir.display()
    );
    let current = head(root);
    if current != journal.old_head {
        let parents = project::git(root, &["show", "-s", "--format=%P", &current])?;
        let subject = project::git(root, &["show", "-s", "--format=%B", &current])?;
        anyhow::ensure!(
            parents == journal.old_head
                && (subject.trim() == journal.message.trim()
                    || normalized_commit_message(root, &subject)?
                        == normalized_commit_message(root, &journal.message)?),
            "HEAD changed during commit recovery; inspect {} before continuing",
            dir.display()
        );
        let original = if journal.had_index {
            Some(fs::read(dir.join("index.before"))?)
        } else {
            None
        };
        let index = Index::new(root, "")?;
        if let Some(bytes) = &original {
            fs::write(&index.path, bytes)?;
        }
        let mut cmd = index.command(root);
        cmd.args(["reset", "-q", &current, "--"])
            .args(paths(root, Some(&journal.state))?);
        checked(cmd.output()?)?;
        let now = fs::read(&target).ok();
        let updated = fs::read(&index.path)?;
        if now.as_deref() != Some(updated.as_slice()) {
            anyhow::ensure!(
                now == original,
                "Your index changed after an interrupted commit. Both index versions are retained at {}",
                dir.display()
            );
            if !owned_lock {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lock)?;
            }
            fs::write(&lock, updated)?;
            fs::rename(&lock, &target)?;
        }
    }
    if owned_lock && lock.exists() {
        fs::remove_file(&lock)?;
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}
/// Snapshotting is private; normal commits preserve hooks, signing, and unrelated staging.
pub fn promote(
    root: &Path,
    state: &Path,
    expected_head: &str,
    expected_tree: &str,
    message: &str,
) -> Result<String> {
    promote_inner(root, state, expected_head, expected_tree, message, false)
}
pub fn commit_current(
    root: &Path,
    state: &Path,
    expected_head: &str,
    expected_tree: &str,
    message: &str,
) -> Result<String> {
    promote_inner(root, state, expected_head, expected_tree, message, true)
}
fn promote_inner(
    root: &Path,
    state: &Path,
    expected_head: &str,
    expected_tree: &str,
    message: &str,
    include_staged: bool,
) -> Result<String> {
    recover_promotion(root)?;
    anyhow::ensure!(
        head(root) == expected_head,
        "HEAD changed; autosave retained, commit deferred"
    );
    if !expected_head.is_empty()
        && project::git(root, &["rev-parse", &format!("{expected_head}^{{tree}}")])?
            == expected_tree
    {
        return Ok(expected_head.into());
    }
    anyhow::ensure!(
        include_staged || !staged(root, state)?,
        "Your staged project changes are preserved. Commit or unstage them in Git before automatic task commits can continue."
    );
    anyhow::ensure!(
        tree(root, Some(state))? == expected_tree,
        "Files changed after validation; autosave retained, commit deferred"
    );
    let dir = git_dir(root)?.join("chuggin-promotion");
    fs::create_dir_all(&dir)?;
    let target = git_dir(root)?.join("index");
    let lock_path = target.with_extension("lock");
    let lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .context("Git index busy; commit deferred")?;
    let journal = Promotion {
        root: root.into(),
        state: state.into(),
        old_head: expected_head.into(),
        tree: expected_tree.into(),
        message: message.into(),
        lock_id: file_id(&lock_path)?,
        had_index: target.exists(),
    };
    let preparation = (|| {
        if target.exists() {
            fs::copy(&target, dir.join("index.before"))?;
        }
        fs::write(
            dir.join("record.json"),
            serde_json::to_vec_pretty(&journal)?,
        )?;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(e) = preparation {
        drop(lock);
        let _ = fs::remove_file(&lock_path);
        return Err(e);
    }
    let result = (|| {
        anyhow::ensure!(
            head(root) == expected_head && (include_staged || !staged(root, state)?),
            "Git changed while preparing commit; retry after inspecting current files"
        );
        let index = Index::new(root, expected_tree)?;
        let mut cmd = index.command(root);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .args(["commit", "-m", message]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        let pid = child.id();
        crate::project::register_check(pid as i32);
        let out = child.wait_with_output();
        crate::project::unregister_check(pid as i32);
        checked(out?)?;
        Ok(head(root))
    })();
    drop(lock);
    #[cfg(not(unix))]
    {
        let _ = fs::remove_file(&lock_path);
    }
    let recovery = recover_promotion(root);
    recovery?;
    result.context("Normal commit deferred; recovery autosave retained")
}

pub fn changed_paths(root: &Path, state: &Path, before: &str, after: &str) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args([
            "diff",
            "--relative",
            "--name-only",
            "-z",
            before,
            after,
            "--",
        ])
        .args(paths(root, Some(state))?);
    let output = cmd.output()?;
    anyhow::ensure!(
        output.status.success(),
        "Previous content identity unavailable; re-read current files"
    );
    Ok(String::from_utf8(output.stdout)?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Keep local runtime files out of ordinary human Git staging too.
pub fn ignore_runtime(root: &Path, state: &Path) -> Result<()> {
    use std::io::Write;
    let prefix = project::git(root, &["rev-parse", "--show-prefix"])?;
    let exclude = PathBuf::from(project::git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    )?);
    if let Some(parent) = exclude.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut names = vec![
        format!("{prefix}.chuggin/"),
        format!("{prefix}chuggin.json"),
        ".chuggin-save-*".into(),
    ];
    if let Ok(relative) = state.strip_prefix(root)
        && !relative.as_os_str().is_empty()
    {
        names.push(format!("{prefix}{}/", relative.to_string_lossy()));
    }
    let old = fs::read_to_string(&exclude).unwrap_or_default();
    let mut added = String::new();
    for name in names {
        let mut pattern = String::from("/");
        for c in name.chars() {
            if matches!(c, '*' | '?' | '[' | ']' | '\\' | ' ') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        if !old.lines().any(|line| line == pattern) && !added.lines().any(|line| line == pattern) {
            added.push_str(&pattern);
            added.push('\n');
        }
    }
    if !added.is_empty() {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(exclude)?;
        file.write_all(format!("\n# Chuggin local runtime files\n{added}").as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for args in [
            &["init"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.com"],
        ] {
            project::git(d.path(), args).unwrap();
        }
        fs::write(d.path().join("file"), "original").unwrap();
        project::git(d.path(), &["add", "."]).unwrap();
        project::git(d.path(), &["commit", "-m", "initial"]).unwrap();
        d
    }
    #[test]
    fn autosaves_preserve_partial_staging_and_branch() {
        let d = repo();
        let p = d.path();
        let original = head(p);
        fs::write(p.join("file"), "staged").unwrap();
        project::git(p, &["add", "file"]).unwrap();
        fs::write(p.join("file"), "unstaged").unwrap();
        let index = fs::read(git_dir(p).unwrap().join("index")).unwrap();
        let (saved, changed) = autosave(p, &p.join(".chuggin"), "test", "recovery").unwrap();
        assert!(changed);
        assert_eq!(head(p), original);
        assert_eq!(
            project::git(p, &["show", &format!("{saved}:file")]).unwrap(),
            "unstaged"
        );
        assert_eq!(fs::read(git_dir(p).unwrap().join("index")).unwrap(), index);
        assert!(!autosave(p, &p.join(".chuggin"), "test", "again").unwrap().1);
        assert!(
            promote(
                p,
                &p.join(".chuggin"),
                &original,
                &tree(p, None).unwrap(),
                "task"
            )
            .is_err()
        );
        assert_eq!(head(p), original);
        commit_current(
            p,
            &p.join(".chuggin"),
            &original,
            &tree(p, None).unwrap(),
            "User requested all current work",
        )
        .unwrap();
        assert_eq!(project::git(p, &["show", "HEAD:file"]).unwrap(), "unstaged");
        assert!(
            project::git(p, &["status", "--porcelain"])
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn task_commit_preserves_other_staging_and_respects_failing_hooks() {
        let d = repo();
        let p = d.path();
        fs::create_dir(p.join("project")).unwrap();
        fs::write(p.join("project/new"), "project").unwrap();
        fs::write(p.join("file"), "human").unwrap();
        project::git(p, &["add", "file"]).unwrap();
        let sub = p.join("project");
        let old = head(p);
        let content = tree(&sub, None).unwrap();
        promote(&sub, &sub.join(".chuggin"), &old, &content, "Add project").unwrap();
        assert_eq!(project::git(p, &["show", "HEAD:file"]).unwrap(), "original");
        assert_eq!(project::git(p, &["show", ":file"]).unwrap(), "human");
        assert_eq!(
            project::git(&sub, &["diff", "--cached", "--name-only", "--", "."]).unwrap(),
            ""
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = git_dir(p).unwrap().join("hooks/pre-commit");
            fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(sub.join("new"), "next").unwrap();
            let old = head(p);
            assert!(
                promote(
                    &sub,
                    &sub.join(".chuggin"),
                    &old,
                    &tree(&sub, None).unwrap(),
                    "Next"
                )
                .is_err()
            );
            assert_eq!(head(p), old);
            assert_eq!(fs::read_to_string(sub.join("new")).unwrap(), "next");
            assert!(!git_dir(p).unwrap().join("index.lock").exists());
        }
    }
    #[test]
    fn task_commit_recovers_after_git_cleans_message_whitespace() {
        let d = repo();
        let p = d.path();
        fs::write(p.join("file"), "completed").unwrap();
        let message = "Finish task  \n\n\nVerified files; \n[truncated]\t\n";
        let committed = promote(
            p,
            &p.join(".chuggin"),
            &head(p),
            &tree(p, None).unwrap(),
            message,
        )
        .unwrap();
        assert_eq!(head(p), committed);
        assert_eq!(
            project::git(p, &["show", "-s", "--format=%B", "HEAD"]).unwrap(),
            "Finish task\n\nVerified files;\n[truncated]"
        );
        assert!(
            project::git(p, &["status", "--porcelain"])
                .unwrap()
                .is_empty()
        );
        assert!(!git_dir(p).unwrap().join("chuggin-promotion").exists());
        assert!(!git_dir(p).unwrap().join("index.lock").exists());
        recover_promotion(p).unwrap();
    }
    fn pending_commit(p: &Path, message: &str) -> PathBuf {
        let dir = git_dir(p).unwrap().join("chuggin-promotion");
        fs::create_dir(&dir).unwrap();
        fs::copy(git_dir(p).unwrap().join("index"), dir.join("index.before")).unwrap();
        let journal = Promotion {
            root: fs::canonicalize(p).unwrap(),
            state: p.join(".chuggin"),
            old_head: head(p),
            tree: tree(p, None).unwrap(),
            message: message.into(),
            lock_id: 0,
            had_index: true,
        };
        fs::write(
            dir.join("record.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        Index::new(p, &journal.tree)
            .unwrap()
            .git(p, &["commit", "-m", message])
            .unwrap();
        dir
    }
    #[test]
    fn legacy_commit_recovery_preserves_later_work_and_other_staging() {
        let d = repo();
        let p = d.path();
        fs::create_dir(p.join("project")).unwrap();
        fs::write(p.join("project/new"), "completed").unwrap();
        fs::write(p.join("file"), "human staging").unwrap();
        project::git(p, &["add", "file"]).unwrap();
        let sub = p.join("project");
        let dir = pending_commit(&sub, "Finish task\n\nEvidence; \n[truncated]");
        let committed = head(p);
        fs::write(sub.join("new"), "later work").unwrap();
        recover_promotion(&sub).unwrap();
        assert_eq!(head(p), committed);
        assert_eq!(fs::read_to_string(sub.join("new")).unwrap(), "later work");
        assert_eq!(
            project::git(p, &["show", ":project/new"]).unwrap(),
            "completed"
        );
        assert_eq!(
            project::git(p, &["show", ":file"]).unwrap(),
            "human staging"
        );
        assert!(!dir.exists());
        recover_promotion(&sub).unwrap();
    }
    #[test]
    fn recovery_rejects_substantive_message_changes_and_unrelated_commits() {
        for unrelated in [false, true] {
            let d = repo();
            let p = d.path();
            fs::write(p.join("file"), "completed").unwrap();
            let dir = pending_commit(p, "Finish task\n\nEvidence; \n[truncated]");
            let record = dir.join("record.json");
            if unrelated {
                project::git(p, &["commit", "--allow-empty", "-m", "Unrelated commit"]).unwrap();
            } else {
                let mut journal: Promotion =
                    serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
                journal.message = "Finish task\n\nDifferent evidence".into();
                fs::write(&record, serde_json::to_vec(&journal).unwrap()).unwrap();
            }
            let committed = head(p);
            let index = fs::read(git_dir(p).unwrap().join("index")).unwrap();
            let error = recover_promotion(p).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("HEAD changed during commit recovery")
            );
            assert_eq!(head(p), committed);
            assert_eq!(fs::read(git_dir(p).unwrap().join("index")).unwrap(), index);
            assert!(record.exists());
        }
    }
}
