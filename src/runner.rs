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
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,
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
#[derive(Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct Task {
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
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Conversation {
    messages: Vec<Value>,
    tools: Value,
    note: RepairNote,
    context_pressure: bool,
    response_errors: u32,
}

/// Compact evidence accompanies the transcript and survives conversation handoffs.
#[derive(Default, Serialize, Deserialize)]
struct RepairNote {
    model_note: String,
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
        anyhow::ensure!(
            note.len() <= 1600,
            "Keep the progress note within 1600 bytes"
        );
        self.model_note = note.trim().into();
        Ok(())
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
    fn notes_are_replaced_bounded_and_clearable() {
        let mut note = RepairNote::default();
        note.set_note("Previous observation").unwrap();
        assert!(note.set_note(&"x".repeat(1601)).is_err());
        assert_eq!(note.model_note, "Previous observation");
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
fn checks(
    c: &Config,
    workspace: &Path,
    art: &Path,
    label: &str,
    stop: &AtomicBool,
) -> Result<Vec<CheckResult>> {
    let mut results: Vec<CheckResult> = c
        .checks
        .iter()
        .enumerate()
        .map(|(i, x)| project::check(workspace, x, &art.join(format!("{label}-{i}.log")), stop))
        .collect::<Result<_>>()?;
    for result in &mut results {
        if workspace.join("Cargo.toml").exists()
            && result.output.contains("running 0 tests")
            && !result.output.lines().any(|line| {
                line.strip_prefix("running ")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| n > 0)
            })
        {
            result.output.push_str("\nChuggin: ZERO tests executed. New .rs files are not compiled automatically. Wire modules from src/lib.rs (pub mod ...) or src/main.rs (mod ...), then run checks again. A green empty suite does not verify new source files.\n");
        }
    }
    Ok(results)
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
            s.schema_version <= 2,
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
            schema_version: 2,
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
    save(
        &art.join(format!(
            "conversation-before-refresh-{}.json",
            session.messages.len()
        )),
        session,
    )?;
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
    session.messages.push(json!({"role":"user","content":json!({"reason":reason,"current_task":s.current_task,"working_checkpoint":s.working_ref,"feedback":s.feedback,"progress_note":session.note,"instruction":"Earlier history was archived. Continue with the existing working files; inspect them as needed. Do not rebuild completed work."}).to_string()}));
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
fn inspect_tool(
    root: &Path,
    name: &str,
    args: &Value,
    research: &mut crate::web_tools::Research,
) -> Result<String> {
    match name {
        "project_map" => Ok(crate::code_index::index(root)?.to_string()),
        "lookup_symbol" => Ok(crate::symbols::lookup(root, args)?.to_string()),
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
struct WorkResult {
    finish_requested: bool,
    summary: String,
}
fn work(
    c: &Config,
    s: &mut State,
    m: &Model,
    session: &mut Conversation,
    art: &Path,
    stop: &AtomicBool,
) -> Result<WorkResult> {
    crate::events::send(crate::events::Event::Phase("Orient".into()));
    session.messages.push(json!({"role":"user","content":json!({"cycle":s.cycle,"current_task":s.current_task,"previous_feedback":s.feedback,"working_checkpoint":s.working_ref,"instruction":"Continue this project from its current files and the conversation above. Address unresolved failures before expanding unrelated work. Use set_task to record or revise the next useful task. This cycle ends with a checkpoint; unfinished work is kept."}).to_string()}));
    save_conversation(c, session)?;
    let mut research = crate::web_tools::Research::default();
    let mut finish_requested = false;
    let mut summary = String::new();
    for step in 0..c.implementation_calls {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "Stopped by operator");
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
            let result: Result<String> = (|| match name {
                "set_task" => {
                    let task: Task = serde_json::from_value(args.clone())?;
                    anyhow::ensure!(!task.title.trim().is_empty(), "Task needs a title");
                    s.current_task = Some(task);
                    finish_requested = false;
                    emit(art, "task", s.current_task.as_ref().unwrap())?;
                    save(&c.state_dir.join("state.json"), s)?;
                    Ok("Task recorded. All existing working files remain available; these paths and criteria are planning notes, not edit restrictions.".into())
                }
                "finish_task" => {
                    summary = args["summary"]
                        .as_str()
                        .context("Missing summary")?
                        .to_owned();
                    finish_requested = true;
                    Ok("Completion intent recorded. The harness will check and save this checkpoint, retaining any unresolved failures for continued repair.".into())
                }
                "save_progress_note" => {
                    session
                        .note
                        .set_note(args["note"].as_str().context("Missing note")?)?;
                    Ok("Progress note saved; verify against current files and checks.".into())
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
                    project::write(
                        &root,
                        path,
                        args["content"].as_str().context("Missing content")?,
                    )?;
                    Ok(format!("Wrote {path}"))
                }
                "run_checks" => {
                    crate::events::send(crate::events::Event::Phase("Check".into()));
                    let results = checks(c, &root, art, &format!("tool-{step}-{index}"), stop);
                    crate::events::send(crate::events::Event::ValidationDone {
                        passed: results
                            .as_ref()
                            .is_ok_and(|r| !r.is_empty() && r.iter().all(|check| check.passed)),
                        checkpoint: None,
                    });
                    Ok(serde_json::to_string(&results?)?)
                }
                "run_command" => {
                    Ok(
                        crate::dev_tools::run(&root, art, &format!("{step}-{index}"), args, stop)?
                            .to_string(),
                    )
                }
                "read_command_log" => Ok(crate::dev_tools::read_log(art, args)?.to_string()),
                "compiler_diagnostics" => Ok(crate::dev_tools::diagnostics(
                    &root,
                    art,
                    &format!("{step}-{index}"),
                    stop,
                )?
                .to_string()),
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
            })();
            let value = match result {
                Ok(value) => json!({"ok":true,"result":project::excerpt(&value,12000)}),
                Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
            };
            session
                .note
                .record(name, &value.to_string(), value["ok"] == false);
            emit(art, &format!("tool-{step}-{index}"), &value)?;
            emit(art, "repair-note", &session.note)?;
            let mut reply = json!({"role":"tool","tool_name":name,"content":value.to_string()});
            if let Some(id) = call.get("id") {
                reply["tool_call_id"] = id.clone();
            }
            session.messages.push(reply);
            save_conversation(c, session)?;
        }
        if finish_requested {
            break;
        }
    }
    Ok(WorkResult {
        finish_requested,
        summary,
    })
}

pub fn run(path: &Path, count: Option<u64>, stop: Arc<AtomicBool>) -> Result<()> {
    let c = load(path)?;
    crate::setup::ensure_git_identity(&c.repo)?;
    let _lock = lock_project(&c)?;
    let mut state = prepare_state(&c)?;
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
        let result = work(&c, &mut state, &model, &mut session, &art, &stage_stop);
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
        let validation = checks(
            &c,
            &state.working_workspace,
            &art,
            "verification",
            &stage_stop,
        );
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
        let finished = result.as_ref().is_ok_and(|r| r.finish_requested) && passed;
        if finished {
            state.current_task = None;
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
            &json!({"commit":state.working_ref,"changed":changed,"checks_passed":passed,"checked_tree":before_check,"saved_tree":after_check,"task_complete":finished,"last_checks_passed_ref":state.last_checks_passed_ref}),
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
