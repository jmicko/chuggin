//! Previewable checkout migration. The old workspace and both input snapshots remain recoverable.
use crate::{project, workspace};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub phase: String,
    pub branch: String,
    pub root: PathBuf,
    pub source: PathBuf,
    pub prepared: PathBuf,
    pub original_head: String,
    pub source_head: String,
    pub original_tree: String,
    pub source_tree: String,
    pub original_index: String,
    pub original_snapshot: String,
    pub source_snapshot: String,
    pub conflicts: bool,
    #[serde(default)]
    pub target_tree: String,
    #[serde(default)]
    pub target_head: String,
    #[serde(default)]
    pub target_index: String,
}
fn index_tree(root: &Path) -> Result<String> {
    project::git(root, &["write-tree"])
}
fn snapshot(root: &Path, state: &Path, label: &str) -> Result<String> {
    let tree = workspace::tree(root, Some(state))?;
    workspace::commit_tree(root, &tree, &[&workspace::head(root)], label)
}
fn persist(path: &Path, plan: &Plan) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec_pretty(plan)?)?;
    fs::rename(temp, path)?;
    Ok(())
}
pub fn load(state: &Path) -> Result<Plan> {
    Ok(serde_json::from_slice(&fs::read(
        state.join("migration.json"),
    )?)?)
}
pub fn prepare(root: &Path, source: &Path, state: &Path) -> Result<Plan> {
    let record = state.join("migration.json");
    if record.exists() {
        return load(state);
    }
    workspace::check(root, &workspace::branch(root)?)?;
    workspace::check(source, &workspace::branch(source)?)?;
    anyhow::ensure!(
        project::git(root, &["rev-parse", "--show-prefix"])?.is_empty(),
        "Legacy migration requires the repository root"
    );
    let dir = state.join("migration");
    fs::create_dir_all(&dir)?;
    let original_snapshot = snapshot(root, state, "chuggin: visible files before migration")?;
    let source_snapshot = snapshot(source, state, "chuggin: developing files before migration")?;
    let original_index = index_tree(root)?;
    for (path, tree) in [
        (root, original_index.clone()),
        (source, index_tree(source)?),
    ] {
        let saved = workspace::commit_tree(
            root,
            &tree,
            &[&workspace::head(path)],
            "chuggin: migration index backup",
        )?;
        project::git(
            root,
            &[
                "update-ref",
                &format!("refs/chuggin/migration/{saved}"),
                &saved,
            ],
        )?;
    }
    for (_label, oid) in [
        ("original", &original_snapshot),
        ("developing", &source_snapshot),
    ] {
        project::git(
            root,
            &["update-ref", &format!("refs/chuggin/migration/{oid}"), oid],
        )?;
    }
    for name in ["state.json", "conversation.json"] {
        if state.join(name).exists() {
            fs::copy(state.join(name), dir.join(name))?;
        }
    }
    let index_path = workspace::git_dir(root)?.join("index");
    if index_path.exists() {
        fs::copy(index_path, dir.join("original.index"))?;
    }
    let source_index = workspace::git_dir(source)?.join("index");
    if source_index.exists() {
        fs::copy(source_index, dir.join("developing.index"))?;
    }
    let prepared = dir.join("prepared");
    if prepared.exists() {
        anyhow::bail!(
            "An interrupted preparation remains at {}. Original files are intact; inspect it before retrying.",
            prepared.display()
        );
    }
    project::git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            prepared.to_str().context("Non-UTF8 migration path")?,
            &original_snapshot,
        ],
    )?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&prepared)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "merge",
            "--no-commit",
            "--no-ff",
            &source_snapshot,
        ])
        .stdin(Stdio::null())
        .output()?;
    let conflicts = !project::git(&prepared, &["ls-files", "--unmerged"])?.is_empty();
    anyhow::ensure!(
        output.status.success() || conflicts,
        "Cannot prepare migration: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = Plan {
        phase: "prepared".into(),
        branch: workspace::branch(root)?,
        root: root.into(),
        source: source.into(),
        prepared,
        original_head: workspace::head(root),
        source_head: workspace::head(source),
        original_tree: project::git(
            root,
            &["rev-parse", &format!("{original_snapshot}^{{tree}}")],
        )?,
        source_tree: project::git(root, &["rev-parse", &format!("{source_snapshot}^{{tree}}")])?,
        original_index,
        original_snapshot,
        source_snapshot,
        conflicts,
        target_tree: String::new(),
        target_head: String::new(),
        target_index: String::new(),
    };
    persist(&record, &plan)?;
    Ok(plan)
}
pub fn describe(plan: &Plan) -> Result<String> {
    let diff = project::git(&plan.prepared, &["diff", "--stat", &plan.original_snapshot])?;
    let conflicts = project::git(&plan.prepared, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(format!(
        "Move developing work into the visible project\n\nProject: {}\nDeveloping files: {}\nPrepared result: {}\n\n{}\n\n{}\nOriginal files, staging, history, and the old workspace are retained as recovery inputs. No visible files change until you apply.\n",
        plan.root.display(),
        plan.source.display(),
        plan.prepared.display(),
        diff,
        if conflicts.is_empty() {
            "Prepared merge has no unresolved conflicts.".into()
        } else {
            format!(
                "Resolve these conflicts in the prepared result and stage their resolutions before applying:\n{conflicts}"
            )
        }
    ))
}
fn merge_tree(root: &Path, a: &str, b: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-tree", "--write-tree", a, b])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "Staged changes conflict with developing history. Original staging is backed up; choose Apply and clear old staging, or resolve it yourself before retrying."
    );
    Ok(String::from_utf8(out.stdout)?
        .lines()
        .next()
        .context("Missing merge tree")?
        .into())
}
fn apply_patch(root: &Path, old: &str, new: &str) -> Result<()> {
    let patch = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--binary",
            "--full-index",
            old,
            new,
            "--",
            ".",
            ":(exclude).chuggin",
            ":(exclude)chuggin.json",
        ])
        .output()?;
    anyhow::ensure!(patch.status.success(), "Cannot prepare file changes");
    if patch.stdout.is_empty() {
        return Ok(());
    }
    for check in [true, false] {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(root)
            .args(["apply", "--binary", "--whitespace=nowarn"]);
        if check {
            cmd.arg("--check");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(&patch.stdout)?;
        let out = child.wait_with_output()?;
        anyhow::ensure!(
            out.status.success(),
            "Cannot apply migration without overwriting conflicting files (including ignored files): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
fn entries(root: &Path, tree: &str) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-rz", tree])
        .output()?;
    anyhow::ensure!(out.status.success(), "Cannot inspect recovery tree");
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|line| {
            line.iter()
                .position(|b| *b == b'\t')
                .map(|n| (line[n + 1..].to_vec(), line[..n].to_vec()))
        })
        .collect())
}
pub fn apply(state: &Path, clear_staging: bool) -> Result<Plan> {
    let mut plan = load(state)?;
    let root = &plan.root.clone();
    workspace::check(root, &plan.branch)?;
    if plan.phase == "complete" {
        return Ok(plan);
    }
    if plan.phase == "prepared" {
        anyhow::ensure!(
            project::git(&plan.prepared, &["ls-files", "--unmerged"])?.is_empty(),
            "Resolve and stage conflicts in {} first",
            plan.prepared.display()
        );
        anyhow::ensure!(
            workspace::head(root) == plan.original_head
                && workspace::tree(root, Some(state))? == plan.original_tree
                && index_tree(root)? == plan.original_index,
            "Original checkout changed since preview. Preserve your prepared result and refresh the migration preview."
        );
        anyhow::ensure!(
            workspace::head(&plan.source) == plan.source_head
                && workspace::tree(&plan.source, Some(state))? == plan.source_tree,
            "Developing workspace changed since preview. Stop its runner and refresh the migration preview."
        );
        plan.target_tree = workspace::tree(&plan.prepared, None)?;
        plan.target_head = if project::git(
            root,
            &[
                "merge-base",
                "--is-ancestor",
                &plan.original_head,
                &plan.source_head,
            ],
        )
        .is_ok()
        {
            plan.source_head.clone()
        } else if project::git(
            root,
            &[
                "merge-base",
                "--is-ancestor",
                &plan.source_head,
                &plan.original_head,
            ],
        )
        .is_ok()
        {
            plan.original_head.clone()
        } else {
            let tree = merge_tree(root, &plan.original_head, &plan.source_head)
                .unwrap_or_else(|_| plan.target_tree.clone());
            workspace::commit_tree(
                root,
                &tree,
                &[&plan.original_head, &plan.source_head],
                "Integrate preserved Chuggin project history",
            )?
        };
        plan.target_index = if clear_staging {
            project::git(
                root,
                &["rev-parse", &format!("{}^{{tree}}", plan.target_head)],
            )?
        } else {
            let index_commit = workspace::commit_tree(
                root,
                &plan.original_index,
                &[&plan.original_head],
                "chuggin: saved original staging",
            )?;
            merge_tree(root, &index_commit, &plan.target_head)?
        };
        // Validate before journaling the transition. Git apply refuses untracked/ignored collisions.
        for tree in [&plan.target_tree, &plan.target_index] {
            let saved = workspace::commit_tree(
                root,
                tree,
                &[&plan.target_head],
                "chuggin: prepared migration result",
            )?;
            project::git(
                root,
                &[
                    "update-ref",
                    &format!("refs/chuggin/migration/{saved}"),
                    &saved,
                ],
            )?;
        }
        plan.phase = "applying".into();
        persist(&state.join("migration.json"), &plan)?;
    }
    let current = workspace::tree(root, Some(state))?;
    if current != plan.original_tree && current != plan.target_tree {
        // On interrupted application, only complete paths still equal to one of the
        // two journaled versions. Never overwrite a third-party edit after interruption.
        let before = entries(root, &plan.original_tree)?;
        let after = entries(root, &plan.target_tree)?;
        let now = entries(root, &current)?;
        let paths: std::collections::BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .chain(now.keys())
            .collect();
        anyhow::ensure!(
            paths
                .into_iter()
                .all(|p| now.get(p) == before.get(p) || now.get(p) == after.get(p)),
            "Files changed after interrupted migration. Recovery inputs are retained; inspect {} before continuing.",
            state.join("migration").display()
        );
    }
    let actual_head = workspace::head(root);
    anyhow::ensure!(
        actual_head == plan.original_head || actual_head == plan.target_head,
        "HEAD changed during migration; refusing to overwrite it"
    );
    let actual_index = index_tree(root)?;
    anyhow::ensure!(
        actual_index == plan.original_index || actual_index == plan.target_index,
        "Staging changed after migration began; refusing to overwrite your index"
    );
    apply_patch(root, &current, &plan.target_tree)?;
    let index = workspace::Index::new(root, &plan.target_index)?;
    let target = workspace::git_dir(root)?.join("index");
    let lock = target.with_extension("lock");
    let f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .context("Git index busy; resume migration when free")?;
    let result = (|| {
        fs::copy(&index.path, &lock)?;
        project::git(
            root,
            &["update-ref", "HEAD", &plan.target_head, &actual_head],
        )?;
        fs::rename(&lock, &target)?;
        Ok::<_, anyhow::Error>(())
    })();
    drop(f);
    let _ = fs::remove_file(lock);
    result?;
    anyhow::ensure!(
        workspace::tree(root, Some(state))? == plan.target_tree,
        "Migration content verification failed; recovery inputs retained"
    );
    plan.phase = "files-applied".into();
    persist(&state.join("migration.json"), &plan)?;
    Ok(plan)
}
pub fn complete(state: &Path, mut plan: Plan) -> Result<()> {
    plan.phase = "complete".into();
    persist(&state.join("migration.json"), &plan)
}
pub fn restore_files(root: &Path, state: &Path, target: &str) -> Result<()> {
    anyhow::ensure!(
        !workspace::staged(root, state)?,
        "Commit or unstage your project changes before restoring files"
    );
    let before = workspace::tree(root, Some(state))?;
    apply_patch(root, &before, target)
}

pub fn refresh(state: &Path) -> Result<()> {
    let plan = load(state)?;
    anyhow::ensure!(
        plan.phase == "prepared",
        "Finish or recover the interrupted migration before preparing another result"
    );
    let archive = state.join(format!(
        "migration-archive-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&archive)?;
    project::git(
        &plan.root,
        &[
            "worktree",
            "move",
            plan.prepared.to_str().context("Non-UTF8 path")?,
            archive.join("prepared").to_str().context("Non-UTF8 path")?,
        ],
    )?;
    for file in fs::read_dir(state.join("migration"))? {
        let file = file?;
        fs::rename(file.path(), archive.join(file.file_name()))?;
    }
    fs::rename(state.join("migration.json"), archive.join("migration.json"))?;
    Ok(())
}
