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
struct State {
    run_id: String,
    goal: String,
    repo: PathBuf,
    cycle: u64,
    accepted_ref: String,
    accepted_branch: String,
    accepted_workspace: PathBuf,
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
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct Discovery {
    gap: String,
    why_now: String,
    files: Vec<String>,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct Task {
    title: String,
    objective: String,
    #[schemars(length(min = 1, max = 3))]
    acceptance: Vec<String>,
    files: Vec<String>,
    out_of_scope: Vec<String>,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct Review {
    decision: Decision,
    reason: String,
    criteria: Vec<Criterion>,
}
#[derive(Serialize, Deserialize, PartialEq, Debug, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Accept,
    Partial,
    Repair,
    Replan,
    Rollback,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct Criterion {
    criterion: String,
    passed: bool,
    evidence: String,
}

/// Bounded, task-local evidence survives resets without retaining a transcript.
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
fn discovery_input(c: &Config, s: &State) -> Result<Value> {
    let files = project::inventory(&s.accepted_workspace)?;
    let mut seed: Vec<_> = files
        .iter()
        .filter(|p| {
            p.ends_with("README.md")
                || p.ends_with("Cargo.toml")
                || p.ends_with("package.json")
                || p.ends_with("main.rs")
                || p.ends_with("lib.rs")
        })
        .take(8)
        .cloned()
        .collect();
    // Fresh discovery needs implementation evidence, not only entry points.
    // Prioritize the latest accepted files so omitted snippets aren't mistaken
    // for missing code and the planner doesn't rebuild its last milestone.
    let changed = project::git(
        &s.accepted_workspace,
        &["show", "--pretty=", "--name-only", "HEAD"],
    )?;
    for name in changed.lines().chain(files.iter().map(String::as_str)) {
        if !seed.iter().any(|p| p == name) && name.ends_with(".rs") {
            seed.push(name.into());
        }
    }
    let history = project::git(&s.accepted_workspace, &["log", "-30", "--format=%h %s"])?;
    let previous_task = s
        .recent
        .last()
        .filter(|o| o.disposition != "accepted")
        .and_then(|o| fs::read(o.artifact_dir.join("task.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    Ok(
        json!({"accepted_milestones":project::excerpt(&history,2500),"previous_unfinished_task":previous_task.as_ref().and_then(|t|t.get("title")),"main_goal":c.goal,"accepted_commit":s.accepted_ref,"recent_outcomes":recent_evidence(s),"inventory":project::excerpt(&files.join("\n"),6000),"code_index":crate::code_index::index(&s.accepted_workspace)?,"orientation":project::context(&s.accepted_workspace,&seed,4000),"evidence_rules":"Every inventory path exists on disk. Orientation is an excerpt, not the complete project. Do not infer missing files or broken builds from omitted snippets. Identify a genuinely absent behavior beyond the latest accepted milestone."}),
    )
}
fn acceptance_allowed(
    task: &Task,
    review: &Review,
    results: &[CheckResult],
    changed: bool,
) -> bool {
    changed
        && !results.is_empty()
        && results.iter().all(|c| c.passed)
        && review.decision == Decision::Accept
        && task
            .acceptance
            .iter()
            .enumerate()
            .all(|(i, _)| criterion_satisfied(task, review, i))
}
fn criterion_key(text: &str) -> String {
    text.replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn normalize_task(task: &mut Task) {
    let mut seen = std::collections::BTreeSet::new();
    task.acceptance.retain(|s| seen.insert(criterion_key(s)));
    seen.clear();
    task.out_of_scope.retain(|s| seen.insert(criterion_key(s)));
    seen.clear();
    task.files.retain(|s| seen.insert(s.clone()));
}
fn criterion_specs(task: &Task) -> Value {
    json!(
        task.acceptance
            .iter()
            .enumerate()
            .map(|(i, text)| json!({"id":format!("C{}",i+1),"text":text}))
            .collect::<Vec<_>>()
    )
}
fn criterion_satisfied(task: &Task, review: &Review, index: usize) -> bool {
    let id = format!("C{}", index + 1);
    let mut matches = review.criteria.iter().filter(|r| {
        criterion_key(&r.criterion).eq_ignore_ascii_case(&id)
            || criterion_key(&r.criterion) == criterion_key(&task.acceptance[index])
    });
    let Some(result) = matches.next() else {
        return false;
    };
    result.passed && !result.evidence.trim().is_empty() && matches.next().is_none()
}
fn inspect_tool(
    root: &Path,
    name: &str,
    args: &Value,
    research: &mut crate::web_tools::Research,
) -> Result<String> {
    crate::events::send(crate::events::Event::Tool(format!("inspect · {name}")));
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
            let text = args["text"].as_str().context("Missing text")?;
            anyhow::ensure!(!text.is_empty(), "Search text is empty");
            let mut out = String::new();
            for file in project::inventory(root)? {
                if let Ok(s) = project::read(root, &file) {
                    for (i, line) in s.lines().enumerate() {
                        if line.contains(text) {
                            out.push_str(&format!(
                                "{file}:{}: {}\n",
                                i + 1,
                                project::excerpt(line, 300)
                            ));
                            if out.len() > 6000 {
                                return Ok(out);
                            }
                        }
                    }
                }
            }
            Ok(out)
        }
        "web_search" | "read_web_page" => research.call(name, args),
        _ => anyhow::bail!("Planning tools are read-only; use implementation for changes"),
    }
}
fn test_names(results: &[CheckResult]) -> std::collections::BTreeSet<String> {
    results
        .iter()
        .flat_map(|r| r.output.lines())
        .filter_map(|line| {
            line.strip_prefix("test ")
                .and_then(|s| s.strip_suffix(" ... ok"))
                .map(str::to_owned)
        })
        .collect()
}
fn partial_allowed(
    task: &Task,
    review: &Review,
    results: &[CheckResult],
    baseline: &[CheckResult],
) -> bool {
    review.decision == Decision::Partial
        && !results.is_empty()
        && results.iter().all(|r| r.passed)
        && task
            .acceptance
            .iter()
            .enumerate()
            .any(|(i, _)| criterion_satisfied(task, review, i))
        && test_names(baseline).is_subset(&test_names(results))
}
fn validate_task(task: &Task, root: &Path) -> Result<()> {
    anyhow::ensure!(
        !task.objective.trim().is_empty(),
        "Task needs a nonempty objective"
    );
    anyhow::ensure!(
        !task.acceptance.is_empty() && task.acceptance.iter().all(|a| !a.trim().is_empty()),
        "Task needs nonempty acceptance criteria"
    );
    anyhow::ensure!(
        !task.files.is_empty(),
        "Task needs at least one writable file"
    );
    for file in &task.files {
        project::safe_path(root, file).with_context(|| format!("Invalid task path: {file}"))?;
    }
    Ok(())
}
fn path_in_scope(path: &str, task: &Task, root: &Path) -> bool {
    task.files.iter().any(|p| p == path)
        || (Path::new(path)
            .file_name()
            .is_some_and(|p| p == "Cargo.lock")
            && project::safe_path(root, path).is_ok()
            && root.join(path).with_file_name("Cargo.toml").is_file())
}
fn extend_source_access(task: &mut Task, root: &Path, path: &str, reason: &str) -> Result<String> {
    anyhow::ensure!(
        !reason.trim().is_empty(),
        "Explain the dependency change needed for this task"
    );
    anyhow::ensure!(
        !matches!(path, "chuggin.json"),
        "Project control files cannot be added to task scope"
    );
    let content = project::read(root, path)?;
    if !task.files.iter().any(|p| p == path) {
        task.files.push(path.into());
        task.out_of_scope.push(format!("Exception to earlier exclusions: minimal supporting changes in {path} are permitted for this task: {reason}. Preserve existing APIs and tests."));
    }
    Ok(format!(
        "Access granted for minimal supporting changes in {path}.\n{}",
        project::excerpt(&content, 10000)
    ))
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct PatchPlan {
    edits: Vec<Replacement>,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct Replacement {
    path: String,
    old_text: String,
    new_text: String,
}
fn concrete_patch(
    m: &Model,
    task: &Task,
    workspace: &Path,
    results: &[CheckResult],
    art: &Path,
    step: u32,
) -> Result<()> {
    let plan: PatchPlan=m.structured("Produce concrete file edits, not a summary or plan. Return JSON {edits:[{path,old_text,new_text}]}. Make 1-4 small edits that advance the task. If checks fail, fix the first validation failure. Otherwise add the missing behavior and a relevant validation. old_text must match an exact unique existing substring, with no line-number prefixes. Use empty old_text only for a NEW file. Use only the supplied allowed paths. Preserve existing behavior and tests. Do not claim edits were made; the harness will apply these literal replacements and run checks. Choose a complete small patch rather than further inspection.",json!({"task":task,"allowed_paths":task.files,"files":project::context(workspace,&task.files,24000),"actual_checks":results}))?;
    emit(art, &format!("patch-{step}"), &plan)?;
    anyhow::ensure!(
        !plan.edits.is_empty() && plan.edits.len() <= 4,
        "Patch needs 1-4 concrete edits"
    );
    for edit in plan.edits {
        anyhow::ensure!(
            task.files.contains(&edit.path),
            "Patch path outside task: {}",
            edit.path
        );
        if edit.old_text.is_empty() {
            anyhow::ensure!(
                !project::safe_path(workspace, &edit.path)?.exists(),
                "Empty old_text is only for new files"
            );
            project::write(workspace, &edit.path, &edit.new_text)?;
        } else {
            project::edit(workspace, &edit.path, &edit.old_text, &edit.new_text)?;
        }
    }
    Ok(())
}
fn cycle(c: &Config, s: &mut State, m: &Model, art: &Path, stop: &AtomicBool) -> Result<Outcome> {
    crate::events::send(crate::events::Event::Phase("Discovery".into()));
    m.trace_to(art);
    emit(art, "configuration", c)?;
    emit(
        art,
        "run",
        &json!({"chuggin_version":env!("CARGO_PKG_VERSION"),"started_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"base_commit":s.accepted_ref}),
    )?;
    emit(art, "prompt-version", &crate::prompts::VERSION)?;
    let mut research = crate::web_tools::Research::default();
    let recovery = recoverable(s);
    let task = if let Some(prior) = &recovery {
        emit(art, "recovery", prior)?;
        crate::events::log(format!(
            "Recovering useful files from cycle {}",
            prior.cycle
        ));
        serde_json::from_slice::<Task>(&fs::read(prior.artifact_dir.join("task.json"))?)?
    } else {
        let mut inspection_sequence = 0;
        let discovery:Discovery=m.investigate(crate::prompts::DISCOVERY,discovery_input(c,s)?,crate::model::inspection_tools(&s.accepted_workspace),|name,args|{
            let result=inspect_tool(&s.accepted_workspace,name,args,&mut research);
            inspection_sequence += 1;
            emit(art,&format!("inspection-{inspection_sequence:03}"),&json!({"tool":name,"arguments":args,"result":result.as_ref().ok(),"error":result.as_ref().err().map(|e|e.to_string())}))?;
            result
        })?;
        emit(art, "discovery", &discovery)?;
        crate::events::send(crate::events::Event::Phase("Shape".into()));
        let task:Task=m.structured(crate::prompts::SHAPE,json!({"main_goal":c.goal,"code_index":crate::code_index::index(&s.accepted_workspace)?,"discovery":discovery,"files":project::context(&s.accepted_workspace,&discovery.files,10000),"checks":c.checks,"recent_outcomes":recent_evidence(s)}))?;
        task
    };
    let mut task = task;
    emit(art, "task-proposed", &task)?;
    if recovery.is_none() && (task.acceptance.len() > 5 || task.files.len() > 6) {
        task=m.structured(crate::prompts::SHAPE,json!({"instruction":"Narrow this oversized proposal to one complete behavior, not an already-existing declaration. Keep dependency wiring and focused tests together.","proposed_task":task,"code_index":crate::code_index::index(&s.accepted_workspace)?}))?;
    }
    emit(art, "task", &task)?;
    validate_task(&task, &s.accepted_workspace)?;
    if s.accepted_workspace.join("Cargo.toml").exists()
        && task
            .files
            .iter()
            .any(|p| p.starts_with("src/") && p.ends_with(".rs"))
    {
        for path in ["src/lib.rs", "src/main.rs"] {
            if !task.files.iter().any(|p| p == path) {
                task.files.push(path.into());
            }
        }
        let mut wiring = Vec::new();
        for file in &task.files {
            let mut parent = Path::new(file).parent();
            while let Some(dir) = parent.filter(|p| p.starts_with("src") && *p != Path::new("src"))
            {
                for candidate in [dir.with_extension("rs"), dir.join("mod.rs")] {
                    if s.accepted_workspace.join(&candidate).is_file() {
                        wiring.push(candidate.to_string_lossy().into_owned());
                    }
                }
                parent = dir.parent();
            }
        }
        for path in wiring {
            if !task.files.contains(&path) {
                task.files.push(path);
            }
        }
        task.acceptance.push("The new or changed behavior is compiled and exercised by at least one passing focused test; an unreferenced source file is not completion.".into());
        task.out_of_scope.push(
            "Changes to module entry points must be limited to wiring the new code and its tests."
                .into(),
        );
        emit(art, "task", &task)?;
    }
    crate::events::log(format!("Cycle {}: {}", s.cycle, task.title));
    normalize_task(&mut task);
    emit(art, "task", &task)?;
    let branch = format!("codex/chuggin-{}-{}", s.run_id, s.cycle);
    let workspace = art.join("workspace");
    project::git(
        &c.repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            workspace.to_str().context("Non-UTF8 workspace path")?,
            &s.accepted_ref,
        ],
    )?;
    emit(
        art,
        "attempt",
        &json!({"base":s.accepted_ref,"branch":branch,"workspace":workspace}),
    )?;
    let baseline = checks(c, &workspace, art, "baseline", stop)?;
    emit(art, "baseline", &baseline)?;
    let before = project::snapshot(&workspace)?;
    if let Some(prior) = &recovery {
        let source = prior.artifact_dir.join("workspace");
        for path in &task.files {
            if let Ok(content) = project::read(&source, path) {
                project::write(&workspace, path, &content)?;
            }
        }
    }
    let working_checks = if recovery.is_some() {
        checks(c, &workspace, art, "recovery-check", stop)?
    } else {
        baseline.clone()
    };
    crate::events::send(crate::events::Event::Phase("Implement".into()));
    let mut repair_note: RepairNote = recovery
        .as_ref()
        .and_then(|prior| fs::read(prior.artifact_dir.join("repair-note.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut messages = vec![
        json!({"role":"system","content":crate::prompts::IMPLEMENT}),
        json!({"role":"user","content":json!({"main_goal":c.goal,"task":task,"current_checks":working_checks,"repair_note":repair_note,"recovery_origin":recovery.as_ref().map(|o|o.cycle),"instruction":"When checks fail, fix the first validation failure with a targeted edit before doing any broader work. Then validate the requested outcome.","current_files":project::context(&workspace,&task.files,18000)}).to_string()}),
    ];
    let mut notes = String::new();
    let mut last_result = String::new();
    let mut latest_checks = working_checks;
    let mut validated_snapshot = None;
    let mut response_errors = 0;
    let mut observations = 0;
    let mut edited = false;
    for step in 0..c.implementation_calls {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "Stopped by operator");
        if serde_json::to_vec(&messages)?.len()
            > (c.context_tokens.saturating_sub(c.output_tokens + 2048) as usize).min(48000)
        {
            messages.truncate(1);
            messages.push(json!({"role":"user","content":json!({"task":task,"repair_note":repair_note,"current_files":project::context(&workspace,&task.files,12000),"latest_result":last_result,"latest_checks":latest_checks,"instruction":"Fresh implementation session, same task and files. Continue the existing work. Do not re-plan the project. Implement the missing behavior and run checks."}).to_string()}));
            emit(art, &format!("context-refresh-{step}"), &messages)?;
        }
        let response = match m.chat(&messages, Some(crate::model::tools_for(&workspace)), false) {
            Ok(r) => r,
            Err(e) => {
                response_errors += 1;
                notes = format!("Model response failed: {e:#}");
                repair_note.record("model response", &notes, true);
                emit(art, "repair-note", &repair_note)?;
                emit(art, &format!("response-error-{step}"), &notes)?;
                if response_errors >= 3 {
                    break;
                }
                messages.truncate(1);
                messages.push(json!({"role":"user","content":json!({"task":task,"repair_note":repair_note,"current_files":project::context(&workspace,&task.files,12000),"latest_checks":latest_checks,"recovery":"Previous response failed. Work already written remains. Make a small targeted edit now; keep each response short."}).to_string()}));
                continue;
            }
        };
        emit(art, &format!("implementation-{step}"), &response)?;
        let calls = response["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut history_response = response.clone();
        if !calls.is_empty() {
            history_response["content"] = json!(project::excerpt(
                response["content"].as_str().unwrap_or(""),
                1000
            ));
        }
        messages.push(history_response);
        if calls.is_empty() {
            if !edited && latest_checks.iter().any(|r| !r.passed) {
                match concrete_patch(m, &task, &workspace, &latest_checks, art, step) {
                    Ok(()) => {
                        edited = true;
                        latest_checks =
                            checks(c, &workspace, art, &format!("patch-check-{step}"), stop)?;
                        repair_note.record(
                            "applied patch and checked",
                            &serde_json::to_string(&latest_checks)?,
                            latest_checks.iter().any(|r| !r.passed),
                        );
                        emit(art, "repair-note", &repair_note)?;
                    }
                    Err(e) => {
                        repair_note.record("patch attempt", &format!("{e:#}"), true);
                        emit(art, "repair-note", &repair_note)?;
                        emit(art, &format!("patch-error-{step}"), &format!("{e:#}"))?;
                    }
                }
                messages.truncate(1);
                messages.push(json!({"role":"user","content":json!({"task":task,"repair_note":repair_note,"current_files":project::context(&workspace,&task.files,18000),"current_checks":latest_checks,"instruction":"These are the actual files and checks after the patch. Add any missing validation or fix the remaining failure using tools. Do not claim unperformed actions."}).to_string()}));
                continue;
            }
            notes = response["content"].as_str().unwrap_or("").into();
            break;
        }
        anyhow::ensure!(calls.len() <= 8, "Too many sibling tool calls");
        for (index, call) in calls.iter().enumerate() {
            let name = call["function"]["name"].as_str().unwrap_or("");
            let args = &call["function"]["arguments"];
            crate::events::send(crate::events::Event::Tool(format!(
                "{} {}",
                name,
                args["path"].as_str().unwrap_or("")
            )));
            let result: Result<String> = (|| match name {
                "save_progress_note" => {
                    repair_note.set_note(args["note"].as_str().context("Missing note")?)?;
                    Ok("Task-local note saved; verify it against current files and checks.".into())
                }
                "project_map" => Ok(crate::code_index::index(&workspace)?.to_string()),
                "lookup_symbol" => Ok(crate::symbols::lookup(&workspace, args)?.to_string()),
                "run_command" => {
                    let prior = project::snapshot(&workspace)?;
                    let result = crate::dev_tools::run(
                        &workspace,
                        art,
                        &format!("{step}-{index}"),
                        args,
                        stop,
                    )?;
                    if project::snapshot(&workspace)? != prior {
                        edited = true;
                        observations = 0;
                    }
                    Ok(result.to_string())
                }
                "compiler_diagnostics" => Ok(crate::dev_tools::diagnostics(
                    &workspace,
                    art,
                    &format!("{step}-{index}"),
                    stop,
                )?
                .to_string()),
                "read_command_log" => Ok(crate::dev_tools::read_log(art, args)?.to_string()),
                "web_search" | "read_web_page" => research.call(name, args),
                "request_file_access" => {
                    let result = extend_source_access(
                        &mut task,
                        &workspace,
                        args["path"].as_str().context("Missing path")?,
                        args["reason"].as_str().context("Missing reason")?,
                    )?;
                    emit(art, &format!("scope-extension-{step}-{index}"), args)?;
                    emit(art, "task", &task)?;
                    Ok(result)
                }
                "read_file" => project::read_lines(
                    &workspace,
                    args["path"].as_str().context("Missing path")?,
                    args["start_line"].as_u64().unwrap_or(1) as usize,
                    args["line_count"].as_u64().unwrap_or(80) as usize,
                ),
                "list_files" => Ok(project::inventory(&workspace)?.join("\n")),
                "search" => {
                    let needle = args["text"].as_str().context("Missing search text")?;
                    let mut result = String::new();
                    for path in project::inventory(&workspace)? {
                        if let Ok(text) = project::read(&workspace, &path) {
                            for (line, text) in
                                text.lines().enumerate().filter(|(_, t)| t.contains(needle))
                            {
                                result.push_str(&format!(
                                    "{path}:{}: {}\n",
                                    line + 1,
                                    project::excerpt(text, 300)
                                ));
                                if result.len() > 8000 {
                                    return Ok(result);
                                }
                            }
                        }
                    }
                    Ok(result)
                }
                "edit_file" => {
                    let path = args["path"].as_str().context("Missing path")?;
                    anyhow::ensure!(
                        task.files.iter().any(|p| p == path),
                        "Path is outside task specification"
                    );
                    project::edit(
                        &workspace,
                        path,
                        args["old_text"].as_str().context("Missing old_text")?,
                        args["new_text"].as_str().context("Missing new_text")?,
                    )
                }
                "write_file" => {
                    let path = args["path"].as_str().context("Missing path")?;
                    anyhow::ensure!(
                        task.files.iter().any(|p| p == path),
                        "Path is outside task specification; ask next discovery cycle to expand scope"
                    );
                    project::write(
                        &workspace,
                        path,
                        args["content"].as_str().context("Missing content")?,
                    )?;
                    Ok(format!("Wrote {path}"))
                }
                "run_checks" => {
                    latest_checks =
                        checks(c, &workspace, art, &format!("tool-{step}-{index}"), stop)?;
                    validated_snapshot = Some(project::snapshot(&workspace)?);
                    Ok(serde_json::to_string(&latest_checks)?)
                }
                _ => anyhow::bail!("Unknown tool: {name}"),
            })();
            let command_failed = matches!(name, "run_command" | "compiler_diagnostics")
                && result
                    .as_ref()
                    .ok()
                    .and_then(|v| serde_json::from_str::<Value>(v).ok())
                    .is_some_and(|v| {
                        v["passed"] == false
                            || v["timed_out"] == true
                            || v["exit_code"].as_i64().is_some_and(|code| code != 0)
                    });
            let value = match result {
                Ok(v) => json!({"ok":true,"result":project::excerpt(&v,12000)}),
                Err(e) => json!({"ok":false,"error":e.to_string()}),
            };
            emit(art, &format!("tool-{step}-{index}"), &value)?;
            last_result = project::excerpt(&value.to_string(), 4000);
            if matches!(
                name,
                "edit_file" | "write_file" | "run_command" | "run_checks" | "compiler_diagnostics"
            ) || value["ok"] == false
            {
                let failed = value["ok"] == false
                    || command_failed
                    || (name == "run_checks" && latest_checks.iter().any(|r| !r.passed));
                let evidence = if name == "run_checks" && !failed {
                    "Configured checks passed; any earlier failure may now be resolved.".to_string()
                } else if name == "run_checks" {
                    serde_json::to_string(
                        &latest_checks
                            .iter()
                            .filter(|r| !r.passed)
                            .collect::<Vec<_>>(),
                    )?
                } else {
                    value.to_string()
                };
                repair_note.record(
                    &format!(
                        "{name} {}",
                        args["path"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| args
                                .get("argv")
                                .map(Value::to_string)
                                .unwrap_or_default())
                    ),
                    &evidence,
                    failed,
                );
            }
            emit(art, "repair-note", &repair_note)?;
            if (name == "write_file" || name == "edit_file") && value["ok"] == true {
                observations = 0;
                edited = true;
            }
            let mut tool_reply =
                json!({"role":"tool","tool_name":name,"content":value.to_string()});
            if let Some(id) = call.get("id") {
                tool_reply["tool_call_id"] = id.clone();
            }
            messages.push(tool_reply);
        }
        observations += 1;
        if !latest_checks.is_empty()
            && latest_checks.iter().all(|r| r.passed)
            && validated_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot != &before)
            && validated_snapshot == Some(project::snapshot(&workspace)?)
        {
            notes="Implementation changes passed configured validation; handing off to independent verification and review.".into();
            break;
        }
        if observations >= 4 {
            match concrete_patch(m, &task, &workspace, &latest_checks, art, step) {
                Ok(()) => {
                    edited = true;
                    latest_checks =
                        checks(c, &workspace, art, &format!("patch-check-{step}"), stop)?;
                    repair_note.record(
                        "applied patch and checked",
                        &serde_json::to_string(&latest_checks)?,
                        latest_checks.iter().any(|r| !r.passed),
                    );
                    emit(art, "repair-note", &repair_note)?;
                    messages.push(json!({"role":"user","content":json!({"repair_note":repair_note,"applied_patch":true,"current_files":project::context(&workspace,&task.files,16000),"current_checks":latest_checks}).to_string()}));
                }
                Err(e) => {
                    repair_note.record("patch attempt", &format!("{e:#}"), true);
                    emit(art, "repair-note", &repair_note)?;
                    emit(art, &format!("patch-error-{step}"), &format!("{e:#}"))?;
                }
            }
            messages.push(json!({"role":"user","content":"Stop re-exploring unchanged files. If checks pass and the requested behavior is complete, finish with a short summary. Otherwise make the next corrective edit using the source already supplied."}));
            observations = 0;
        }
    }
    emit(art, "implementation-note", &notes)?;
    crate::events::send(crate::events::Event::Phase("Verify".into()));
    let mut result = checks(c, &workspace, art, "verification", stop)?;
    if result.iter().all(|r| r.passed)
        && workspace.join("Cargo.toml").exists()
        && project::snapshot(&workspace)?.iter().any(|(path, hash)| {
            path.ends_with(".rs") && !path.starts_with("tests/") && before.get(path) != Some(hash)
        })
    {
        match independent_probe(c, m, &workspace, art, &mut task, s.cycle, stop) {
            Ok(Some(probed)) => result.extend(probed),
            Ok(None) => {}
            Err(e) => {
                emit(
                    art,
                    "probe-error",
                    &format!("Independent probe unavailable: {e:#}"),
                )?;
                crate::events::log(format!("Independent probe unavailable: {e:#}"));
            }
        }
    }
    emit(art, "verification", &result)?;
    let after = project::snapshot(&workspace)?;
    let changed_files: Vec<_> = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|p| before.get(*p) != after.get(*p))
        .cloned()
        .collect();
    // Stage all attempt files so review includes new files. Only the private attempt is staged.
    project::git(&workspace, &["add", "-A"])?;
    let diff = project::git(
        &workspace,
        &["diff", "--cached", "--no-ext-diff", "--no-textconv"],
    )?;
    crate::events::send(crate::events::Event::Phase("Review".into()));
    let review:Review=m.structured(crate::prompts::REVIEW,json!({"main_goal":c.goal,"criteria":criterion_specs(&task),"task":task,"diff":diff,"diff_truncated":false,"changed_files":changed_files,"resulting_files":project::context(&workspace,&task.files,8000),"baseline_test_names":test_names(&baseline),"verification":result,"independent_probe":fs::read(art.join("probe-outcome.json")).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok())}))?;
    emit(art, "review", &review)?;
    let staged_names = project::git(&workspace, &["diff", "--cached", "--name-only", "-z"])?;
    let unexpected: Vec<_> = changed_files
        .iter()
        .map(String::as_str)
        .chain(staged_names.split('\0').filter(|p| !p.is_empty()))
        .filter(|p| !path_in_scope(p, &task, &workspace))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let in_scope = unexpected.is_empty();
    emit(
        art,
        "scope",
        &json!({"allowed_task_files":task.files,"unexpected_files":unexpected,"cargo_lockfiles_allowed":true}),
    )?;
    let tests_preserved = test_names(&baseline).is_subset(&test_names(&result));
    let accepted = in_scope
        && tests_preserved
        && !changed_files.is_empty()
        && (acceptance_allowed(&task, &review, &result, true)
            || partial_allowed(&task, &review, &result, &baseline));
    emit(
        art,
        "acceptance",
        &json!({"accepted":accepted,"changed":!changed_files.is_empty(),"checks_pass":result.iter().all(|r|r.passed),"in_scope":in_scope,"tests_preserved":tests_preserved,"review_decision":review.decision,"unmet_criteria":task.acceptance.iter().enumerate().filter(|(i,_)|!criterion_satisfied(&task,&review,*i)).map(|(_,a)|a).collect::<Vec<_>>()}),
    )?;
    if accepted {
        anyhow::ensure!(!stop.load(Ordering::SeqCst), "Stopped before acceptance");
        project::git(
            &workspace,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                &format!("chuggin: {}", project::excerpt(&task.title, 100)),
            ],
        )?;
        s.accepted_ref = project::git(&workspace, &["rev-parse", "HEAD"])?;
        s.accepted_branch = branch;
        s.accepted_workspace = workspace;
    }
    Ok(Outcome {
        cycle: s.cycle,
        task: task.title,
        disposition: if accepted {
            if review.decision == Decision::Partial {
                "accepted/partial".into()
            } else {
                "accepted".into()
            }
        } else {
            format!("rejected/{:?}", review.decision)
        },
        evidence: format!(
            "{}; unexpected_files={unexpected:?}; in_scope={in_scope}; tests_preserved={tests_preserved}; checks_pass={}",
            review.reason,
            result.iter().all(|r| r.passed)
        ),
        artifact_dir: art.into(),
    })
}
pub fn run(path: &Path, count: Option<u64>, stop: Arc<AtomicBool>) -> Result<()> {
    let c = load(path)?;
    crate::setup::ensure_git_identity(&c.repo)?;
    let started = Instant::now();
    let time_up = || {
        let limit = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["run_duration_seconds"].as_u64())
            .unwrap_or(c.run_duration_seconds);
        limit > 0 && started.elapsed().as_secs() >= limit
    };
    anyhow::ensure!(
        c.checks
            .iter()
            .all(|x| x.argv[0] != "REPLACE_WITH_YOUR_TEST_COMMAND"),
        "Set real validation commands before running"
    );
    fs::create_dir_all(&c.state_dir)?;
    let state_dir = fs::canonicalize(&c.state_dir)?;
    let lock = state_dir.join("run.lock");
    let _lock_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        anyhow::ensure!(
            unsafe { libc::flock(_lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Another Chuggin process is already running this project"
        );
    }
    let state_path = state_dir.join("state.json");
    let mut state: State = if state_path.exists() {
        let saved: State = serde_json::from_slice(&fs::read(&state_path)?)?;
        anyhow::ensure!(
            saved.goal == c.goal && saved.repo == fs::canonicalize(&c.repo)?,
            "This state belongs to a different goal or repository; use a new state_dir"
        );
        saved
    } else {
        let head = project::git(&c.repo, &["rev-parse", "HEAD"]).context(
            "Target needs an initial commit. Uncommitted changes are not imported into attempts",
        )?;
        let run_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        let baseline = state_dir.join("baseline");
        project::git(
            &c.repo,
            &[
                "worktree",
                "add",
                "--detach",
                baseline.to_str().context("Non-UTF8 path")?,
                &head,
            ],
        )?;
        State {
            goal: c.goal.clone(),
            repo: fs::canonicalize(&c.repo)?,
            run_id,
            accepted_ref: head,
            accepted_workspace: baseline,
            ..State::default()
        }
    };
    // A soft stop is observed only between cycles. Stage work must finish normally.
    let stage_stop = Arc::new(AtomicBool::new(false));
    let mut m = Model::new(
        &c.ollama_url,
        &c.model,
        c.context_tokens,
        c.output_tokens,
        stage_stop.clone(),
    )?;
    m.use_project_settings(path);
    if state.cycle > 0 && !state.recent.iter().any(|o| o.cycle == state.cycle) {
        let art = state_dir.join(format!("cycle-{:06}", state.cycle));
        if let Ok(bytes) = fs::read(art.join("task.json"))
            && let Ok(task) = serde_json::from_slice::<Task>(&bytes)
            && art.join("workspace").is_dir()
        {
            let interrupted = Outcome {
                cycle: state.cycle, task: task.title, disposition: "retry".into(),
                evidence: "Previous process ended before recording an outcome; candidate files require fresh verification.".into(),
                artifact_dir: art.clone(),
            };
            emit(&art, "outcome", &interrupted)?;
            state.recent.push(interrupted);
            if state.recent.len() > 6 {
                state.recent.remove(0);
            }
        }
    }
    save(&state_path, &state)?;
    let mut completed = 0;
    while !stop.load(Ordering::SeqCst) && !time_up() && count.is_none_or(|n| completed < n) {
        state.cycle += 1;
        crate::events::send(crate::events::Event::Cycle(state.cycle));
        completed += 1;
        let art = state_dir.join(format!("cycle-{:06}", state.cycle));
        fs::create_dir(&art)?;
        save(&state_path, &state)?;
        crate::events::log(format!(
            "Cycle {}: discovery → task shaping → implementation → verification → review",
            state.cycle
        ));
        let outcome = match cycle(&c, &mut state, &m, &art, &stage_stop) {
            Ok(o) => o,
            Err(e) => Outcome {
                cycle: state.cycle,
                task: fs::read(art.join("task.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Task>(&b).ok())
                    .map(|t| t.title)
                    .unwrap_or("incomplete attempt".into()),
                disposition: "retry".into(),
                evidence: format!("{e:#}"),
                artifact_dir: art.clone(),
            },
        };
        emit(&art, "outcome", &outcome)?;
        crate::events::log(format!("{}: {}", outcome.disposition, outcome.evidence));
        state.recent.push(outcome);
        if state.recent.len() > 6 {
            state.recent.remove(0);
        }
        save(&state_path, &state)?;
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
        crate::events::log("Run duration reached; completed the current cycle and saved progress. Resume starts a new timer.".into());
    }
    crate::events::log(format!(
        "Accepted branch: {}\nArtifacts: {}",
        state.accepted_branch,
        state_dir.display()
    ));
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reviewer_cannot_override_failed_checks_or_missing_criteria() {
        let t = Task {
            title: "t".into(),
            objective: "o".into(),
            acceptance: vec!["roundtrip".into()],
            files: vec![],
            out_of_scope: vec![],
        };
        let mut r = Review {
            decision: Decision::Accept,
            reason: "ok".into(),
            criteria: vec![],
        };
        let mut c = CheckResult {
            exit_code: None,
            argv: vec!["test".into()],
            passed: true,
            timed_out: false,
            output: "".into(),
        };
        assert!(!acceptance_allowed(&t, &r, &[c.clone()], true));
        r.criteria.push(Criterion {
            criterion: "roundtrip".into(),
            passed: true,
            evidence: "diff and test evidence".into(),
        });
        assert!(acceptance_allowed(&t, &r, &[c.clone()], true));
        r.criteria[0].criterion = "  `roundtrip`  ".into();
        assert!(acceptance_allowed(&t, &r, &[c.clone()], true));
        r.criteria[0].criterion = "different behavior".into();
        assert!(!acceptance_allowed(&t, &r, &[c.clone()], true));
        r.criteria[0].criterion = "roundtrip".into();
        c.passed = false;
        assert!(!acceptance_allowed(&t, &r, &[c], true));
        assert!(!acceptance_allowed(&t, &r, &[], true));
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    #[test]
    fn source_dependencies_can_be_added_without_allowing_control_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/run.rs"), "pub struct Run;").unwrap();
        let mut task = Task {
            title: "Paragraph".into(),
            objective: "Store runs".into(),
            acceptance: vec!["tested".into()],
            files: vec!["src/paragraph.rs".into()],
            out_of_scope: vec![],
        };
        assert!(
            extend_source_access(&mut task, dir.path(), "src/run.rs", "Need a text accessor")
                .is_ok()
        );
        assert!(task.files.contains(&"src/run.rs".to_string()));
        fs::write(dir.path().join("NOTES.md"), "Document conventions").unwrap();
        assert!(
            extend_source_access(
                &mut task,
                dir.path(),
                "NOTES.md",
                "Keep documentation consistent"
            )
            .is_ok()
        );
        assert!(task.files.contains(&"NOTES.md".to_string()));
        assert!(
            extend_source_access(&mut task, dir.path(), "../outside.rs", "dependency").is_err()
        );
        assert!(extend_source_access(&mut task, dir.path(), ".git/config", "dependency").is_err());
        assert!(
            extend_source_access(&mut task, dir.path(), "chuggin.json", "change checks").is_err()
        );
        assert!(
            extend_source_access(&mut task, dir.path(), "src/missing.rs", "dependency").is_err()
        );
    }
    fn task() -> Task {
        Task {
            title: "Architecture".into(),
            objective: "Document architecture".into(),
            acceptance: (0..7).map(|n| format!("Criterion {n}")).collect(),
            files: vec!["DESIGN.md".into()],
            out_of_scope: vec![],
        }
    }
    #[test]
    fn longer_criteria_lists_are_not_invalid_scope() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = task();
        assert!(validate_task(&t, dir.path()).is_ok());
        t.files = (0..10).map(|n| format!("src/file{n}.rs")).collect();
        assert!(validate_task(&t, dir.path()).is_ok());
        t.files.push("../escape".into());
        assert!(validate_task(&t, dir.path()).is_err());
        t.files.clear();
        assert!(
            validate_task(&t, dir.path())
                .unwrap_err()
                .to_string()
                .contains("writable file")
        );
    }
    #[test]
    fn cargo_lock_is_incidental_but_unrelated_source_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let t = task();
        assert!(!path_in_scope("Cargo.lock", &t, dir.path()));
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(path_in_scope("Cargo.lock", &t, dir.path()));
        assert!(path_in_scope("DESIGN.md", &t, dir.path()));
        assert!(!path_in_scope("src/main.rs", &t, dir.path()));
        assert!(!path_in_scope("../Cargo.lock", &t, dir.path()));
    }
}

fn recent_evidence(s: &State) -> Value {
    json!(
        s.recent
            .iter()
            .map(|o| {
                let verification=fs::read(o.artifact_dir.join("verification.json")).ok().and_then(|b|serde_json::from_slice::<Vec<CheckResult>>(&b).ok());
                let gate=fs::read(o.artifact_dir.join("acceptance.json")).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok());
                json!({"gate":gate,"cycle":o.cycle,"task":o.task,"accepted":o.disposition.starts_with("accepted"),
                    "checks_passed":verification.map(|v|v.iter().all(|r|r.passed)),"disposition":o.disposition})
            })
            .collect::<Vec<_>>()
    )
}

fn recoverable(s: &State) -> Option<Outcome> {
    s.recent
        .last()
        .filter(|o| {
            if !matches!(
                o.disposition.as_str(),
                "rejected/Accept"
                    | "rejected/Repair"
                    | "rejected/Replan"
                    | "rejected/Rollback"
                    | "retry"
            ) {
                return false;
            }
            if o.disposition == "rejected/Accept" {
                let checks = fs::read(o.artifact_dir.join("verification.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Vec<CheckResult>>(&b).ok());
                if !checks.is_some_and(|c| !c.is_empty() && c.iter().all(|r| r.passed)) {
                    return false;
                }
            }
            if s.recent.iter().filter(|p| p.task == o.task).count() > 2 {
                return false;
            }
            let attempt = fs::read(o.artifact_dir.join("attempt.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            attempt.is_some_and(|a| a["base"] == s.accepted_ref)
                && o.artifact_dir.join("task.json").is_file()
                && o.artifact_dir.join("workspace").is_dir()
        })
        .cloned()
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn recover_only_same_baseline_and_bound_retries() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("workspace")).unwrap();
        fs::write(dir.path().join("task.json"), "{}").unwrap();
        fs::write(dir.path().join("attempt.json"), r#"{"base":"base-one"}"#).unwrap();
        let prior = Outcome {
            cycle: 1,
            task: "small fix".into(),
            disposition: "rejected/Repair".into(),
            evidence: "one failing test".into(),
            artifact_dir: dir.path().into(),
        };
        let mut state = State {
            accepted_ref: "base-one".into(),
            recent: vec![prior.clone()],
            ..State::default()
        };
        assert!(recoverable(&state).is_some());
        state.accepted_ref = "different-base".into();
        assert!(recoverable(&state).is_none());
        state.accepted_ref = "base-one".into();
        state.recent = vec![prior.clone(), prior.clone(), prior];
        assert!(recoverable(&state).is_none());
        state.recent.truncate(1);
        let mut latest = state.recent[0].clone();
        latest.disposition = "accepted".into();
        state.recent.push(latest);
        assert!(
            recoverable(&state).is_none(),
            "Never resurrect an older failed candidate"
        );
    }

    #[test]
    fn planning_uses_check_results_instead_of_review_claims() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("verification.json"), r#"[{"argv":["cargo","test"],"passed":false,"timed_out":false,"output":"compiler error"}]"#).unwrap();
        let state = State {
            recent: vec![Outcome {
                cycle: 1,
                task: "small fix".into(),
                disposition: "rejected/Accept".into(),
                evidence: "Everything passed; make Style Copy".into(),
                artifact_dir: dir.path().into(),
            }],
            ..State::default()
        };
        let evidence = recent_evidence(&state);
        assert_eq!(evidence[0]["accepted"], false);
        assert_eq!(evidence[0]["checks_passed"], false);
        assert!(!evidence.to_string().contains("Style Copy"));
    }
}

#[cfg(test)]
mod partial_tests {
    use super::*;
    #[test]
    fn partial_progress_uses_validation_and_preserves_observed_tests() {
        let task = Task {
            title: "slice".into(),
            objective: "one useful slice".into(),
            acceptance: vec!["behavior".into(), "later feature".into()],
            files: vec![],
            out_of_scope: vec![],
        };
        let review = Review {
            decision: Decision::Partial,
            reason: "independent behavior complete".into(),
            criteria: vec![Criterion {
                criterion: "behavior".into(),
                passed: true,
                evidence: "new behavior test".into(),
            }],
        };
        let baseline = CheckResult {
            exit_code: None,
            argv: vec!["cargo".into(), "test".into()],
            passed: true,
            timed_out: false,
            output: "test existing ... ok\n".into(),
        };
        assert!(partial_allowed(
            &task,
            &review,
            std::slice::from_ref(&baseline),
            std::slice::from_ref(&baseline)
        ));
        let mut result = baseline.clone();
        result.output.push_str("test new_behavior ... ok\n");
        assert!(partial_allowed(
            &task,
            &review,
            std::slice::from_ref(&result),
            std::slice::from_ref(&baseline)
        ));
        result.output = "test new_behavior ... ok\n".into();
        assert!(!partial_allowed(
            &task,
            &review,
            std::slice::from_ref(&result),
            std::slice::from_ref(&baseline)
        ));
        result.passed = false;
        assert!(!partial_allowed(&task, &review, &[result], &[]));
    }
}

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
struct Probe {
    code: String,
    rationale: String,
}
fn independent_probe(
    c: &Config,
    m: &Model,
    workspace: &Path,
    art: &Path,
    task: &mut Task,
    cycle: u64,
    stop: &AtomicBool,
) -> Result<Option<Vec<CheckResult>>> {
    crate::events::log("Checking the change with a fresh regression-test stage".into());
    let mut probe:Probe=m.structured(crate::prompts::PROBE,json!({"task":task,"cargo":project::read(workspace,"Cargo.toml")?,"code_index":project::excerpt(&crate::code_index::index(workspace)?.to_string(),6500),"changed_files":project::context(workspace,&task.files,12000)}))?;
    emit(art, "probe-proposal", &probe)?;
    for attempt in 0..2 {
        if probe.code.trim().is_empty() {
            emit(
                art,
                "probe-outcome",
                &json!({"status":"skipped","reason":probe.rationale}),
            )?;
            return Ok(None);
        }
        anyhow::ensure!(
            probe.code.len() <= 10000 && probe.code.contains("#[test]"),
            "Probe must be a bounded Rust test"
        );
        syn::parse_file(&probe.code).context("Probe did not contain valid Rust syntax")?;
        let path = format!("tests/chuggin_regression_{cycle}.rs");
        anyhow::ensure!(!workspace.join(&path).exists(), "Probe path already exists");
        project::write(workspace, &path, &probe.code)?;
        let mut bounded = c.clone();
        bounded.checks = vec![Check {
            argv: vec![
                "cargo".into(),
                "test".into(),
                "--test".into(),
                format!("chuggin_regression_{cycle}"),
                "--".into(),
                "--nocapture".into(),
            ],
            timeout_seconds: 60,
        }];
        let checked = checks(&bounded, workspace, art, "independent-probe", stop);
        let invalid = checked.as_ref().map_or(true, |rs| {
            rs.iter().any(|r| {
                r.timed_out
                    || r.output.lines().any(|l| {
                        l.starts_with("error[E") || l.starts_with("error: could not compile")
                    })
            })
        });
        if invalid {
            fs::remove_file(workspace.join(&path))?;
            emit(
                art,
                "probe-outcome",
                &json!({"status":"invalid_probe_removed","reason":"The added probe did not compile or finish; it is not evidence against the implementation.","checks":checked.as_ref().ok(),"error":checked.as_ref().err().map(|e|e.to_string())}),
            )?;
            if attempt == 0
                && checked
                    .as_ref()
                    .is_ok_and(|rs| rs.iter().all(|r| !r.timed_out))
            {
                crate::events::log("Repairing the regression test from compiler feedback".into());
                probe = m.structured(crate::prompts::PROBE,json!({"instruction":"Repair only this test's compile errors using the actual compiler diagnostics. Preserve assertions and expected values. Add explicit imports from the Cargo library crate; this file lives in tests/, outside src/lib.rs. Return empty code if the API is not public or the test cannot be made valid.","cargo":project::read(workspace,"Cargo.toml")?,"public_api":project::excerpt(&crate::code_index::index(workspace)?.to_string(),4000),"previous_code":probe.code,"compiler_feedback":project::excerpt(&serde_json::to_string(&checked.as_ref().ok())?,5000)}))?;
                emit(art, "probe-repair-proposal", &probe)?;
                continue;
            }
            return Ok(None);
        }
        let checks = checked?;
        task.files.push(path);
        emit(art, "task", task)?;
        emit(
            art,
            "probe-outcome",
            &json!({"status":if checks.iter().all(|r|r.passed){"passed_and_retained"}else{"failed_and_retained"},"rationale":probe.rationale}),
        )?;
        return Ok(Some(checks));
    }
    Ok(None)
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn stable_ids_accept_evidence_without_reproducing_sentences() {
        let mut task = Task {
            title: "Fix text handling".into(),
            objective: "Correct text behavior".into(),
            acceptance: vec!["A long criterion with `code`, punctuation, and exact APIs.".into()],
            files: vec!["src/lib.rs".into()],
            out_of_scope: vec![],
        };
        let mut review = Review {
            decision: Decision::Accept,
            reason: "verified".into(),
            criteria: vec![Criterion {
                criterion: "C1".into(),
                passed: true,
                evidence: "non-ASCII regression passes".into(),
            }],
        };
        assert!(criterion_satisfied(&task, &review, 0));
        review.criteria.push(Criterion {
            criterion: "C1".into(),
            passed: false,
            evidence: "conflicting evidence".into(),
        });
        assert!(!criterion_satisfied(&task, &review, 0));
        task.acceptance.push(task.acceptance[0].clone());
        task.files.push("src/lib.rs".into());
        normalize_task(&mut task);
        normalize_task(&mut task);
        assert_eq!(task.acceptance.len(), 1);
        assert_eq!(task.files.len(), 1);
    }
    #[test]
    fn discovery_cannot_write_files() {
        let d = tempfile::tempdir().unwrap();
        let mut research = crate::web_tools::Research::default();
        assert!(
            inspect_tool(
                d.path(),
                "write_file",
                &json!({"path":"oops.rs","content":"bad"}),
                &mut research
            )
            .is_err()
        );
        assert!(!d.path().join("oops.rs").exists());
    }
}
