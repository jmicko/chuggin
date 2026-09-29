//! Shared operator service. Both chat and MCP use this dispatcher.
use crate::{project, run_control::RunControl, runner};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub fn id() -> String {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        SERIAL.fetch_add(1, Ordering::SeqCst)
    )
}
pub fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid session or operation ID"
    );
    Ok(())
}
#[derive(Clone)]
struct Lease {
    session: String,
    granted: bool,
    baseline: String,
    branch: String,
    touched: Instant,
    generation: u64,
}
type OwnedJob = (String, Arc<Mutex<crate::command_jobs::Job>>);
pub struct Controller {
    pub path: PathBuf,
    pub controls: Arc<RunControl>,
    pub stop: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
    pub generation: AtomicU64,
    lease: Mutex<Option<Lease>>,
    sessions: Mutex<BTreeMap<String, Instant>>,
    commands: Mutex<BTreeMap<String, OwnedJob>>,
    operations: Mutex<()>,
    release_when_idle: Mutex<BTreeMap<String, String>>,
    settings: Mutex<()>,
    workspace: Mutex<()>,
    handoffs: Mutex<()>,
    pub last_error: Mutex<Option<String>>,
    started: Mutex<Option<Instant>>,
    ended_elapsed: Mutex<Option<u64>>,
    _locks: Vec<fs::File>,
}
impl Controller {
    pub fn new(path: &Path) -> Result<Arc<Self>> {
        let path = fs::canonicalize(path)?;
        let c = runner::load(&path)?;
        ensure!(
            !runner::needs_migration(&path)?,
            "Review the one-time project upgrade from Resume project first"
        );
        let locks = runner::lock_project(&c)?;
        let controls = Arc::new(RunControl::default());
        controls.configure(&path);
        controls.owns_project.store(true, Ordering::SeqCst);
        if c.state_dir.join("operator/editing.json").exists() {
            controls.hold("recovery", "An editing session was interrupted. Inspect files and command logs, then recover editing ownership in Chat or external control.");
        }
        fs::create_dir_all(c.state_dir.join("operator/operations"))?;
        fs::create_dir_all(c.state_dir.join("operator/sessions"))?;
        let marker = c.state_dir.join("operator/controller-running.json");
        if marker.exists() {
            crate::setup::save(
                &c.state_dir.join("operator/interrupted-controller.json"),
                &read_json(&marker)?,
            )?;
        }
        if c.state_dir
            .join("operator/interrupted-controller.json")
            .exists()
        {
            controls.hold("controller_restart","Controller ended unexpectedly. Review saved work and any surviving commands before resuming; Chat can help recover.");
        }
        crate::setup::save(
            &marker,
            &json!({"pid":std::process::id(),"started":chrono::Utc::now()}),
        )?;
        // An incomplete journal entry can represent a completed external side effect.
        for entry in fs::read_dir(c.state_dir.join("operator/operations"))? {
            let p = entry?.path();
            if let Ok(mut v) = read_json(&p)
                && (v["status"] == "running" || v["status"] == "uncertain")
            {
                v["status"] = json!("uncertain");
                v["error"] = json!(
                    "Controller interrupted; inspect files and logs before retrying this operation"
                );
                crate::setup::save(&p, &v)?;
                controls.hold(
                    "recovery",
                    "Interrupted operator action needs review in Chat or external control",
                );
            }
        }
        Ok(Arc::new(Self {
            path,
            controls,
            stop: Arc::default(),
            running: Arc::default(),
            generation: AtomicU64::new(0),
            lease: Mutex::new(None),
            sessions: Mutex::default(),
            commands: Mutex::default(),
            operations: Mutex::new(()),
            release_when_idle: Mutex::default(),
            settings: Mutex::new(()),
            workspace: Mutex::new(()),
            handoffs: Mutex::new(()),
            last_error: Mutex::default(),
            started: Mutex::default(),
            ended_elapsed: Mutex::default(),
            _locks: locks,
        }))
    }
    pub fn has_editor(&self) -> bool {
        self.lease.lock().unwrap().is_some()
    }
    pub fn config(&self) -> Result<runner::Config> {
        runner::load(&self.path)
    }
    pub fn open_session(&self, requested: Option<&str>) -> Result<String> {
        let session = requested.map(str::to_owned).unwrap_or_else(id);
        valid_id(&session)?;
        let c = self.config()?;
        let dir = c.state_dir.join("operator/sessions").join(&session);
        fs::create_dir_all(&dir)?;
        self.sessions
            .lock()
            .unwrap()
            .insert(session.clone(), Instant::now());
        Ok(session)
    }
    pub fn touch(&self, session: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        *sessions
            .get_mut(session)
            .context("Open or resume this operator session first")? = Instant::now();
        if let Some(l) = self.lease.lock().unwrap().as_mut()
            && l.session == session
        {
            l.touched = Instant::now();
        }
        Ok(())
    }
    pub fn start(self: &Arc<Self>, mode: &str) -> Result<Value> {
        self.start_count(mode, None, true)
    }
    pub fn start_count(
        self: &Arc<Self>,
        mode: &str,
        count: Option<u64>,
        reopen: bool,
    ) -> Result<Value> {
        ensure!(
            ["scheduled", "one_cycle", "until_close"].contains(&mode),
            "Unknown resume mode"
        );
        let report = self.config()?.state_dir.join("goal-completion.json");
        if reopen && report.exists() {
            fs::remove_file(report)?;
        }
        if !self.running.load(Ordering::SeqCst) {
            self.controls.cycle_active(false);
            self.controls.finish_requested_cycle();
        }
        if mode == "scheduled" {
            self.controls.respect_schedule();
        }
        if mode == "one_cycle" {
            self.controls.allow_one_cycle();
        }
        if mode == "until_close" {
            self.controls.resume_outside_hours()?;
        } else {
            self.controls.resume();
        }
        self.stop.store(false, Ordering::SeqCst);
        if self.running.swap(true, Ordering::SeqCst) {
            return self.status();
        }
        self.controls.reset_stopped();
        *self.started.lock().unwrap() = Some(Instant::now());
        *self.ended_elapsed.lock().unwrap() = None;
        let engine = self.clone();
        let count = if mode == "one_cycle" { Some(1) } else { count };
        std::thread::spawn(move || {
            crate::events::set_actor("loop");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner::run_controlled(
                    &engine.path,
                    count,
                    engine.stop.clone(),
                    engine.controls.clone(),
                )
            }));
            *engine.last_error.lock().unwrap() = match result {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(_) => Some("Loop worker panicked; files retained".into()),
            };
            *engine.ended_elapsed.lock().unwrap() = engine
                .started
                .lock()
                .unwrap()
                .map(|t| engine.controls.active_elapsed(t).as_secs());
            engine.controls.cycle_active(false);
            engine.running.store(false, Ordering::SeqCst);
        });
        self.status()
    }
    fn elapsed(&self) -> Option<u64> {
        let ended = *self.ended_elapsed.lock().unwrap();
        let started = *self.started.lock().unwrap();
        ended.or_else(|| started.map(|t| self.controls.active_elapsed(t).as_secs()))
    }
    pub fn status(&self) -> Result<Value> {
        let c = self.config()?;
        let lease = self
            .lease
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| json!({"session":l.session,"granted":l.granted}));
        Ok(
            json!({"version":env!("CARGO_PKG_VERSION"),"config":self.path,"project":c.repo,"running":self.running.load(Ordering::SeqCst),"paused":self.controls.is_paused(),"pause_requested":self.controls.pause_requested(),"manual_pause":self.controls.manual_pause(),"holds":self.controls.reasons(),"schedule":self.controls.schedule_status().ok(),"stop_requested":self.stop.load(Ordering::SeqCst),"active_commands":project::active_checks(),"editing":lease,"interrupted_editing":read_json(&c.state_dir.join("operator/editing.json")).ok(),"generation":self.generation.load(Ordering::SeqCst),"model":c.model,"goal":c.goal,"state":read_json(&c.state_dir.join("state.json")).ok().map(|v|compact_state(&v)),"active_elapsed_seconds":self.elapsed(),"nudges":crate::nudge::read(&c.state_dir)?,"last_error":self.last_error.lock().unwrap().clone()}),
        )
    }
    pub fn begin_edit(&self, session: &str) -> Result<Value> {
        self.touch(session)?;
        let mut lease = self.lease.lock().unwrap();
        if let Some(l) = lease.as_ref() {
            ensure!(
                l.session == session,
                "Editing is owned by session {}; wait or ask the user to end that session",
                l.session
            );
        }
        if lease.is_none() {
            self.controls
                .hold("operator", "Loop waiting for operator editing");
            *lease = Some(Lease {
                session: session.into(),
                granted: false,
                baseline: String::new(),
                branch: String::new(),
                touched: Instant::now(),
                generation: self.controls.changed.load(Ordering::SeqCst),
            });
        }
        let l = lease.as_mut().unwrap();
        if !l.granted
            && (!self.running.load(Ordering::SeqCst) || self.controls.is_paused())
            && project::active_checks().is_empty()
        {
            let c = self.config()?;
            l.baseline = crate::workspace::tree(&c.repo, Some(&c.state_dir))?;
            l.branch = crate::workspace::branch(&c.repo)?;
            crate::workspace::autosave(
                &c.repo,
                &c.state_dir,
                &format!("operator-{}", id()),
                "Before operator editing",
            )?;
            l.granted = true;
            // An external client can write through native tools, including editing then reverting.
            self.controls.changed.fetch_add(1, Ordering::SeqCst);
            crate::setup::save(
                &c.state_dir.join("operator/editing.json"),
                &json!({"session":session,"baseline":l.baseline,"branch":l.branch}),
            )?;
        }
        Ok(
            json!({"granted":l.granted,"session_id":session,"status":if l.granted {"ready"} else {"waiting"},"active_commands":project::active_checks(),"instruction":"Keep ownership throughout edits and commands. Call end_edit after all writers finish. If waiting, poll begin_edit; no write has been executed."}),
        )
    }
    pub fn end_edit(&self, session: &str, summary: &str) -> Result<Value> {
        let _workspace = self.workspace.lock().unwrap();
        self.touch(session)?;
        let mut lease = self.lease.lock().unwrap();
        let Some(l) = lease.as_ref() else {
            return Ok(json!({"released":true}));
        };
        ensure!(l.session == session, "Another operator owns editing");
        ensure!(
            !l.granted
                || (project::active_checks().is_empty()
                    && !self
                        .commands
                        .lock()
                        .unwrap()
                        .values()
                        .any(|(_, j)| j.lock().unwrap().running())),
            "Commands are still running; editing ownership is retained"
        );
        let c = self.config()?;
        if l.granted {
            crate::workspace::check(&c.repo, &l.branch)?;
            let current = crate::workspace::tree(&c.repo, Some(&c.state_dir))?;
            if current != l.baseline || l.generation != self.controls.changed.load(Ordering::SeqCst)
            {
                let paths =
                    crate::workspace::changed_paths(&c.repo, &c.state_dir, &l.baseline, &current)?;
                crate::workspace::autosave(
                    &c.repo,
                    &c.state_dir,
                    &format!("operator-{}", id()),
                    "After operator editing; unfinished work retained",
                )?;
                self.handoff(json!({"changed_paths":paths,"summary":summary,"instruction":"The operator changed these files. Re-read current content and reconsider the pending task. Earlier validation may be stale. Continue normal work; this is a one-time notice."}))?;
            }
        }
        *lease = None;
        let record = c.state_dir.join("operator/editing.json");
        if record.exists() {
            fs::remove_file(record)?;
        }
        self.controls.release("operator");
        Ok(json!({"released":true,"loop_eligible":!self.controls.pause_requested()}))
    }
    fn handoff(&self, mut value: Value) -> Result<()> {
        let _handoffs = self.handoffs.lock().unwrap();
        let c = self.config()?;
        let mut notes = read_json(&c.state_dir.join("operator-handoff.json"))
            .unwrap_or(json!({"revision":0,"notes":[]}));
        let revision = notes["revision"].as_u64().unwrap_or(0) + 1;
        notes["revision"] = json!(revision);
        value["revision"] = json!(revision);
        notes["notes"]
            .as_array_mut()
            .context("Invalid operator handoff")?
            .push(value);
        if notes["notes"].as_array().unwrap().len() > 20 {
            notes["notes"].as_array_mut().unwrap().remove(0);
        }
        crate::setup::save(&c.state_dir.join("operator-handoff.json"), &notes)?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.controls.changed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    pub fn finish_chat(&self, session: &str) {
        self.release_when_idle.lock().unwrap().insert(
            session.into(),
            "Interactive chat changes; inspect current files and checks".into(),
        );
    }
    pub fn maintenance(&self) {
        let jobs: Vec<_> = self.commands.lock().unwrap().values().cloned().collect();
        for (owner, job) in jobs {
            crate::events::set_actor(&owner);
            let mut job = job.lock().unwrap();
            if job.running() {
                let _ = job.poll(0, &AtomicBool::new(false));
            }
        }
        crate::events::set_actor("loop");
        let releases = self.release_when_idle.lock().unwrap().clone();
        for (session, summary) in releases {
            if self.end_edit(&session, &summary).is_ok() {
                self.release_when_idle.lock().unwrap().remove(&session);
            }
        }

        if let Some(l) = self.lease.lock().unwrap().as_ref()
            && l.touched.elapsed() > Duration::from_secs(180)
        {
            self.controls.hold("operator","Operator disconnected during editing. Resume the session and release ownership after reviewing its commands.");
        }
    }
    pub fn operation_path(&self, session: &str, operation: &str) -> Result<PathBuf> {
        valid_id(session)?;
        valid_id(operation)?;
        Ok(self
            .config()?
            .state_dir
            .join("operator/operations")
            .join(format!("{session}-{operation}.json")))
    }
    pub fn call(
        self: &Arc<Self>,
        session: &str,
        name: &str,
        args: Value,
        operation: &str,
    ) -> Result<Value> {
        self.touch(session)?;
        ensure!(
            schemas()
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == name),
            "Unknown operator tool: {name}"
        );
        let path = self.operation_path(session, operation)?;
        let mutation = is_mutation(name);
        if mutation {
            let _guard = self.operations.lock().unwrap();
            if path.exists() {
                let saved = read_json(&path)?;
                ensure!(
                    saved["name"] == name && saved["arguments"] == args,
                    "Operation ID reused with different arguments"
                );
                return Ok(saved);
            }
            crate::setup::save(
                &path,
                &json!({"status":"running","name":name,"arguments":args,"session":session,"operation_id":operation}),
            )?;
        }
        let result = self.execute(session, name, &args);
        let value = match result {
            Ok(v) => json!({"status":"complete","result":v}),
            Err(e) => json!({"status":"failed","error":format!("{e:#}")}),
        };
        if mutation {
            let mut saved = value.clone();
            saved["name"] = json!(name);
            saved["arguments"] = args;
            saved["operation_id"] = json!(operation);
            saved["session"] = json!(session);
            crate::setup::save(&path, &saved)?;
        }
        Ok(value)
    }
    fn execute(self: &Arc<Self>, session: &str, name: &str, args: &Value) -> Result<Value> {
        let c = self.config()?;
        match name {
            "acknowledge_controller_restart" => {
                ensure!(
                    args["previous_commands_stopped"] == true,
                    "Verify all commands from the interrupted controller have stopped before acknowledging recovery"
                );
                let marker = c.state_dir.join("operator/interrupted-controller.json");
                ensure!(
                    marker.exists(),
                    "No interrupted controller requires acknowledgement"
                );
                self.handoff(json!({"controller_recovery":text(args,"summary")?,"instruction":"Controller was interrupted. Re-read current files and command results; never replay uncertain actions automatically."}))?;
                fs::rename(
                    &marker,
                    c.state_dir
                        .join(format!("operator/recovered-controller-{}.json", id())),
                )?;
                self.controls.release("controller_restart");
                Ok(
                    json!({"reviewed":true,"instruction":"Other holds remain in effect; autonomous work was not restarted."}),
                )
            }
            "recover_edit_session" => {
                let _workspace = self.workspace.lock().unwrap();
                ensure!(
                    args["external_writers_stopped"] == true,
                    "Confirm all external editing processes have stopped before recovering ownership"
                );
                ensure!(
                    project::active_checks().is_empty(),
                    "Managed commands are still running; inspect list_commands first"
                );
                let path = c.state_dir.join("operator/editing.json");
                let record = read_json(&path)?;
                ensure!(
                    record["session"] == args["previous_session"],
                    "Editing ownership changed; inspect project_status again"
                );
                if let Some(old) = record["session"].as_str() {
                    self.sessions.lock().unwrap().remove(old);
                }
                crate::workspace::autosave(
                    &c.repo,
                    &c.state_dir,
                    &format!("operator-recovery-{}", id()),
                    "Recovered interrupted editing; preserve current files",
                )?;
                self.handoff(json!({"instruction":"An interrupted operator session was reviewed. Inspect current files before continuing.","summary":text(args,"summary")?}))?;
                *self.lease.lock().unwrap() = None;
                fs::remove_file(path)?;
                self.controls.release("operator");
                let uncertain = fs::read_dir(c.state_dir.join("operator/operations"))?
                    .filter_map(Result::ok)
                    .any(|e| read_json(&e.path()).is_ok_and(|v| v["status"] == "uncertain"));
                if !uncertain {
                    self.controls.release("recovery");
                }
                Ok(
                    json!({"recovered":true,"instruction":"No work was restarted or replayed. Request editing again if needed."}),
                )
            }
            "list_pending_actions" => {
                let mut pending = Vec::new();
                for entry in fs::read_dir(c.state_dir.join("operator/operations"))? {
                    if let Ok(v) = read_json(&entry?.path())
                        && (v["status"] == "running" || v["status"] == "uncertain")
                    {
                        pending.push(json!({"session":v["session"],"operation_id":v["operation_id"],"tool":v["name"],"status":v["status"],"error":v["error"]}));
                    }
                }
                Ok(json!({"actions":pending}))
            }
            "operation_status" => read_json(&self.operation_path(session, text(args, "id")?)?),
            "resolve_interrupted_action" => {
                let path = self.operation_path(session, text(args, "id")?)?;
                let mut record = read_json(&path)?;
                ensure!(
                    record["status"] == "uncertain",
                    "This action is not awaiting recovery"
                );
                record["status"] = json!("reviewed");
                record["resolution"] = json!(text(args, "summary")?);
                crate::setup::save(&path, &record)?;
                let pending = fs::read_dir(c.state_dir.join("operator/operations"))?
                    .filter_map(Result::ok)
                    .any(|e| read_json(&e.path()).is_ok_and(|v| v["status"] == "uncertain"));
                if !pending && !c.state_dir.join("operator/editing.json").exists() {
                    self.controls.release("recovery");
                }
                Ok(record)
            }
            "list_commands" => Ok(json!({"commands":crate::command_session::managed_commands()})),
            "project_status" => self.status(),
            "begin_edit" => self.begin_edit(session),
            "end_edit" => self.end_edit(
                session,
                args["summary"].as_str().unwrap_or("Operator changes"),
            ),
            "pause_loop" => {
                if !self.controls.manual_pause() {
                    self.controls.toggle_pause();
                }
                self.status()
            }
            "resume_loop" => self.start(args["mode"].as_str().unwrap_or("scheduled")),
            "stop_loop" => {
                self.stop.store(true, Ordering::SeqCst);
                self.status()
            }
            "retry_provider" => Ok(json!({"retry_queued":self.controls.retry_now()})),
            "get_settings" => Ok(
                json!({"revision":config_revision(&self.path)?,"settings":read_json(&self.path)?}),
            ),
            "update_settings" => {
                let _guard = self.settings.lock().unwrap();
                check_revision(&self.path, args)?;
                let mut v = read_json(&self.path)?;
                let patch = args["settings"]
                    .as_object()
                    .context("settings must be an object")?;
                for (key, val) in patch {
                    ensure!(
                        [
                            "model",
                            "chat_model",
                            "active_hours",
                            "run_duration_seconds",
                            "request_timeout_seconds",
                            "command_review_seconds",
                            "allow_goal_completion",
                            "context_tokens",
                            "output_tokens"
                        ]
                        .contains(&key.as_str()),
                        "Setting {key} is managed in human connection setup"
                    );
                    v[key] = val.clone();
                }
                let proposed: runner::Config = serde_json::from_value(v.clone())?;
                proposed.active_hours.at(chrono::Utc::now())?;
                ensure!(
                    !proposed.model.trim().is_empty()
                        && proposed.context_tokens >= 1024
                        && proposed.output_tokens > 0
                        && proposed.command_review_seconds > 0,
                    "Invalid model or working settings"
                );
                crate::setup::save(&self.path, &v)?;
                if patch.contains_key("active_hours") {
                    self.controls.respect_schedule();
                }
                Ok(json!({"revision":config_revision(&self.path)?,"settings":v}))
            }
            "set_goal" => {
                let _guard = self.settings.lock().unwrap();
                check_revision(&self.path, args)?;
                let goal = text(args, "goal")?.trim();
                ensure!(
                    !goal.is_empty() && goal.len() <= 64000,
                    "Goal must contain 1–64000 bytes"
                );
                let mut v = read_json(&self.path)?;
                let change = json!({"previous":v["goal"],"goal":goal,"id":id()});
                crate::setup::save(&c.state_dir.join("operator-goal.json"), &change)?;
                v["goal"] = json!(goal);
                crate::setup::save(&self.path, &v)?;
                if c.state_dir.join("goal-completion.json").exists() {
                    fs::rename(
                        c.state_dir.join("goal-completion.json"),
                        c.state_dir.join(format!("goal-completion-{}.json", id())),
                    )?;
                }
                self.handoff(json!({"goal":goal,"instruction":"The user revised the overall goal. Reconsider the active task without discarding useful work."}))?;
                Ok(json!({"goal":goal,"revision":config_revision(&self.path)?}))
            }
            "set_nudge" => {
                let expected = args["expected_nudge_id"].as_u64();
                Ok(serde_json::to_value(crate::nudge::set(
                    &c.state_dir,
                    text(args, "request")?,
                    expected,
                )?)?)
            }
            "cancel_nudge" => Ok(serde_json::to_value(crate::nudge::cancel(
                &c.state_dir,
                args["nudge_id"].as_u64().context("Missing nudge_id")?,
            )?)?),
            "read_history" => history(&c, args),
            "list_history" => list_history(&c, args),
            "operator_note" => {
                self.handoff(json!({"note":text(args,"note")?}))?;
                Ok(json!({"queued":true}))
            }
            "preview_restore" => {
                let target = restore_target(&c, text(args, "target")?)?;
                let before = crate::workspace::tree(&c.repo, Some(&c.state_dir))?;
                let diff = project::git(&c.repo, &["diff", "--stat", &before, &target, "--"])?;
                Ok(
                    json!({"target":target,"expected_tree":before,"changes":diff,"instruction":"Show these changes to the user. Restore only when requested; current files will first be backed up. Human staging must be empty."}),
                )
            }
            "reopen_nudge" => Ok(serde_json::to_value(crate::nudge::reopen(&c.state_dir)?)?),
            "project_diff" => Ok(
                json!({"diff":project::excerpt(&project::git(&c.repo,&["diff","HEAD","--"] )?,20000),"status":project::git(&c.repo,&["status","--short"])?}),
            ),
            "edit_file" | "write_file" | "run_command" | "run_checks" | "save_checkpoint"
            | "commit_changes" | "restore_checkpoint" => {
                let _workspace = self.workspace.lock().unwrap();
                let lease = self.begin_edit(session)?;
                ensure!(
                    lease["granted"] == true,
                    "Editing access is pending. Call begin_edit until granted, then retry with a new operation ID. No action executed."
                );
                if name == "restore_checkpoint" {
                    ensure!(
                        args["confirmed"] == true,
                        "Show preview_restore and obtain the user's restoration request first"
                    );
                    ensure!(
                        project::active_checks().is_empty(),
                        "Finish commands before restoring"
                    );
                    let target = restore_target(&c, text(args, "target")?)?;
                    let before = crate::workspace::tree(&c.repo, Some(&c.state_dir))?;
                    ensure!(
                        args["expected_tree"].as_str() == Some(before.as_str()),
                        "Files changed; refresh the restoration preview"
                    );
                    let (backup, _) = crate::workspace::autosave(
                        &c.repo,
                        &c.state_dir,
                        &format!("operator-{}", id()),
                        "Before operator restore",
                    )?;
                    crate::migration::restore_files(&c.repo, &c.state_dir, &target)?;
                    self.handoff(json!({"restored_tree":target,"backup":backup,"instruction":"User requested restoration. Re-read files and reconsider the current task."}))?;
                    return Ok(json!({"restored":true,"backup":backup}));
                }
                if name == "commit_changes" {
                    ensure!(
                        project::active_checks().is_empty(),
                        "Finish running commands before committing"
                    );
                    let tree = crate::workspace::tree(&c.repo, Some(&c.state_dir))?;
                    let head = crate::workspace::head(&c.repo);
                    let commit = crate::workspace::promote(
                        &c.repo,
                        &c.state_dir,
                        &head,
                        &tree,
                        text(args, "summary")?,
                    )?;
                    return Ok(
                        json!({"commit":commit,"instruction":"Commit recorded. This does not mark the loop task or overall goal complete."}),
                    );
                }
                if name == "save_checkpoint" {
                    let (reference, changed) = crate::workspace::autosave(
                        &c.repo,
                        &c.state_dir,
                        &format!("operator-{}", id()),
                        args["summary"].as_str().unwrap_or("Operator recovery save"),
                    )?;
                    return Ok(json!({"reference":reference,"changed":changed}));
                }
                if name == "run_command" || name == "run_checks" {
                    let checks = if name == "run_checks" {
                        c.checks.clone()
                    } else {
                        vec![crate::project::Check {
                            argv: serde_json::from_value(args["argv"].clone())?,
                            timeout_seconds: c.command_review_seconds,
                        }]
                    };
                    ensure!(!checks.is_empty(), "No configured checks");
                    let command = id();
                    let dir = c.state_dir.join("operator/sessions").join(session);
                    self.controls.changed.fetch_add(1, Ordering::SeqCst);
                    let mut job = crate::command_jobs::Job::new(
                        &c.repo,
                        &dir,
                        &command,
                        checks,
                        name == "run_checks",
                        false,
                    )?;
                    job.poll(0, &AtomicBool::new(false))?;
                    self.commands
                        .lock()
                        .unwrap()
                        .insert(command.clone(), (session.into(), Arc::new(Mutex::new(job))));
                    return Ok(json!({"command_id":command,"running":true}));
                }
                let file = text(args, "path")?;
                ensure!(
                    file != "chuggin.json",
                    "Use project control tools for settings"
                );
                project::safe_path(&c.repo, file)?;
                ensure!(
                    project::active_checks().is_empty(),
                    "A command is still using the workspace; inspect or finish it before editing"
                );
                self.controls.changed.fetch_add(1, Ordering::SeqCst);
                let result = if name == "edit_file" {
                    ensure!(
                        args["expected_version"].as_str()
                            == Some(config_revision(&project::safe_path(&c.repo, file)?)?.as_str()),
                        "File changed or expected_version missing; read_file again and use its version"
                    );
                    project::edit(
                        &c.repo,
                        file,
                        text(args, "old_text")?,
                        text(args, "new_text")?,
                    )?
                } else {
                    let target = project::safe_path(&c.repo, file)?;
                    if target.exists() {
                        ensure!(
                            args["expected_content"].as_str()
                                == Some(fs::read_to_string(&target)?.as_str()),
                            "Existing file changed or expected_content missing; read it before replacing"
                        );
                    }
                    project::write(&c.repo, file, text(args, "content")?)?;
                    "File written".into()
                };
                Ok(json!({"message":result}))
            }
            "command_status" | "command_input" | "stop_command" => {
                let id = text(args, "command_id")?;
                let job = {
                    let jobs = self.commands.lock().unwrap();
                    let Some((owner, job)) = jobs.get(id) else {
                        return crate::command_session::manage(id, name, args);
                    };
                    ensure!(
                        owner == session,
                        "Command belongs to another operator session"
                    );
                    job.clone()
                };
                let mut job = job.lock().unwrap();
                if name == "stop_command" {
                    job.terminate(args["reason"].as_str().unwrap_or("Stopped by operator"))?;
                }
                if name == "command_input" {
                    job.input(args)?;
                }
                job.poll(
                    args["wait_ms"].as_u64().unwrap_or(0).min(1000),
                    &AtomicBool::new(false),
                )
            }
            _ => {
                let version = if name == "read_file" {
                    Some(config_revision(&project::safe_path(
                        &c.repo,
                        text(args, "path")?,
                    )?)?)
                } else {
                    None
                };
                let result = runner::inspect_tool(
                    &c.repo,
                    name,
                    args,
                    &mut crate::web_tools::Research::default(),
                )?;
                if let Some(version) = version {
                    ensure!(
                        version
                            == config_revision(&project::safe_path(&c.repo, text(args, "path")?)?)?,
                        "File changed while reading; try again"
                    );
                    return Ok(
                        json!({"text":result,"version":version,"observed_at":chrono::Utc::now()}),
                    );
                }
                Ok(serde_json::from_str(&result).unwrap_or(json!({"text":result})))
            }
        }
    }
}
pub fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().with_context(|| format!("Missing {key}"))
}
pub fn config_revision(path: &Path) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fs::read(path)?.hash(&mut h);
    Ok(format!("{:x}", h.finish()))
}
fn check_revision(path: &Path, args: &Value) -> Result<()> {
    ensure!(
        args["expected_revision"].as_str() == Some(config_revision(path)?.as_str()),
        "Settings changed or expected_revision missing; call get_settings first"
    );
    Ok(())
}
fn history(c: &runner::Config, args: &Value) -> Result<Value> {
    let name = args["artifact"].as_str().unwrap_or("conversation.json");
    ensure!(
        !name.contains("request-")
            && !name.contains("configuration")
            && !name.starts_with("operator/operations"),
        "Raw request/settings artifacts are not served by history"
    );
    let relative = Path::new(name);
    ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "Use a project history artifact path"
    );
    let path = c.state_dir.join(relative);
    let canonical = fs::canonicalize(&path)?;
    ensure!(
        canonical.starts_with(fs::canonicalize(&c.state_dir)?),
        "History path leaves project state"
    );
    let content = fs::read_to_string(canonical)?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    ensure!(
        offset <= content.len() && content.is_char_boundary(offset),
        "Invalid UTF-8 offset"
    );
    let mut end = (offset + 12000).min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    Ok(
        json!({"artifact":name,"text":&content[offset..end],"next_offset":if end<content.len(){Some(end)}else{None}}),
    )
}
pub fn is_mutation(name: &str) -> bool {
    ![
        "project_status",
        "operation_status",
        "list_pending_actions",
        "list_commands",
        "get_settings",
        "read_history",
        "list_history",
        "preview_restore",
        "project_diff",
        "begin_edit",
        "command_status",
        "read_file",
        "search",
        "list_files",
        "project_map",
        "lookup_symbol",
        "web_search",
        "read_web_page",
    ]
    .contains(&name)
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required}}})
}
pub fn schemas() -> Value {
    let common = crate::model::tools();
    let allowed = [
        "read_file",
        "search",
        "list_files",
        "project_map",
        "lookup_symbol",
        "edit_file",
        "write_file",
        "run_command",
        "run_checks",
        "command_status",
        "command_input",
        "stop_command",
        "web_search",
        "read_web_page",
    ];
    let mut all: Vec<Value> = common
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| allowed.contains(&t["function"]["name"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    for t in &mut all {
        if t["function"]["name"] == "edit_file" {
            t["function"]["parameters"]["properties"]["expected_version"] =
                json!({"type":"string","description":"version returned by read_file"});
            t["function"]["parameters"]["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("expected_version"));
        }
        if t["function"]["name"] == "write_file" {
            t["function"]["parameters"]["properties"]["expected_content"] = json!({"type":"string","description":"Exact current content required when replacing an existing file"});
        }
    }
    all.extend([
        tool("acknowledge_controller_restart","After an unexpected controller exit, acknowledge user-verified recovery. Inspect files and command logs and confirm old commands have stopped; never assumes an unrecorded action succeeded or replays it.",json!({"previous_commands_stopped":{"type":"boolean"},"summary":{"type":"string"}}),&["previous_commands_stopped","summary"]),
        tool("recover_edit_session","Recover an interrupted editing session only after the user confirms its external writers have stopped. Retains files, revokes the old session, and saves a recovery checkpoint. Inspect project_status first.",json!({"previous_session":{"type":"string"},"external_writers_stopped":{"type":"boolean"},"summary":{"type":"string"}}),&["previous_session","external_writers_stopped","summary"]),
        tool("operator_note","Deliver a concise user direction to the loop at its next safe boundary without replacing the overall goal.",json!({"note":{"type":"string"}}),&["note"]),
        tool("list_history","List saved progress, chat, command and cycle artifacts with timestamps. Optional path/text filter and pagination.",json!({"query":{"type":"string"},"offset":{"type":"integer"}}),&[]),
        tool("preview_restore","Preview changes from restoring a saved Git tree or recovery reference. Does not change files.",json!({"target":{"type":"string"}}),&["target"]),
        tool("restore_checkpoint","Apply a user-requested restoration after showing its preview. Requires editing ownership; preserves a recovery backup and refuses human staging.",json!({"target":{"type":"string"},"expected_tree":{"type":"string"},"confirmed":{"type":"boolean"}}),&["target","expected_tree","confirmed"]),
        tool("list_commands","Inspect running and recent commands from the loop and operator sessions. Use returned command_id to inspect output, send input or explicitly stop one.",json!({}),&[]),
        tool("list_pending_actions","List active or uncertain operator actions with session and operation IDs. Resume the originating session to inspect or acknowledge a completed recovery; never blindly retry uncertain commands.",json!({}),&[]),
        tool("operation_status","Read a previously submitted action without replaying it.",json!({"id":{"type":"string"}}),&["id"]),
        tool("resolve_interrupted_action","After inspecting an uncertain action and its files/logs, acknowledge the result and release its recovery hold. Never retries the action.",json!({"id":{"type":"string"},"summary":{"type":"string"}}),&["id","summary"]),
        tool("reopen_nudge","Reopen the most recent finished or cancelled nudge at the user’s request.",json!({}),&[]),
        tool("commit_changes","Commit requested work while preserving human staging. Does not complete the loop task or goal.",json!({"summary":{"type":"string"}}),&["summary"]),
        tool("project_status","Read current loop state, goal, task, nudges, holds and editing owner.",json!({}),&[]),
        tool("begin_edit","Request exclusive editing for this session. Poll until granted before file edits or native shell tools. Keep it through all background writers.",json!({}),&[]),
        tool("end_edit","Return editing control after all writers finish. Save unfinished work and tell the loop what changed.",json!({"summary":{"type":"string"}}),&["summary"]),
        tool("pause_loop","Pause the loop after its current operation; existing commands can finish.",json!({}),&[]),
        tool("resume_loop","Resume the loop. scheduled respects hours; until_close overrides hours until the next closing; one_cycle runs one cycle now.",json!({"mode":{"type":"string","enum":["scheduled","until_close","one_cycle"]}}),&["mode"]),
        tool("stop_loop","Finish the current cycle and stop, or stop safely if already paused.",json!({}),&[]),
        tool("retry_provider","Retry a provider now without bypassing a pause or schedule.",json!({}),&[]),
        tool("get_settings","Read settings and revision before changing them.",json!({}),&[]),
        tool("update_settings","Change normal project settings using the exact revision. Credentials/provider trust stay in human settings.",json!({"expected_revision":{"type":"string"},"settings":{"type":"object"}}),&["expected_revision","settings"]),
        tool("set_goal","Revise the overall goal at the user's request, preserving work and history.",json!({"goal":{"type":"string"},"expected_revision":{"type":"string"}}),&["goal","expected_revision"]),
        tool("set_nudge","Set a temporary user priority within the overall goal. Supply the current nudge ID when replacing one.",json!({"request":{"type":"string"},"expected_nudge_id":{"type":"integer"}}),&["request"]),
        tool("cancel_nudge","Cancel the specified active nudge.",json!({"nudge_id":{"type":"integer"}}),&["nudge_id"]),
        tool("read_history","Read project history or command artifacts in pages. Default is loop conversation.json.",json!({"artifact":{"type":"string"},"offset":{"type":"integer"}}),&[]),
        tool("project_diff","Read the current diff and Git status without changing staging.",json!({}),&[]),
        tool("save_checkpoint","Save current unfinished work for recovery, without making a normal commit.",json!({"summary":{"type":"string"}}),&["summary"]),
    ]);
    json!(all)
}

fn compact_state(v: &Value) -> Value {
    json!({"cycle":v["cycle"],"current_task":v["current_task"],"working_branch":v["working_branch"],"working_ref":v["working_ref"],"feedback":v["feedback"],"recent":v["recent"].as_array().map(|a|a.iter().rev().take(3).collect::<Vec<_>>()),"history":"state.json"})
}
fn restore_target(c: &runner::Config, target: &str) -> Result<String> {
    ensure!(
        !target.starts_with('-') && target.len() < 256,
        "Invalid recovery reference"
    );
    Ok(project::git(
        &c.repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{target}^{{tree}}"),
        ],
    )?
    .trim()
    .into())
}
fn list_history(c: &runner::Config, args: &Value) -> Result<Value> {
    let mut pending = vec![c.state_dir.clone()];
    let mut files = Vec::new();
    let query = args["query"].as_str().unwrap_or("").to_lowercase();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let e = entry?;
            let kind = e.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let p = e.path();
            let name = p.strip_prefix(&c.state_dir)?.to_string_lossy();
            if name.contains("request-")
                || name.contains("configuration")
                || name.starts_with("operator/operations")
                || name.starts_with("migration")
                || name.starts_with("working")
            {
                continue;
            }
            if kind.is_dir() {
                pending.push(p);
            } else if kind.is_file()
                && (query.is_empty()
                    || name.to_lowercase().contains(&query)
                    || (e.metadata()?.len() < 100000
                        && fs::read_to_string(&p).is_ok_and(|s| s.to_lowercase().contains(&query))))
            {
                let meta = e.metadata()?;
                files.push(json!({"artifact":name,"bytes":meta.len(),"modified":meta.modified().ok().map(chrono::DateTime::<chrono::Utc>::from)}));
            }
        }
    }
    files.sort_by(|a, b| b["modified"].as_str().cmp(&a["modified"].as_str()));
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let items: Vec<_> = files.iter().skip(offset).take(50).collect();
    Ok(
        json!({"artifacts":items,"next_offset":if offset.saturating_add(50)<files.len(){Some(offset+50)}else{None}}),
    )
}
