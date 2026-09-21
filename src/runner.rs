use crate::{
    model::Model,
    project::{self, Check, CheckResult},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub repo: PathBuf,
    pub goal: String,
    pub ollama_url: String,
    pub model: String,
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub implementation_calls: u32,
    pub checks: Vec<Check>,
    pub state_dir: PathBuf,
    pub retry_seconds: u64,
    #[serde(default)]
    pub run_duration_seconds: u64,
    #[serde(default)]
    pub allow_goal_completion: bool,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,
    #[serde(default = "default_command_review")]
    pub command_review_seconds: u64,
}
pub fn default_command_review() -> u64 {
    120
}
pub fn default_request_timeout() -> u64 {
    1800
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    schema_version: u32,
    run_id: String,
    goal: String,
    repo: PathBuf,
    cycle: u64,
    #[serde(alias = "accepted_ref")]
    working_ref: String,
    #[serde(alias = "accepted_branch")]
    working_branch: String,
    #[serde(alias = "accepted_workspace")]
    working_workspace: PathBuf,
    last_checks_passed_ref: Option<String>,
    current_task: Option<Task>,
    task_serial: u64,
    completed_tasks: Vec<CompletedTask>,
    feedback: String,
    seed_from_repo: bool,
    recent: Vec<Outcome>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Outcome {
    cycle: u64,
    task: String,
    disposition: String,
    evidence: String,
    artifact_dir: PathBuf,
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct Task {
    title: String,
    #[serde(default)]
    objective: String,
    #[serde(default)]
    acceptance: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    out_of_scope: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct CompletedTask {
    id: u64,
    title: String,
    summary: String,
    checkpoint: String,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Conversation {
    messages: Vec<Value>,
    tools: Value,
    note: RepairNote,
    context_pressure: bool,
    response_errors: u32,
    prompt_version: String,
    action_watch: crate::action_watch::ActionWatch,
    command_watch: crate::command_watch::CommandWatch,
    nudge_revision: Option<u64>,
    delivered_nudge_id: Option<u64>,
}

/// Compact evidence accompanies the transcript and survives conversation handoffs.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RepairNote {
    model_note: String,
    task_id: Option<u64>,
    recent_actions: Vec<String>,
    last_failure: String,
}
impl RepairNote {
    fn record(&mut self, action: &str, result: &str, failed: bool) {
        let entry = project::excerpt(&format!("{action}: {result}"), 700);
        if failed {
            self.last_failure = entry.clone();
        }
        self.recent_actions.push(entry);
        if self.recent_actions.len() > 4 {
            self.recent_actions.remove(0);
        }
    }
    fn set_note(&mut self, note: &str) -> Result<()> {
        self.model_note = note.trim().into();
        self.task_id = None;
        Ok(())
    }
    fn context(&self) -> Value {
        json!({"model_note_excerpt":project::excerpt(&self.model_note,6000),"model_note_bytes":self.model_note.len(),"note_task_id":self.task_id,"instruction":"Use read_progress_note to retrieve the complete note. A missing or different note_task_id means the note is not tied to the current task. Notes and past failures may be stale; verify against current files.","recent_actions":self.recent_actions,"last_observed_failure":self.last_failure})
    }
    fn page(&self, offset: usize) -> Result<Value> {
        let text = &self.model_note;
        anyhow::ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "offset must be a UTF-8 byte boundary within the note; use next_offset from the previous page"
        );
        let mut end = offset.saturating_add(6000).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(
            json!({"text":&text[offset..end],"note_task_id":self.task_id,"offset":offset,"total_bytes":text.len(),"next_offset":if end < text.len() {Some(end)} else {None}}),
        )
    }
}

#[cfg(test)]
mod repair_note_tests {
    use super::*;

    #[test]
    fn failed_attempt_survives_later_actions_and_disk_round_trip() {
        let mut note = RepairNote::default();
        note.set_note("Keep the existing column names; fix the new formula instead.")
            .unwrap();
        note.record("validate report", "unknown column: Revenue", true);
        for n in 0..20 {
            note.record("edit report", &format!("attempt {n}"), false);
        }
        let saved = serde_json::to_vec(&note).unwrap();
        let restored: RepairNote = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored.recent_actions.len(), 4);
        assert!(restored.last_failure.contains("unknown column"));
        assert!(restored.model_note.contains("existing column names"));
        assert!(
            !restored
                .recent_actions
                .iter()
                .any(|v| v.ends_with("attempt 0"))
        );
    }

    #[test]
    fn notes_are_preserved_paginated_and_clearable() {
        let mut note = RepairNote::default();
        note.set_note("Previous observation").unwrap();
        let long = "界".repeat(5000);
        note.set_note(&long).unwrap();
        let mut restored = String::new();
        let mut offset = 0;
        loop {
            let page = note.page(offset).unwrap();
            restored.push_str(page["text"].as_str().unwrap());
            let Some(next) = page["next_offset"].as_u64() else {
                break;
            };
            offset = next as usize;
        }
        assert_eq!(restored, long);
        assert!(note.context().to_string().len() < long.len());
        note.set_note("New observation").unwrap();
        note.record("check", &"界".repeat(2000), true);
        assert!(note.last_failure.len() < 750);
        note.set_note("").unwrap();
        assert!(note.model_note.is_empty());
        assert!(RepairNote::default().last_failure.is_empty());
    }
}
fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
pub fn load(path: &Path) -> Result<Config> {
    let mut merged = serde_json::to_value(crate::setup::settings()?)?;
    let project: Value = serde_json::from_slice(&fs::read(path)?)?;
    let fields = project
        .as_object()
        .context("Project configuration must be an object")?;
    for (key, value) in fields {
        merged[key] = value.clone();
    }
    let mut c: Config = serde_json::from_value(merged)?;
    let parent = fs::canonicalize(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    if c.repo.is_relative() {
        c.repo = parent.join(&c.repo);
    }
    if c.state_dir.is_relative() {
        c.state_dir = parent.join(&c.state_dir);
    }
    anyhow::ensure!(
        !c.model.trim().is_empty(),
        "Choose an Ollama model from the home menu before starting a run"
    );
    anyhow::ensure!(!c.goal.trim().is_empty(), "Goal must not be empty");
    anyhow::ensure!(
        !c.checks.is_empty(),
        "Configure at least one real validation command"
    );
    anyhow::ensure!(
        (4096..=262144).contains(&c.context_tokens)
            && c.output_tokens > 0
            && c.output_tokens < c.context_tokens,
        "Invalid context/output token limits"
    );
    anyhow::ensure!(
        (1..=100).contains(&c.implementation_calls),
        "implementation_calls must be 1..100"
    );
    anyhow::ensure!(c.retry_seconds >= 1, "retry_seconds must be positive");
    anyhow::ensure!(
        (1..=86400).contains(&c.command_review_seconds),
        "Command review interval must be 1–86400 seconds"
    );
    for check in &c.checks {
        anyhow::ensure!(
            !check.argv.is_empty() && check.timeout_seconds > 0,
            "Checks need argv and a positive timeout"
        );
    }
    Ok(c)
}
pub fn init(path: &Path, repo: &Path, goal: &str) -> Result<()> {
    anyhow::ensure!(!path.exists(), "Config already exists: {}", path.display());
    let repo = fs::canonicalize(repo)?;
    let argv: Vec<String> = if repo.join("Cargo.toml").exists() {
        vec!["cargo".into(), "test".into()]
    } else {
        vec!["REPLACE_WITH_YOUR_TEST_COMMAND".into()]
    };
    crate::setup::save(
        path,
        &json!({
            "repo":repo,"goal":goal,"checks":[{"argv":argv,"timeout_seconds":120}],
            "state_dir":".chuggin"
        }),
    )?;
    crate::events::log(format!(
        "Created {}. Review the goal, endpoint, checks, and state_dir before running.",
        path.display()
    ));
    Ok(())
}
pub fn status(path: &Path) -> Result<()> {
    let c = load(path)?;
    let p = c.state_dir.join("state.json");
    if p.exists() {
        crate::events::log(fs::read_to_string(p)?.to_string());
    } else {
        crate::events::log("No run started.".to_string());
    }
    Ok(())
}
fn emit(art: &Path, stage: &str, value: &impl Serialize) -> Result<()> {
    if matches!(stage, "task" | "outcome")
        || stage.starts_with("scope-extension-")
        || stage.starts_with("response-error-")
        || stage.starts_with("patch-error-")
        || stage.starts_with("tool-")
    {
        crate::events::artifact(stage, &serde_json::to_value(value)?);
    }
    save(&art.join(format!("{stage}.json")), value)
}

fn lock_project(c: &Config) -> Result<fs::File> {
    fs::create_dir_all(&c.state_dir)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(c.state_dir.join("run.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Another Chuggin process is already running this project"
        );
    }
    Ok(file)
}
fn verify_workspace(c: &Config, workspace: &Path) -> Result<()> {
    anyhow::ensure!(
        fs::canonicalize(workspace)?.starts_with(fs::canonicalize(&c.state_dir)?),
        "Working workspace must be inside this project's state directory"
    );
    let common = |root: &Path| -> Result<PathBuf> {
        let path = project::git(root, &["rev-parse", "--git-common-dir"])?;
        Ok(fs::canonicalize(root.join(path))?)
    };
    anyhow::ensure!(
        common(workspace)? == common(&c.repo)?,
        "Working workspace belongs to another repository"
    );
    Ok(())
}

fn create_workspace(c: &Config, id: &str, head: &str) -> Result<(PathBuf, String, bool)> {
    let workspace = fs::canonicalize(&c.state_dir)?.join("working");
    if workspace.exists() {
        verify_workspace(c, &workspace)?;
        let branch = project::git(&workspace, &["branch", "--show-current"])?;
        anyhow::ensure!(
            !branch.is_empty(),
            "Existing working checkout has no branch"
        );
        crate::events::log(
            "Recovered the existing working checkout; continuing with its current files.".into(),
        );
        return Ok((workspace, branch, false));
    }
    let branch = format!("codex/chuggin-working-{id}");
    project::git(
        &c.repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            workspace.to_str().context("Non-UTF8 workspace")?,
            head,
        ],
    )?;
    Ok((workspace, branch, true))
}

/// Import all current project files without changing the user's original index,
/// branch or checkout. Preserve deletions, renames, binaries and untracked files.
fn seed_working_files(c: &Config, workspace: &Path) -> Result<()> {
    let mut files = std::collections::BTreeSet::new();
    for root in [&c.repo, &workspace.to_path_buf()] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Cannot list project files for initial checkpoint"
        );
        for path in output.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            files.insert(
                std::str::from_utf8(path)
                    .context("Non-UTF8 project path")?
                    .to_owned(),
            );
        }
    }
    for name in files {
        let path = Path::new(&name);
        if name == "chuggin.json" || path.components().any(|part| !matches!(part, std::path::Component::Normal(n) if n != ".git" && n != ".chuggin")) { continue; }
        let source = c.repo.join(path);
        let target = workspace.join(path);
        // Never follow a changed parent symlink while importing another path.
        if path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
            .any(|parent| {
                [&c.repo, &workspace.to_path_buf()].iter().any(|root| {
                    fs::symlink_metadata(root.join(parent))
                        .is_ok_and(|m| m.file_type().is_symlink())
                })
            })
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&source).ok();
        // A tracked file may have become a directory (or vice versa).
        if metadata.as_ref().is_some_and(|m| m.is_dir()) && target.is_file() {
            fs::remove_file(&target)?;
        }
        if metadata
            .as_ref()
            .is_some_and(|m| m.is_file() || m.file_type().is_symlink())
            && target.is_dir()
        {
            fs::remove_dir_all(&target)?;
        }
        if let Some(parent) = target.parent() {
            // An earlier imported parent file supersedes old tracked descendants.
            if parent
                .ancestors()
                .take_while(|p| *p != workspace)
                .any(|p| p.is_file())
            {
                continue;
            }
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&target)?;
        }
        match metadata {
            Some(meta) if meta.file_type().is_symlink() => {
                if target.is_file() {
                    fs::remove_file(&target)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(fs::read_link(&source)?, &target)?;
                #[cfg(not(unix))]
                anyhow::bail!("Symlink import is unsupported on this platform");
            }
            Some(meta) if meta.is_file() => {
                fs::copy(&source, &target)?;
            }
            None if target.is_file() => {
                fs::remove_file(&target)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn prepare_state(c: &Config) -> Result<State> {
    let path = c.state_dir.join("state.json");
    if path.exists() {
        let bytes = fs::read(&path)?;
        let mut s: State = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            s.schema_version <= 3,
            "This state needs a newer Chuggin version"
        );
        anyhow::ensure!(
            s.goal == c.goal && s.repo == fs::canonicalize(&c.repo)?,
            "This state belongs to a different goal or repository"
        );
        if s.schema_version < 2 {
            // Preserve the original record before adopting any unfinished work.
            let backup = c.state_dir.join("state-v1-backup.json");
            if !backup.exists() {
                fs::write(&backup, &bytes)?;
            }
            let baseline = s.working_ref.clone();
            if !s.working_branch.is_empty() {
                s.last_checks_passed_ref = Some(baseline.clone());
            }
            let mut dirs: Vec<_> = fs::read_dir(&c.state_dir)?
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
                .map(|e| e.path())
                .collect();
            dirs.sort();
            let mut fallback = None;
            let mut candidate = None;
            for dir in dirs.into_iter().rev() {
                let attempt = fs::read(dir.join("attempt.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let workspace = dir.join("workspace");
                if !attempt.as_ref().is_some_and(|a| a["base"] == baseline)
                    || !workspace.is_dir()
                    || verify_workspace(c, &workspace).is_err()
                {
                    continue;
                }
                if fallback.is_none() {
                    fallback = Some((dir.clone(), workspace.clone()));
                }
                let mut diff = vec!["diff", "--name-only", &baseline, "--"];
                diff.extend_from_slice(PROJECT_PATHS);
                let mut untracked = vec!["ls-files", "--others", "--exclude-standard", "--"];
                untracked.extend_from_slice(PROJECT_PATHS);
                if !project::git(&workspace, &diff)?.is_empty()
                    || !project::git(&workspace, &untracked)?.is_empty()
                {
                    candidate = Some((dir, workspace));
                    break;
                }
            }
            // A newer attempt may have stopped before editing anything. Recover
            // the latest actual work instead of adopting that empty baseline.
            if let Some((dir, workspace)) = candidate.or(fallback) {
                s.working_workspace = workspace;
                s.current_task = fs::read(dir.join("task.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let previous = fs::read(dir.join("outcome.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Outcome>(&b).ok())
                    .map(|o| o.evidence)
                    .unwrap_or_else(|| {
                        "Resuming unfinished work. Inspect the actual files and validate them."
                            .into()
                    });
                s.feedback = format!(
                    "Migrated unfinished work from the earlier runner. Prior rejection and scope rules no longer decide whether work is kept. Inspect these files, run the configured checks, and refine them in place. Historical feedback: {previous}"
                );
                crate::events::log(format!(
                    "Adopted the complete unfinished workspace at {}. Old attempts and history are preserved.",
                    s.working_workspace.display()
                ));
            }
            if fs::canonicalize(&s.working_workspace)? == fs::canonicalize(&c.repo)? {
                let (workspace, branch, seed) = create_workspace(c, &s.run_id, &baseline)?;
                s.working_workspace = workspace;
                s.working_branch = branch;
                s.seed_from_repo = seed;
            }
            verify_workspace(c, &s.working_workspace)?;
            s.working_branch = project::git(&s.working_workspace, &["branch", "--show-current"])?;
            if s.working_branch.is_empty() {
                s.working_branch = format!("codex/chuggin-working-{}", s.run_id);
                project::git(&s.working_workspace, &["switch", "-c", &s.working_branch])?;
            }
            s.working_ref = project::git(&s.working_workspace, &["rev-parse", "HEAD"])?;
            s.schema_version = 2;
            save(&path, &s)?;
        }
        verify_workspace(c, &s.working_workspace)?;
        if s.seed_from_repo {
            seed_working_files(c, &s.working_workspace)?;
            s.seed_from_repo = false;
            save(&path, &s)?;
        }
        // HEAD may have advanced just before an interrupted state save.
        s.working_ref = project::git(&s.working_workspace, &["rev-parse", "HEAD"])?;
        if s.schema_version < 3 {
            let backup = c.state_dir.join("state-before-v3.json");
            if !backup.exists() {
                fs::write(backup, &bytes)?;
            }
            if s.current_task.is_some() {
                s.task_serial = s.task_serial.max(1);
            }
            s.schema_version = 3;
            save(&path, &s)?;
        }
        Ok(s)
    } else {
        let head = project::git(&c.repo, &["rev-parse", "HEAD"])
            .context("Target needs an initial commit before creating its working branch")?;
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        let (workspace, branch, seed) = create_workspace(c, &id, &head)?;
        let working_ref = project::git(&workspace, &["rev-parse", "HEAD"])?;
        let mut state = State {
            schema_version: 3,
            run_id: id,
            goal: c.goal.clone(),
            repo: fs::canonicalize(&c.repo)?,
            working_ref,
            working_branch: branch,
            working_workspace: workspace,
            seed_from_repo: seed,
            ..State::default()
        };
        save(&path, &state)?;
        if seed {
            seed_working_files(c, &state.working_workspace)?;
            state.seed_from_repo = false;
            save(&path, &state)?;
        }
        Ok(state)
    }
}

const PROJECT_PATHS: &[&str] = &[".", ":(exclude).chuggin", ":(exclude)chuggin.json"];
fn stage_project(workspace: &Path) -> Result<()> {
    let mut args = vec!["add", "-A", "--"];
    args.extend_from_slice(PROJECT_PATHS);
    project::git(workspace, &args)?;
    project::git(
        workspace,
        &["reset", "-q", "HEAD", "--", ".chuggin", "chuggin.json"],
    )?;
    Ok(())
}
fn checkpoint(c: &Config, s: &mut State, message: &str) -> Result<bool> {
    stage_project(&s.working_workspace)?;
    // A model may have staged a control file through a command; never checkpoint it.
    project::git(
        &s.working_workspace,
        &["reset", "-q", "HEAD", "--", ".chuggin", "chuggin.json"],
    )?;
    let changed = !project::git(
        &s.working_workspace,
        &["diff", "--cached", "--name-only", "-z"],
    )?
    .is_empty();
    if changed {
        project::git(
            &s.working_workspace,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                message,
            ],
        )?;
    }
    s.working_ref = project::git(&s.working_workspace, &["rev-parse", "HEAD"])?;
    save(&c.state_dir.join("state.json"), s)?;
    Ok(changed)
}
fn working_tree(workspace: &Path) -> Result<String> {
    stage_project(workspace)?;
    project::git(workspace, &["write-tree"])
}
fn save_conversation(c: &Config, session: &Conversation) -> Result<()> {
    save(&c.state_dir.join("conversation.json"), session)
}
fn load_conversation(c: &Config, s: &State) -> Result<Conversation> {
    let path = c.state_dir.join("conversation.json");
    let mut session = if path.exists() {
        serde_json::from_slice::<Conversation>(&fs::read(path)?)?
    } else {
        Conversation::default()
    };
    if session.messages.is_empty() {
        session.messages = vec![
            json!({"role":"system","content":crate::prompts::WORK}),
            json!({"role":"user","content":json!({"main_goal":c.goal,"workspace":s.working_workspace,"configured_checks":c.checks,"instruction":crate::prompts::ORIENT}).to_string()}),
        ];
    }
    if session.prompt_version != crate::prompts::VERSION {
        // Intentional upgrade once on resume; healthy requests retain a stable prefix.
        if session
            .messages
            .first()
            .is_some_and(|m| m["role"] == "system")
        {
            session.messages[0]["content"] = json!(crate::prompts::WORK);
        }
        session.prompt_version = crate::prompts::VERSION.into();
    }
    // Schemas remain identical between calls. Intentional tool configuration changes
    // can update them once when a run starts.
    session.tools = crate::model::tools();
    repair_pending_tools(&mut session.messages);
    save_conversation(c, &session)?;
    Ok(session)
}
fn repair_pending_tools(messages: &mut Vec<Value>) {
    let Some(index) = messages.iter().rposition(|m| m["role"] == "assistant") else {
        return;
    };
    let calls = messages[index]["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let answered = messages[index + 1..]
        .iter()
        .take_while(|m| m["role"] == "tool")
        .count();
    for call in calls.iter().skip(answered) {
        let mut reply = json!({"role":"tool","tool_name":call["function"]["name"],"content":"The process stopped before this tool result was recorded. Execution status is unknown. Inspect current files before deciding whether to retry; no tool has been automatically replayed."});
        if let Some(id) = call.get("id") {
            reply["tool_call_id"] = id.clone();
        }
        messages.push(reply);
    }
}
fn refresh_conversation(
    c: &Config,
    s: &State,
    session: &mut Conversation,
    art: &Path,
    reason: &str,
    keep_recent: bool,
) -> Result<()> {
    // Several recoveries can have identical message counts in one cycle.
    // Never overwrite the evidence of an earlier recovery.
    let mut archive_index = 0;
    let archive = loop {
        let candidate = art.join(format!("conversation-before-refresh-{archive_index}.json"));
        if !candidate.exists() {
            break candidate;
        }
        archive_index += 1;
    };
    save(&archive, session)?;
    let recent = if keep_recent {
        let minimum = session.messages.len().saturating_sub(12).max(2);
        session
            .messages
            .iter()
            .enumerate()
            .skip(minimum)
            .find(|(_, m)| m["role"] == "assistant")
            .map(|(i, _)| session.messages[i..].to_vec())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    session.messages.truncate(2);
    session.nudge_revision = None;
    session.delivered_nudge_id = None;
    session.messages.push(json!({"role":"user","content":json!({"reason":reason,"current_task":s.current_task,"task_id":s.task_serial,"last_completed_task":s.completed_tasks.last(),"working_checkpoint":s.working_ref,"feedback":s.feedback,"progress_note":session.note.context(),"instruction":"Earlier history was archived. Continue with existing files. Closed tasks remain closed. If there is no current task, use set_task to select useful work toward the main goal; do not report an old completion again. Research and foundational work are valid. Inspect files and evidence rather than repeating prior narration."}).to_string()}));
    session.messages.extend(recent);
    session.context_pressure = false;
    crate::events::log(format!(
        "Conversation handoff: {reason}. Files and checkpoints are retained."
    ));
    save_conversation(c, session)
}

fn file_target(root: &Path, path: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        path != "chuggin.json",
        "Project settings are managed by the operator"
    );
    project::safe_path(root, path)
}
pub(crate) fn inspect_tool(
    root: &Path,
    name: &str,
    args: &Value,
    research: &mut crate::web_tools::Research,
) -> Result<String> {
    match name {
        "project_map" => Ok(crate::code_index::index(root)?.to_string()),
        "lookup_symbol" => Ok(crate::symbols::lookup(root, args)?.to_string()),
        "read_file" if args.get("byte_offset").is_some() => Ok(project::read_bytes(
            root,
            args["path"].as_str().context("Missing path")?,
            args["byte_offset"]
                .as_u64()
                .context("byte_offset must be nonnegative")?,
        )?
        .to_string()),
        "read_file" => project::read_lines(
            root,
            args["path"].as_str().context("Missing path")?,
            args["start_line"].as_u64().unwrap_or(1) as usize,
            args["line_count"].as_u64().unwrap_or(80) as usize,
        ),
        "list_files" => Ok(project::inventory(root)?.join("\n")),
        "search" => {
            let text = args["text"].as_str().context("Missing search text")?;
            anyhow::ensure!(!text.is_empty(), "Search text is empty");
            let mut output = String::new();
            for file in project::inventory(root)? {
                if let Ok(content) = project::read(root, &file) {
                    for (line, value) in content
                        .lines()
                        .enumerate()
                        .filter(|(_, v)| v.contains(text))
                    {
                        output.push_str(&format!(
                            "{file}:{}: {}\n",
                            line + 1,
                            project::excerpt(value, 400)
                        ));
                        if output.len() > 12000 {
                            return Ok(output);
                        }
                    }
                }
            }
            Ok(output)
        }
        "web_search" | "read_web_page" => research.call(name, args),
        _ => anyhow::bail!("Unknown tool: {name}"),
    }
}
fn recover_commands(
    c: &Config,
    s: &State,
    m: &Model,
    session: &mut Conversation,
    art: &Path,
) -> Result<()> {
    if !session.command_watch.pending_diagnosis {
        return Ok(());
    }
    let current = working_tree(&s.working_workspace).ok();
    let observed = session
        .command_watch
        .recent
        .last()
        .and_then(|v| v["project_tree"].as_str());
    if current.as_deref() != observed || current.is_none() {
        session.command_watch.reset_streak();
        return save_conversation(c, session);
    }
    session.command_watch.diagnoses += 1;
    let id = session.command_watch.diagnoses;
    let input = json!({"goal":c.goal,"current_task":s.current_task,"task_id":s.task_serial,"last_completed_task":s.completed_tasks.last(),"checkpoint":s.working_ref,"observed_project_tree":current,"repetitions":session.command_watch.count,"command":session.command_watch.command,"recent_commands":session.command_watch.recent,"recent_activity_including_inspections":session.command_watch.activity,"latest_check_feedback":project::excerpt(&s.feedback,4000),"progress_note_metadata":{"note_task_id":session.note.task_id,"bytes":session.note.model_note.len(),"instruction":"Use read_progress_note only if needed. It may describe an earlier task; historical fixes are not evidence of progress during these repeated commands."},"instruction":"Determine whether this repeated work is justified. The project snapshot excludes ignored build outputs and cannot establish external side effects. Inspect evidence, then propose a concrete next action. The full main conversation has deliberately not been copied."});
    save_conversation(c, session)?;
    crate::events::send(crate::events::Event::Phase("Diagnose".into()));
    crate::events::log(format!(
        "Assessing repeated commands in a fresh read-only conversation (diagnostic {id})"
    ));
    let mut research = crate::web_tools::Research::default();
    let assessment =
        crate::stall_diagnostic::diagnose(m, input, art, id, |name, args| match name {
            "read_command_log" => Ok(crate::dev_tools::read_log(art, args)?.to_string()),
            "read_progress_note" => {
                let offset = match args.get("offset") {
                    Some(v) => usize::try_from(v.as_u64().context("offset must be nonnegative")?)?,
                    None => 0,
                };
                Ok(session.note.page(offset)?.to_string())
            }
            _ => inspect_tool(&s.working_workspace, name, args, &mut research),
        });
    let mut productive = false;
    let feedback = match assessment {
        Ok(report) => {
            productive = report.verdict == crate::stall_diagnostic::Verdict::Productive;
            emit(art, &format!("command-diagnostic-{id}-report"), &report)?;
            if report.verdict == crate::stall_diagnostic::Verdict::Stalled {
                session.note.recent_actions.clear();
                refresh_conversation(
                    c,
                    s,
                    session,
                    art,
                    "Fresh diagnostic suggests a persistent command loop; repetitive history was archived",
                    false,
                )?;
                session.action_watch.recovered();
                session.command_watch.reset_streak();
            }
            crate::events::log(format!(
                "Command diagnostic: {:?}. {}",
                report.verdict, report.next_action
            ));
            json!({"command_diagnostic":report,"instruction":"This is advisory, not a task completion or approval. Verify its evidence and pursue the proposed investigation or another justified next action. Repeated testing, sampling or polling is allowed when it resolves a concrete uncertainty. All existing work is retained."})
        }
        Err(error) => {
            if error.downcast_ref::<crate::provider::Stopped>().is_some() {
                return Err(error);
            }
            let error = format!("{error:#}");
            emit(art, &format!("command-diagnostic-{id}-error"), &error)?;
            crate::events::log(format!(
                "Command diagnostic unavailable: {error}. Continuing with saved work."
            ));
            json!({"command_diagnostic_error":error,"repetition_evidence":session.command_watch.notice(),"instruction":"Assessment was inconclusive. Inspect relevant files or full logs and state what evidence the next action will obtain. No work has been discarded."})
        }
    };
    session.command_watch.reviewed(productive);
    session
        .messages
        .push(json!({"role":"user","content":feedback.to_string()}));
    save_conversation(c, session)
}

fn sync_nudge(c: &Config, session: &mut Conversation) -> Result<()> {
    let store = crate::nudge::read(&c.state_dir)?;
    if session.nudge_revision == Some(store.revision) {
        return Ok(());
    }
    session.nudge_revision = Some(store.revision);
    session.delivered_nudge_id = store.active.as_ref().map(|n| n.id);
    if store.revision > 0 {
        let content = json!({"priority_update":{"active_nudge":store.active,"latest_closed_nudge":store.history.last().map(|n|json!({"id":n.id,"status":n.status,"summary":project::excerpt(&n.summary,600)})),"instruction":crate::nudge::INSTRUCTION}});
        session
            .messages
            .push(json!({"role":"user","content":content.to_string()}));
    }
    save_conversation(c, session)
}

struct WorkResult {
    finish_requested: Option<u64>,
    project_completion: Option<Value>,
    summary: String,
}
fn work(
    config_path: &Path,
    c: &Config,
    s: &mut State,
    m: &Model,
    session: &mut Conversation,
    art: &Path,
    stop: &AtomicBool,
) -> Result<WorkResult> {
    crate::events::send(crate::events::Event::Phase("Orient".into()));
    session.messages.push(json!({"role":"user","content":json!({"cycle":s.cycle,"current_task":s.current_task,"task_id":s.task_serial,"last_completed_task":s.completed_tasks.last(),"working_checkpoint":s.working_ref,"instruction":if s.current_task.is_some() {"Continue the active task from existing files and the latest check feedback. Investigate and repair unresolved failures. All work is retained."} else {"There is no active task. Previous completions are already saved. Inspect what is needed toward the main goal and use set_task for the next useful task, then work on it. Research and foundational work count; do not report an old task complete again."}}).to_string()}));
    save_conversation(c, session)?;
    let mut research = crate::web_tools::Research::default();
    let mut jobs = crate::command_jobs::Jobs::default();
    let mut finish_requested = None;
    let mut project_completion = None;
    let mut summary = String::new();
    for step in 0..c.implementation_calls {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "Stopped by operator");
        for update in jobs.monitor(c, s.current_task.as_ref(), m, stop)? {
            session.messages.push(
                json!({"role":"user","content":json!({"command_update":update}).to_string()}),
            );
        }
        if !jobs.running() {
            recover_commands(c, s, m, session, art)?;
        }
        if std::mem::take(&mut session.command_watch.notice_pending) {
            session
                .messages
                .push(json!({"role":"user","content":session.command_watch.notice()}));
            save_conversation(c, session)?;
        }
        if session.action_watch.pending_refresh {
            session.note.recent_actions.clear();
            refresh_conversation(
                c,
                s,
                session,
                art,
                "Repeated identical tool actions continued after feedback; repetitive history was archived",
                false,
            )?;
            session.action_watch.recovered();
            save_conversation(c, session)?;
        }
        if session.action_watch.take_notice() {
            session.messages.push(json!({"role":"user","content":"Action-loop recovery: identical tool actions and results repeated without new evidence. Choose a different diagnostic or approach. If no task is active, select new work instead of repeating an old completion. Existing files and results remain available."}));
            save_conversation(c, session)?;
        }
        if session.context_pressure {
            refresh_conversation(
                c,
                s,
                session,
                art,
                "Reported token usage is approaching the configured context capacity",
                true,
            )?;
        }
        sync_nudge(c, session)?;
        let allow_completion = load(config_path)?.allow_goal_completion;
        session.tools = crate::model::tools();
        if allow_completion {
            session
                .tools
                .as_array_mut()
                .unwrap()
                .push(crate::model::goal_completion_tool());
        }
        crate::events::send(crate::events::Event::Phase("Work".into()));
        let response = match m.chat(&session.messages, Some(session.tools.clone()), false) {
            Ok(response) => response,
            Err(error) => {
                if error.downcast_ref::<crate::provider::Stopped>().is_some() {
                    return Err(error);
                }
                session.response_errors += 1;
                emit(
                    art,
                    &format!("response-error-{step}"),
                    &format!("{error:#}"),
                )?;
                session
                    .note
                    .record("model request", &format!("{error:#}"), true);
                session.messages.push(json!({"role":"user","content":format!("The model request failed: {error:#}. Existing work and completed tool results are retained. Continue the current task.")}));
                if session.response_errors == 2 {
                    refresh_conversation(
                        c,
                        s,
                        session,
                        art,
                        "Repeated request recovery failed",
                        false,
                    )?;
                }
                save_conversation(c, session)?;
                if session.response_errors >= 3 {
                    return Err(error);
                }
                continue;
            }
        };
        session.response_errors = 0;
        // Preserve any recovery instructions actually sent, keeping the next
        // request's prefix and conversational state consistent with the model.
        if let Some(used) = m.take_completed_messages() {
            session.messages = used;
        }
        session.context_pressure = m.context_pressure();
        emit(art, &format!("implementation-{step}"), &response)?;
        let calls = response["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(content) = response["content"].as_str() {
            summary = content.to_owned();
        }
        session.messages.push(response);
        save_conversation(c, session)?;
        if calls.is_empty() {
            break;
        }
        for (index, call) in calls.iter().enumerate() {
            let name = call["function"]["name"].as_str().unwrap_or("");
            let args = &call["function"]["arguments"];
            crate::events::send(crate::events::Event::Tool(format!(
                "{name} {}",
                args["path"].as_str().unwrap_or("")
            )));
            let root = s.working_workspace.clone();
            let command = matches!(name, "run_command" | "run_checks" | "compiler_diagnostics");
            let before_command = if command && !jobs.running() {
                working_tree(&root).ok()
            } else {
                None
            };
            let previous_task = s.task_serial;
            let result: Result<String> = (|| {
                anyhow::ensure!(
                    project_completion.is_none(),
                    "Goal completion already reported; remaining actions were not executed"
                );
                if jobs.running()
                    && matches!(
                        name,
                        "run_command"
                            | "run_checks"
                            | "compiler_diagnostics"
                            | "edit_file"
                            | "write_file"
                            | "restore_checkpoint"
                            | "finish_task"
                            | "finish_nudge"
                            | "finish_project"
                            | "set_task"
                    )
                {
                    anyhow::bail!(
                        "A command is still running. Inspect it with command_status, read relevant files, or stop it with an evidence-based reason before editing, launching more work, or completing the task."
                    );
                }
                match name {
                    "set_task" => {
                        let task: Task = serde_json::from_value(args.clone())?;
                        anyhow::ensure!(!task.title.trim().is_empty(), "Task needs a title");
                        if s.current_task.as_ref() != Some(&task) {
                            s.task_serial += 1;
                            s.current_task = Some(task);
                        }
                        finish_requested = None;
                        emit(art, "task", s.current_task.as_ref().unwrap())?;
                        save(&c.state_dir.join("state.json"), s)?;
                        Ok(json!({"task_id":s.task_serial,"message":"Task recorded. Files and criteria are planning hints, not edit restrictions. Existing work is retained."}).to_string())
                    }
                    "finish_project" => {
                        anyhow::ensure!(
                            allow_completion && load(config_path)?.allow_goal_completion,
                            "Goal completion is not enabled by the user"
                        );
                        let summary = args["summary"].as_str().context("Missing summary")?.trim();
                        let evidence = args["evidence"]
                            .as_str()
                            .context("Missing evidence")?
                            .trim();
                        anyhow::ensure!(
                            !summary.is_empty() && !evidence.is_empty(),
                            "Provide a completion summary and verification evidence"
                        );
                        project_completion = Some(json!({"summary":summary,"evidence":evidence}));
                        Ok(
                            "Goal completion reported. Saving the working tree before pausing."
                                .into(),
                        )
                    }
                    "finish_nudge" => {
                        let id = args["nudge_id"].as_u64().context("Missing nudge_id")?;
                        anyhow::ensure!(
                            session.delivered_nudge_id == Some(id),
                            "Only complete the nudge delivered in the latest priority update"
                        );
                        let store = crate::nudge::finish(
                            &c.state_dir,
                            id,
                            args["summary"].as_str().context("Missing summary")?,
                            args["evidence"].as_str().context("Missing evidence")?,
                            &s.working_ref,
                        )?;
                        emit(
                            art,
                            &format!("nudge-completed-{id}"),
                            store.history.last().context("Missing completion")?,
                        )?;
                        crate::events::log(format!(
                            "Nudge #{id} reported complete. Returning to the overall goal."
                        ));
                        Ok(json!({"nudge_completed":id,"message":"Nudge completion recorded with your evidence. The overall goal and current task remain; continue choosing useful work. This is a model report, not independent verification."}).to_string())
                    }
                    "finish_task" => {
                        if s.current_task.is_none() {
                            return Ok(json!({"completion_recorded":false,"last_completed_task":s.completed_tasks.last(),"message":"No task is active. Previous completions are already saved. Use set_task to select the next useful work toward the main goal; this call does not end the work interval."}).to_string());
                        }
                        if let Some(id) = args.get("task_id") {
                            anyhow::ensure!(
                                id.as_u64() == Some(s.task_serial),
                                "Task id is not current. Inspect the current task before reporting completion."
                            );
                        }
                        summary = args["summary"]
                            .as_str()
                            .context("Missing summary")?
                            .to_owned();
                        finish_requested = Some(s.task_serial);
                        Ok("Completion intent recorded. The harness will check and save this checkpoint, retaining any unresolved failures for continued repair.".into())
                    }
                    "save_progress_note" => {
                        let note = args["note"].as_str().context("Missing note")?;
                        let note = if args["append"] == true {
                            format!("{}\n{}", session.note.model_note, note)
                        } else {
                            note.to_owned()
                        };
                        session.note.set_note(&note)?;
                        session.note.task_id = s.current_task.as_ref().map(|_| s.task_serial);
                        Ok("Complete progress note saved. Use read_progress_note to retrieve it; context handoffs carry an excerpt. Verify notes against current files and checks.".into())
                    }
                    "read_progress_note" => {
                        let offset = match args.get("offset") {
                            Some(v) => {
                                usize::try_from(v.as_u64().context("offset must be nonnegative")?)?
                            }
                            None => 0,
                        };
                        Ok(session.note.page(offset)?.to_string())
                    }
                    "edit_file" => {
                        let path = args["path"].as_str().context("Missing path")?;
                        file_target(&root, path)?;
                        project::edit(
                            &root,
                            path,
                            args["old_text"].as_str().context("Missing old_text")?,
                            args["new_text"].as_str().context("Missing new_text")?,
                        )
                    }
                    "write_file" => {
                        let path = args["path"].as_str().context("Missing path")?;
                        file_target(&root, path)?;
                        let changed = project::write(
                            &root,
                            path,
                            args["content"].as_str().context("Missing content")?,
                        )?;
                        Ok(json!({"path":path,"changed":changed,"message":if changed {"File written"} else {"No bytes changed; the file already has this content."}}).to_string())
                    }
                    "run_checks" | "run_command" | "compiler_diagnostics" => {
                        let mut checks = if name == "run_checks" {
                            c.checks.clone()
                        } else if name == "compiler_diagnostics" {
                            vec![Check {
                                argv: vec![
                                    "cargo".into(),
                                    "check".into(),
                                    "--all-targets".into(),
                                    "--message-format=json".into(),
                                ],
                                timeout_seconds: 120,
                            }]
                        } else {
                            let raw = if let Some(text) = args["argv"].as_str() {
                                serde_json::from_str(text)?
                            } else {
                                args["argv"].clone()
                            };
                            let argv: Vec<String> = serde_json::from_value(raw)?;
                            anyhow::ensure!(
                                !argv.is_empty()
                                    && argv.len() <= 128
                                    && argv.iter().all(|v| !v.contains('\0'))
                                    && argv.iter().map(String::len).sum::<usize>() <= 16000,
                                "Invalid command argv"
                            );
                            let seconds = match args.get("timeout_seconds") {
                                Some(v) => {
                                    v.as_u64().context("timeout_seconds must be positive")?
                                }
                                None => 120,
                            };
                            anyhow::ensure!(
                                (1..=86400).contains(&seconds),
                                "timeout_seconds must be 1–86400"
                            );
                            vec![Check {
                                argv,
                                timeout_seconds: seconds,
                            }]
                        };
                        for check in &mut checks {
                            check.timeout_seconds = check
                                .timeout_seconds
                                .min(m.command_review_seconds(c.command_review_seconds));
                        }
                        let job = crate::command_jobs::Job::new(
                            &root,
                            art,
                            &format!("{step}-{index}"),
                            checks,
                            name == "run_checks",
                            name == "compiler_diagnostics",
                        )?;
                        let mut output = jobs.start(job, stop)?;
                        output["arguments_normalized"] = json!(args["argv"].is_string());
                        Ok(output.to_string())
                    }
                    "command_status" | "command_input" | "stop_command" => {
                        let job =
                            jobs.get(args["command_id"].as_str().context("Missing command_id")?)?;
                        if name == "stop_command" {
                            let reason = args["reason"]
                                .as_str()
                                .filter(|s| !s.trim().is_empty())
                                .context("Explain why the command should stop")?;
                            job.terminate(reason)?;
                        } else if name == "command_input" {
                            let written = job.input(args)?;
                            let mut output = job.poll(0, stop)?;
                            output["input_bytes_written"] = json!(written);
                            output["input_instruction"] = json!(
                                "If fewer bytes were written than supplied, retry only the remaining bytes; stdin closes only after all supplied bytes are written."
                            );
                            return Ok(output.to_string());
                        }
                        Ok(job
                            .poll(args["wait_ms"].as_u64().unwrap_or(1000).min(1000), stop)?
                            .to_string())
                    }
                    "read_command_log" => Ok(crate::dev_tools::read_log(art, args)?.to_string()),
                    "restore_checkpoint" => {
                        let reference = args["commit"].as_str().context("Missing commit")?;
                        let reason = args["reason"]
                            .as_str()
                            .context("Explain why this checkpoint should be restored")?;
                        anyhow::ensure!(
                            !reason.trim().is_empty()
                                && reference.len() >= 7
                                && reference.len() <= 64
                                && reference.bytes().all(|b| b.is_ascii_hexdigit()),
                            "Provide a commit hash and a reason"
                        );
                        project::git(&root, &["merge-base", "--is-ancestor", reference, "HEAD"])?;
                        checkpoint(c, s, "chuggin: save work before requested restoration")?;
                        emit(
                            art,
                            &format!("restore-{step}-{index}"),
                            &json!({"from":s.working_ref,"to":reference,"reason":reason}),
                        )?;
                        let mut command = vec![
                            "restore",
                            "--source",
                            reference,
                            "--staged",
                            "--worktree",
                            "--",
                        ];
                        command.extend_from_slice(PROJECT_PATHS);
                        project::git(&root, &command)?;
                        Ok("Restored the requested checkpoint. Prior work is saved in Git. Inspect the files and run checks again.".into())
                    }
                    _ => inspect_tool(&root, name, args, &mut research),
                }
            })();
            let value = match result {
                Ok(value) => {
                    let value = if matches!(
                        name,
                        "run_command"
                            | "run_checks"
                            | "compiler_diagnostics"
                            | "command_status"
                            | "command_input"
                            | "stop_command"
                    ) || name == "read_progress_note"
                        || (name == "read_file" && args.get("byte_offset").is_some())
                    {
                        value
                    } else {
                        project::excerpt(&value, 12000)
                    };
                    // Keep typed results as objects rather than JSON inside a JSON string.
                    let result = serde_json::from_str::<Value>(&value)
                        .ok()
                        .filter(|v| v.is_object() || v.is_array())
                        .unwrap_or(Value::String(value));
                    json!({"ok":true,"result":result})
                }
                Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
            };
            if command && value["result"]["running"] == true {
                session.command_watch.reset_streak();
            } else if command {
                let after_command = working_tree(&root).ok();
                let observed_args = if name == "run_checks" {
                    json!({"checks":c.checks})
                } else {
                    args.clone()
                };
                session.command_watch.observe(
                    name,
                    &observed_args,
                    &value,
                    before_command.as_deref(),
                    after_command.as_deref(),
                    s.task_serial,
                );
                if session.command_watch.count == 8 || session.command_watch.count == 16 {
                    emit(
                        art,
                        &format!("command-repetition-{step}-{index}"),
                        &session.command_watch,
                    )?;
                    crate::events::log(session.command_watch.notice());
                }
            } else if s.task_serial != previous_task
                || (value["ok"] == true
                    && (value["result"]["changed"] == true || name == "restore_checkpoint"))
            {
                session.command_watch.reset_streak();
            }
            session.command_watch.record_activity(name, args, &value);
            session
                .note
                .record(name, &value.to_string(), value["ok"] == false);
            emit(art, &format!("tool-{step}-{index}"), &value)?;
            emit(art, "repair-note", &session.note)?;
            let reply_value = if command {
                crate::command_watch::compact_reply(&value, session.command_watch.count)
            } else {
                value.clone()
            };
            let mut reply =
                json!({"role":"tool","tool_name":name,"content":reply_value.to_string()});
            if let Some(id) = call.get("id") {
                reply["tool_call_id"] = id.clone();
            }
            session.messages.push(reply);
            if let Some(intervention) = session.action_watch.observe(name, args, &value) {
                let reason = format!(
                    "Repeated tool action: {name}. Identical actions and results have repeated without new evidence. Inspect a different relevant source, change the approach, or select new work if the previous task is closed. Files and completed actions are retained."
                );
                emit(
                    art,
                    &format!("action-recovery-{}", session.action_watch.interventions),
                    &json!({"intervention":format!("{intervention:?}"),"tool":name,"reason":reason}),
                )?;
                crate::events::log(reason);
            }
            save_conversation(c, session)?;
        }
        // Append feedback only after every tool reply, preserving tool protocol order.
        if !session.action_watch.pending_refresh && session.action_watch.take_notice() {
            session.messages.push(json!({"role":"user","content":"Action-loop recovery: the same tool actions returned identical results repeatedly. No new information was obtained. Use a different diagnostic or approach; if no task is active, select new work with set_task. Do not repeat the same completion or no-op edit. Existing files and results remain available."}));
            save_conversation(c, session)?;
        }
        if finish_requested.is_some() || project_completion.is_some() {
            break;
        }
    }
    for update in jobs.drain(c, s.current_task.as_ref(), m, stop)? {
        session.messages.push(
            json!({"role":"user","content":json!({"command_final_result":update}).to_string()}),
        );
    }
    save_conversation(c, session)?;
    Ok(WorkResult {
        finish_requested,
        project_completion,
        summary,
    })
}

pub fn reopen_goal(path: &Path) -> Result<()> {
    let c = load(path)?;
    let _lock = lock_project(&c)?;
    let report = c.state_dir.join("goal-completion.json");
    if report.exists() {
        fs::remove_file(report)?;
    }
    // The full completion report remains in the cycle artifacts.
    Ok(())
}

pub fn run(path: &Path, count: Option<u64>, stop: Arc<AtomicBool>) -> Result<()> {
    let c = load(path)?;
    crate::setup::ensure_git_identity(&c.repo)?;
    let _lock = lock_project(&c)?;
    let mut state = prepare_state(&c)?;
    if c.allow_goal_completion && c.state_dir.join("goal-completion.json").exists() {
        crate::events::log(
            "Model reports project complete. Explicitly resume to reopen work.".into(),
        );
        return Ok(());
    }
    if !c.allow_goal_completion && c.state_dir.join("goal-completion.json").exists() {
        fs::remove_file(c.state_dir.join("goal-completion.json"))?;
    }
    let mut session = load_conversation(&c, &state)?;
    let started = Instant::now();
    let time_up = || {
        let limit = fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["run_duration_seconds"].as_u64())
            .unwrap_or(c.run_duration_seconds);
        limit > 0 && started.elapsed().as_secs() >= limit
    };
    // Soft stops finish the current batch and checkpoint. Force-stop remains the
    // main process's signal handler; pending tools are reconciled on resume.
    let stage_stop = Arc::new(AtomicBool::new(false));
    let mut model = Model::new(
        &c.ollama_url,
        &c.model,
        c.context_tokens,
        c.output_tokens,
        stage_stop.clone(),
    )?;
    model.use_project_settings(path);
    model.use_run_controls(stop.clone(), started);
    let mut completed = 0;
    while !stop.load(Ordering::SeqCst) && !time_up() && count.is_none_or(|n| completed < n) {
        state.cycle += 1;
        while c
            .state_dir
            .join(format!("cycle-{:06}", state.cycle))
            .exists()
        {
            state.cycle += 1;
        }
        completed += 1;
        let art = c.state_dir.join(format!("cycle-{:06}", state.cycle));
        fs::create_dir(&art)?;
        save(&c.state_dir.join("state.json"), &state)?;
        model.trace_to(&art);
        crate::events::send(crate::events::Event::Cycle(state.cycle));
        if let Some(task) = &state.current_task {
            emit(&art, "task", task)?;
        }
        emit(&art, "configuration", &c)?;
        emit(
            &art,
            "run",
            &json!({"chuggin_version":env!("CARGO_PKG_VERSION"),"started_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"base_commit":state.working_ref,"workspace":state.working_workspace}),
        )?;
        emit(&art, "prompt-version", &crate::prompts::VERSION)?;
        emit(
            &art,
            "attempt",
            &json!({"base":state.working_ref,"branch":state.working_branch,"workspace":state.working_workspace}),
        )?;
        crate::events::log(format!(
            "Cycle {}: continue working → check → checkpoint",
            state.cycle
        ));
        let result = work(
            path,
            &c,
            &mut state,
            &model,
            &mut session,
            &art,
            &stage_stop,
        );
        let provider_stopped = result
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<crate::provider::Stopped>().is_some());
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        if let Some(error) = &error {
            crate::events::log(format!(
                "Work request failed: {error}. Saving existing work."
            ));
        }
        crate::events::send(crate::events::Event::Phase("Check".into()));
        let before_check = working_tree(&state.working_workspace)?;
        let validation = (|| -> Result<Vec<CheckResult>> {
            if c.checks.is_empty() {
                return Ok(Vec::new());
            }
            let mut job = crate::command_jobs::Job::new(
                &state.working_workspace,
                &art,
                "verification",
                c.checks
                    .iter()
                    .cloned()
                    .map(|mut check| {
                        check.timeout_seconds = check
                            .timeout_seconds
                            .min(model.command_review_seconds(c.command_review_seconds));
                        check
                    })
                    .collect(),
                true,
                false,
            )?;
            job.wait(&c, state.current_task.as_ref(), &model, &stage_stop)
        })();
        let after_check = working_tree(&state.working_workspace)?;
        let check_error = validation.as_ref().err().map(|e| format!("{e:#}"));
        let results = validation.unwrap_or_default();
        emit(&art, "verification", &results)?;
        let checked_same_files = before_check == after_check;
        let passed = !results.is_empty() && results.iter().all(|r| r.passed) && checked_same_files;
        crate::events::send(crate::events::Event::Phase("Review".into()));
        let task_title = state
            .current_task
            .as_ref()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "Continue project".into());
        let summary = result
            .as_ref()
            .ok()
            .map(|r| r.summary.as_str())
            .unwrap_or("Model request interrupted");
        let unfinished=results.iter().filter(|r|!r.passed).map(|r|json!({"argv":r.argv,"exit_code":r.exit_code,"timed_out":r.timed_out,"output":r.output})).collect::<Vec<_>>();
        let feedback = json!({"summary":summary,"checks_passed":passed,"check_results":unfinished,"check_error":check_error,"checked_snapshot_unchanged":checked_same_files,"request_error":error,"instruction":crate::prompts::REVIEW});
        state.feedback = feedback.to_string();
        // Save the complete working tree even if requests or checks failed.
        crate::events::send(crate::events::Event::Phase("Checkpoint".into()));
        let message = format!(
            "chuggin: checkpoint {} — {}",
            state.cycle,
            project::excerpt(&task_title, 100)
        );
        let changed = checkpoint(&c, &mut state, &message)?;
        if passed {
            state.last_checks_passed_ref = Some(state.working_ref.clone());
        }
        crate::events::send(crate::events::Event::ValidationDone {
            passed,
            checkpoint: Some(state.working_ref.clone()),
        });
        let finished = result
            .as_ref()
            .is_ok_and(|r| r.finish_requested == Some(state.task_serial))
            && state.current_task.is_some()
            && passed;
        if finished {
            let task = state
                .current_task
                .take()
                .expect("Active task checked above");
            state.completed_tasks.push(CompletedTask {
                id: state.task_serial,
                title: task.title,
                summary: project::excerpt(summary, 4000),
                checkpoint: state.working_ref.clone(),
            });
            if state.completed_tasks.len() > 32 {
                state.completed_tasks.remove(0);
            }
        }
        let disposition = if !changed {
            "unchanged"
        } else if passed {
            "checkpoint"
        } else if results.is_empty() || !checked_same_files {
            "checkpoint/unverified"
        } else {
            "checkpoint/checks-failing"
        };
        let outcome = Outcome {
            cycle: state.cycle,
            task: task_title,
            disposition: disposition.into(),
            evidence: format!(
                "Work retained at {}. {} {}",
                state.working_ref,
                if passed {
                    "Configured checks passed."
                } else {
                    "Validation needs attention."
                },
                state.feedback
            ),
            artifact_dir: art.clone(),
        };
        emit(
            &art,
            "checkpoint",
            &json!({"commit":state.working_ref,"changed":changed,"checks_passed":passed,"checked_tree":before_check,"saved_tree":after_check,"task_complete":finished,"completed_task":if finished {state.completed_tasks.last()} else {None},"action_recovery_interventions":session.action_watch.interventions,"command_diagnostics":session.command_watch.diagnoses,"last_checks_passed_ref":state.last_checks_passed_ref}),
        )?;
        session.messages.push(json!({"role":"user","content":json!({"checkpoint":state.working_ref,"task_complete":finished,"feedback":feedback,"instruction":"This checkpoint is saved, including unfinished changes. Continue from these files. If checks failed, inspect and repair the actual failure; do not recreate the feature from scratch. If the task is complete, select the next useful improvement toward the main goal."}).to_string()}));
        save_conversation(&c, &session)?;
        emit(&art, "outcome", &outcome)?;
        crate::events::log(format!(
            "{}: {}",
            outcome.disposition,
            if passed {
                "Checkpoint saved; configured checks passed"
            } else {
                "Work saved; continue refinement from this checkpoint"
            }
        ));
        state.recent.push(outcome);
        if state.recent.len() > 20 {
            state.recent.remove(0);
        }
        save(&c.state_dir.join("state.json"), &state)?;
        if let Some(mut report) = result
            .as_ref()
            .ok()
            .and_then(|r| r.project_completion.clone())
            && load(path)?.allow_goal_completion
        {
            report["checkpoint"] = json!(state.working_ref);
            report["cycle"] = json!(state.cycle);
            report["checks_passed"] = json!(passed);
            report["check_error"] = json!(check_error);
            report["goal"] = json!(c.goal);
            emit(&art, "goal-completion", &report)?;
            save(&c.state_dir.join("goal-completion.json"), &report)?;
            crate::events::log(
                "Model reports project complete. Work saved; no further cycles scheduled.".into(),
            );
            break;
        }
        if provider_stopped {
            crate::events::send(crate::events::Event::Phase(format!(
                "Paused · {}",
                error.as_deref().unwrap_or("Provider unavailable")
            )));
            break;
        }
        if time_up() || count.is_some_and(|n| completed >= n) {
            break;
        }
        crate::events::send(crate::events::Event::Phase("Between cycles".into()));
        for _ in 0..c.retry_seconds {
            if stop.load(Ordering::SeqCst) || time_up() {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
    if time_up() {
        crate::events::log(
            "Run duration reached; finished the cycle and saved work. Resume starts a new timer."
                .into(),
        );
    }
    save(&c.state_dir.join("state.json"), &state)?;
    crate::events::log(format!(
        "Working branch: {}\nWorking files: {}\nArtifacts: {}",
        state.working_branch,
        state.working_workspace.display(),
        c.state_dir.display()
    ));
    Ok(())
}

#[cfg(test)]
mod conversation_tests {
    use super::*;
    #[test]
    fn interrupted_tool_batches_are_not_replayed() {
        let mut messages = vec![
            json!({"role":"assistant","tool_calls":[{"id":"a","function":{"name":"write_file"}},{"id":"b","function":{"name":"run_command"}}]}),
            json!({"role":"tool","tool_call_id":"a","content":"Wrote file"}),
        ];
        repair_pending_tools(&mut messages);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["tool_call_id"], "b");
        assert!(messages[2]["content"].as_str().unwrap().contains("unknown"));
        repair_pending_tools(&mut messages);
        assert_eq!(messages.len(), 3);
    }
}

#[cfg(test)]
mod stall_replay_tests {
    use super::*;
    #[test]
    #[ignore = "Live read-only diagnostic replay; set CHUGGIN_DIAGNOSTIC_CONFIG, CHUGGIN_DIAGNOSTIC_INPUT and CHUGGIN_DIAGNOSTIC_OUTPUT"]
    fn live_stall_diagnostic_replay() {
        let c = load(Path::new(
            &std::env::var("CHUGGIN_DIAGNOSTIC_CONFIG").unwrap(),
        ))
        .unwrap();
        let input: Value = serde_json::from_slice(
            &fs::read(std::env::var("CHUGGIN_DIAGNOSTIC_INPUT").unwrap()).unwrap(),
        )
        .unwrap();
        let art = PathBuf::from(std::env::var("CHUGGIN_DIAGNOSTIC_OUTPUT").unwrap());
        fs::create_dir_all(&art).unwrap();
        // Read existing state directly: never prepare/migrate/lock or save this project.
        let s: State =
            serde_json::from_slice(&fs::read(c.state_dir.join("state.json")).unwrap()).unwrap();
        let session: Conversation =
            serde_json::from_slice(&fs::read(c.state_dir.join("conversation.json")).unwrap())
                .unwrap();
        let source_art = c.state_dir.join(format!("cycle-{:06}", s.cycle));
        let m = Model::new(
            &c.ollama_url,
            &c.model,
            c.context_tokens,
            2048,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        // No project-settings path: provider retries cannot write into the source project.
        m.trace_to(&art);
        let mut research = crate::web_tools::Research::default();
        let report =
            crate::stall_diagnostic::diagnose(&m, input, &art, 1, |name, args| match name {
                "read_command_log" => {
                    Ok(crate::dev_tools::read_log(&source_art, args)?.to_string())
                }
                "read_progress_note" => Ok(session
                    .note
                    .page(args["offset"].as_u64().unwrap_or(0) as usize)?
                    .to_string()),
                _ => inspect_tool(&s.working_workspace, name, args, &mut research),
            })
            .unwrap();
        fs::write(
            art.join("report.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert_eq!(
            report.verdict,
            crate::stall_diagnostic::Verdict::Stalled,
            "The model did not recognize the recorded command loop"
        );
    }
}
