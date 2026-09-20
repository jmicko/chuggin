//! Conservative detection of repeated tool actions with identical results.
//! File changes and commands reset observations; unchanged files alone mean nothing.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionWatch {
    recent: Vec<u64>,
    warned_pattern: Vec<u64>,
    notice_pending: bool,
    pub pending_refresh: bool,
    pub interventions: u64,
}

#[derive(Debug, PartialEq)]
pub enum Intervention {
    Notice,
    Refresh,
}

impl ActionWatch {
    pub fn progress(&mut self) {
        self.recent.clear();
        self.warned_pattern.clear();
        self.notice_pending = false;
        self.pending_refresh = false;
    }
    pub fn take_notice(&mut self) -> bool {
        std::mem::take(&mut self.notice_pending)
    }

    pub fn observe(&mut self, name: &str, args: &Value, result: &Value) -> Option<Intervention> {
        let output: Value = result["result"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| result["result"].clone());
        let unchanged_edit = matches!(name, "edit_file" | "write_file")
            && (result["ok"] == false || output["changed"] == false);
        let watch = unchanged_edit
            || matches!(
                name,
                "read_file"
                    | "read_progress_note"
                    | "search"
                    | "list_files"
                    | "project_map"
                    | "lookup_symbol"
            )
            || (name == "finish_task"
                && (output["completion_recorded"] == false || result["ok"] == false));
        if !watch {
            // Commands can have arbitrary side effects. Do not infer that identical
            // stdout means the project or an external process is unchanged.
            self.progress();
            return None;
        }
        // Stable FNV-1a fingerprint; this is a heuristic, never an authorization
        // or integrity decision. Keep no file contents in persisted observations.
        let args = if name == "finish_task" {
            &Value::Null
        } else {
            args
        };
        let input = serde_json::json!([name, args, result]).to_string();
        let fingerprint = input.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        self.recent.push(fingerprint);
        if self.recent.len() > 12 {
            self.recent.remove(0);
        }
        for period in 1..=3 {
            let needed = period * 4;
            if self.recent.len() < needed {
                continue;
            }
            let tail = &self.recent[self.recent.len() - needed..];
            if !tail.chunks(period).all(|chunk| chunk == &tail[..period]) {
                continue;
            }
            let mut pattern = tail[..period].to_vec();
            pattern.sort_unstable();
            pattern.dedup();
            let repeated = self.warned_pattern == pattern;
            self.warned_pattern = pattern;
            self.recent.clear();
            self.interventions += 1;
            if repeated {
                self.pending_refresh = true;
                return Some(Intervention::Refresh);
            }
            self.notice_pending = true;
            return Some(Intervention::Notice);
        }
        None
    }

    pub fn recovered(&mut self) {
        self.progress();
        self.pending_refresh = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeated_actions_escalate_across_restart() {
        let mut watch = ActionWatch::default();
        let args = json!({"text":"same"});
        let result = json!({"ok":true,"result":"same matching lines"});
        for _ in 0..3 {
            assert_eq!(watch.observe("search", &args, &result), None);
        }
        assert_eq!(
            watch.observe("search", &args, &result),
            Some(Intervention::Notice)
        );
        let mut watch: ActionWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        for _ in 0..3 {
            assert_eq!(watch.observe("search", &args, &result), None);
        }
        assert_eq!(
            watch.observe("search", &args, &result),
            Some(Intervention::Refresh)
        );
        assert!(watch.pending_refresh);
        assert_eq!(watch.interventions, 2);
    }

    #[test]
    fn distinct_investigation_and_side_effects_are_not_stagnation() {
        let mut watch = ActionWatch::default();
        for line in 0..100 {
            assert_eq!(
                watch.observe(
                    "read_file",
                    &json!({"start_line":line}),
                    &json!({"ok":true,"result":"same text"})
                ),
                None
            );
        }
        for _ in 0..20 {
            assert_eq!(
                watch.observe(
                    "read_file",
                    &json!({"path":"a"}),
                    &json!({"ok":true,"result":"text"})
                ),
                None
            );
            assert_eq!(
                watch.observe(
                    "write_file",
                    &json!({"path":"a"}),
                    &json!({"ok":true,"result":"{\"changed\":true}"})
                ),
                None
            );
        }
        for _ in 0..20 {
            assert_eq!(
                watch.observe(
                    "run_command",
                    &json!({"argv":["poll"]}),
                    &json!({"ok":true,"result":"waiting"})
                ),
                None
            );
        }
    }

    #[test]
    fn detects_alternating_noops_but_not_changed_results() {
        let mut watch = ActionWatch::default();
        for i in 0..7 {
            assert_eq!(
                watch.observe(
                    "edit_file",
                    &json!({"path":i%2}),
                    &json!({"ok":true,"result":"{\"changed\":false}"})
                ),
                None
            );
        }
        assert_eq!(
            watch.observe(
                "edit_file",
                &json!({"path":1}),
                &json!({"ok":true,"result":"{\"changed\":false}"})
            ),
            Some(Intervention::Notice)
        );
        watch.recovered();
        for i in 0..20 {
            assert_eq!(
                watch.observe(
                    "read_file",
                    &json!({"path":"a"}),
                    &json!({"ok":true,"result":i})
                ),
                None
            );
        }
    }
}
