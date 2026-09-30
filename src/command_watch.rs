//! Repeated commands are a diagnostic signal, never proof of a stalled task.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandWatch {
    key: String,
    pub count: u64,
    next_diagnosis: u64,
    pub notice_pending: bool,
    pub pending_diagnosis: bool,
    pub diagnoses: u64,
    pub command: Value,
    pub recent: Vec<Value>,
    pub activity: Vec<Value>,
    // Command processes are not reattached after a restart. Only completed
    // observations belong in saved recovery state.
    #[serde(skip)]
    pending: std::collections::BTreeMap<String, PendingCommand>,
}

struct PendingCommand {
    name: String,
    args: Value,
    before: Option<String>,
    task_id: u64,
}

impl CommandWatch {
    pub fn record_activity(&mut self, name: &str, args: &Value, result: &Value) {
        self.activity.push(json!({"tool":name,"arguments_excerpt":crate::project::excerpt(&args.to_string(),600),"result_excerpt":crate::project::excerpt(&result.to_string(),600)}));
        if self.activity.len() > 12 {
            self.activity.remove(0);
        }
    }
    pub fn reset_streak(&mut self) {
        self.key.clear();
        self.count = 0;
        self.next_diagnosis = 16;
        self.notice_pending = false;
        self.pending_diagnosis = false;
        self.recent.clear();
        self.command = Value::Null;
    }

    pub fn has_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    /// Capture the original request before a command can complete asynchronously.
    /// IDs include their cycle so a later cycle cannot reuse an interrupted job.
    pub fn started(
        &mut self,
        id: &str,
        name: &str,
        args: &Value,
        before: Option<&str>,
        task_id: u64,
    ) {
        if let Some((cycle, _)) = id.rsplit_once('/') {
            let prefix = format!("{cycle}/");
            self.pending.retain(|key, _| key.starts_with(&prefix));
        }
        self.pending
            .entry(id.to_owned())
            .or_insert_with(|| PendingCommand {
                name: name.to_owned(),
                args: args.clone(),
                before: before.map(str::to_owned),
                task_id,
            });
    }

    /// Consume a completion once, regardless of whether it arrived through the
    /// initial reply, a status poll, automatic monitoring, or final draining.
    pub fn completed(
        &mut self,
        id: &str,
        output: &Value,
        after: Option<&str>,
        current_task_id: u64,
    ) -> bool {
        if output["running"] != false {
            return false;
        }
        let Some(command) = self.pending.remove(id) else {
            return false;
        };
        if command.task_id != current_task_id {
            self.reset_streak();
        } else {
            self.observe(
                &command.name,
                &command.args,
                &json!({"ok":true,"result":output}),
                command.before.as_deref(),
                after,
                command.task_id,
            );
        }
        true
    }

    pub fn observe(
        &mut self,
        name: &str,
        args: &Value,
        result: &Value,
        before: Option<&str>,
        after: Option<&str>,
        task_id: u64,
    ) {
        let (Some(before), Some(after)) = (before, after) else {
            self.reset_streak();
            return;
        };
        if before != after {
            self.reset_streak();
            return;
        }
        let mut args = args.clone();
        if let Some(encoded) = args["argv"].as_str()
            && let Ok(argv) = serde_json::from_str::<Vec<String>>(encoded)
        {
            args["argv"] = json!(argv);
        }
        let reason = args
            .as_object_mut()
            .and_then(|a| a.remove("reason"))
            .and_then(|r| r.as_str().map(|s| crate::project::excerpt(s, 800)));
        let output = &result["result"];
        let statuses =
            if let Some(checks) = output["checks"].as_array().or_else(|| output.as_array()) {
                json!(checks.iter().map(status).collect::<Vec<_>>())
            } else {
                status(output)
            };
        let key = json!([
            name,
            args,
            after,
            task_id,
            result["ok"],
            statuses,
            result["error"]
        ])
        .to_string();
        if key != self.key {
            self.reset_streak();
            self.key = key;
            self.command = json!({"tool":name,"arguments":args});
        }
        self.count = self.count.saturating_add(1);
        self.recent.push(json!({"reason":reason,"project_tree":after,"status":statuses,"ok":result["ok"],"error":result["error"],"log_id":output["log_id"],"result_excerpt":crate::project::excerpt(&output.to_string(),1600)}));
        if self.recent.len() > 6 {
            self.recent.remove(0);
        }
        if self.count == 8 {
            self.notice_pending = true;
        }
        if self.count >= self.next_diagnosis.max(16) {
            self.pending_diagnosis = true;
        }
    }

    pub fn notice(&self) -> String {
        format!(
            "Command repetition check: the same command and outcome status have occurred {} times with the same observed project files. This alone does not prove a stall: external state, sampling, or intermittent failures can justify repetition. State what uncertainty another run will resolve; otherwise inspect a different relevant source or advance the task. Full command logs remain available.",
            self.count
        )
    }

    pub fn reviewed(&mut self, productive: bool) {
        self.pending_diagnosis = false;
        self.notice_pending = false;
        self.next_diagnosis = self.count.saturating_add(if productive { 64 } else { 16 });
    }
}

fn status(output: &Value) -> Value {
    json!({"exit_code":output["exit_code"],"passed":output["passed"],"timed_out":output["timed_out"]})
}

/// Keep successful repeated output concise, but always retain the full log.
pub fn compact_reply(value: &Value, count: u64) -> Value {
    let mut value = value.clone();
    let output = &mut value["result"];
    if count >= 2
        && output["passed"] == true
        && output["log_id"].is_string()
        && let Some(text) = output["output_tail"].as_str()
        && text.len() > 1200
    {
        let mut start = text.len().saturating_sub(1200);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        output["output_tail"] = json!(format!(
            "[Earlier output omitted; full log available]\n{}",
            &text[start..]
        ));
        output["output_compacted"] = json!(true);
        output["instruction"] = json!(
            "Repeated command with unchanged observed project files. This is a short excerpt, not a claim that all output is identical. Use read_command_log with log_id for complete evidence, including any new information."
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    fn result(text: &str) -> Value {
        json!({"ok":true,"result":{"passed":true,"exit_code":0,"timed_out":false,"output_tail":text,"log_id":"cycle-000001/command-1.log"}})
    }
    #[test]
    fn repetition_survives_restart_and_allows_legitimate_rechecks() {
        let mut w = CommandWatch::default();
        for i in 0..8 {
            w.observe(
                "run_command",
                &json!({"argv":["check"],"reason":i.to_string()}),
                &result(&i.to_string()),
                Some("a"),
                Some("a"),
                1,
            );
        }
        assert!(w.notice_pending);
        assert!(!w.pending_diagnosis);
        let mut w: CommandWatch = serde_json::from_value(serde_json::to_value(w).unwrap()).unwrap();
        for _ in 0..8 {
            w.observe(
                "run_command",
                &json!({"argv":"[\"check\"]"}),
                &result("passed"),
                Some("a"),
                Some("a"),
                1,
            );
        }
        assert!(w.pending_diagnosis);
        assert_eq!(w.count, 16);
        w.reviewed(false);
        for _ in 0..15 {
            w.observe(
                "run_command",
                &json!({"argv":["check"]}),
                &result("passed"),
                Some("a"),
                Some("a"),
                1,
            );
        }
        assert!(!w.pending_diagnosis);
        w.observe(
            "run_command",
            &json!({"argv":["check"]}),
            &result("passed"),
            Some("a"),
            Some("a"),
            1,
        );
        assert!(w.pending_diagnosis);
        w.observe(
            "run_command",
            &json!({"argv":["check"]}),
            &result("passed"),
            Some("b"),
            Some("b"),
            1,
        );
        assert_eq!(w.count, 1);
        assert!(!w.pending_diagnosis);
        w.observe(
            "run_command",
            &json!({"argv":["check"]}),
            &result("passed"),
            Some("b"),
            Some("c"),
            1,
        );
        assert_eq!(w.count, 0);
    }
    #[test]
    fn compact_success_preserves_evidence_and_never_hides_failure() {
        let full = result(&"evidence".repeat(1000));
        let short = compact_reply(&full, 2);
        assert_eq!(short["result"]["output_compacted"], true);
        assert_eq!(short["result"]["log_id"], full["result"]["log_id"]);
        assert_eq!(compact_reply(&full, 1), full);
        let mut fail = full;
        fail["result"]["passed"] = json!(false);
        assert_eq!(compact_reply(&fail, 99), fail);
    }

    #[test]
    fn asynchronous_completions_count_original_requests_once() {
        let mut watch = CommandWatch::default();
        let args = json!({"argv":["check"],"reason":"Observe current behavior"});
        for i in 0..16 {
            let id = format!("cycle-000001/{i}");
            watch.started(&id, "run_command", &args, Some("tree"), 7);
            assert!(!watch.completed(&id, &json!({"running":true}), Some("tree"), 7));
            assert_eq!(watch.count, i);
            let output = json!({"running":false,"passed":true,"exit_code":0,"timed_out":false,"log_id":format!("{id}.log"),"elapsed_seconds":i});
            assert!(watch.completed(&id, &output, Some("tree"), 7));
            assert_eq!(watch.count, i + 1);
            assert!(!watch.completed(&id, &output, Some("tree"), 7));
            assert_eq!(watch.count, i + 1);
        }
        assert_eq!(watch.command["tool"], "run_command");
        assert_eq!(watch.command["arguments"]["argv"], args["argv"]);
        assert!(watch.notice_pending);
        assert!(watch.pending_diagnosis);
    }

    #[test]
    fn completion_respects_changed_inputs_and_pending_tickets_survive_streak_resets() {
        let mut watch = CommandWatch::default();
        let args = json!({"argv":["check"]});
        let done = json!({"running":false,"passed":true,"exit_code":0});
        watch.started("cycle-000001/a", "run_command", &args, Some("a"), 1);
        watch.reset_streak();
        assert!(watch.completed("cycle-000001/a", &done, Some("a"), 1));
        assert_eq!(watch.count, 1);
        watch.started("cycle-000001/b", "run_command", &args, Some("a"), 1);
        assert!(watch.completed("cycle-000001/b", &done, Some("b"), 1));
        assert_eq!(watch.count, 0);
        watch.started("cycle-000001/c", "run_command", &args, Some("b"), 1);
        assert!(watch.completed("cycle-000001/c", &done, Some("b"), 2));
        assert_eq!(watch.count, 0);
        watch.started("cycle-000001/d", "run_command", &args, Some("b"), 2);
        let saved = serde_json::to_value(&watch).unwrap();
        assert!(saved.get("pending").is_none());
        let mut restored: CommandWatch = serde_json::from_value(saved).unwrap();
        assert!(!restored.completed("cycle-000001/d", &done, Some("b"), 2));
        watch.started("cycle-000002/d", "run_command", &args, Some("b"), 2);
        assert!(!watch.has_pending("cycle-000001/d"));
        assert!(watch.completed("cycle-000002/d", &done, Some("b"), 2));
        assert_eq!(watch.count, 1);
    }

    #[test]
    fn check_batches_compare_every_outcome() {
        let mut watch = CommandWatch::default();
        let args = json!({"checks":["first","second"]});
        let passed = json!({"running":false,"checks":[{"exit_code":0,"passed":true},{"exit_code":0,"passed":true}]});
        let failed = json!({"running":false,"checks":[{"exit_code":1,"passed":false},{"exit_code":0,"passed":true}]});
        for i in 0..3 {
            let id = format!("cycle-000001/{i}");
            watch.started(&id, "run_checks", &args, Some("tree"), 1);
            watch.completed(&id, &passed, Some("tree"), 1);
        }
        assert_eq!(watch.count, 3);
        watch.started("cycle-000001/failure", "run_checks", &args, Some("tree"), 1);
        watch.completed("cycle-000001/failure", &failed, Some("tree"), 1);
        assert_eq!(watch.count, 1);
    }
}
