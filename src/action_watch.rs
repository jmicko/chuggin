//! Conservative detection of repeated tool actions with identical results.
//! New evidence, file changes, and commands count as progress; unchanged files
//! alone mean nothing. Persist only bounded fingerprints, never file contents.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Four complete passes through up to 2,048 distinct actions can be recognized.
// In particular, a large source file paginated in 80-line reads is not a short
// period loop. The separate coverage history tolerates reordering and repaging.
const ACTION_HISTORY_LIMIT: usize = 8192;
const READ_SOURCE_LIMIT: usize = 64;
const READ_UNIT_LIMIT: usize = 16_384;
const TOTAL_READ_UNIT_LIMIT: usize = 65_536;
const STALE_PATTERN_LIMIT: usize = 1024;
const MIN_STALE_ACTIONS: u64 = 48;
const BYTE_UNIT_SIZE: usize = 64;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionWatch {
    // Retain these original fields so old conversations deserialize and an
    // outstanding warning can still escalate after a restart.
    recent: Vec<u64>,
    warned_pattern: Vec<u64>,
    notice_pending: bool,
    pub pending_refresh: bool,
    pub interventions: u64,
    known_actions: Vec<u64>,
    read_sources: Vec<ReadSource>,
    stale_actions: Vec<u64>,
    stale_read_sources: Vec<u64>,
    stale_other_actions: Vec<u64>,
    stale_read_calls: u64,
    stale_read_units: u64,
    stale_count: u64,
    stale_warned: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ReadSource {
    path: u64,
    bytes: bool,
    total: u64,
    revision: Option<u64>,
    units: BTreeMap<u128, ReadUnit>,
}

#[derive(Serialize, Deserialize)]
struct ReadUnit {
    length: u64,
    hash: u64,
}

struct ReadObservation {
    path: u64,
    bytes: bool,
    total: u64,
    revision: Option<u64>,
    units: Vec<(u64, ReadUnit)>,
    signature: u64,
    text: Option<(u64, String)>,
}

#[derive(Debug, PartialEq)]
pub enum Intervention {
    Notice,
    Refresh,
}

impl ActionWatch {
    pub fn progress(&mut self) {
        self.recent.clear();
        self.known_actions.clear();
        self.read_sources.clear();
        self.clear_suspicion();
    }

    fn clear_suspicion(&mut self) {
        self.warned_pattern.clear();
        self.notice_pending = false;
        self.pending_refresh = false;
        self.stale_actions.clear();
        self.stale_read_sources.clear();
        self.stale_other_actions.clear();
        self.stale_read_calls = 0;
        self.stale_read_units = 0;
        self.stale_count = 0;
        self.stale_warned = false;
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
                    | "read_task_evidence"
                    | "read_history"
                    | "search_history"
                    | "read_agent_result"
                    | "read_command_log"
                    | "search"
                    | "list_files"
                    | "project_map"
                    | "lookup_symbol"
            )
            || (name == "finish_task"
                && (output["completion_recorded"] == false || result["ok"] == false));
        if !watch {
            if matches!(name, "save_progress_note" | "command_status") {
                // Narration and passive status polls do not erase already read
                // evidence, and a long-running process may legitimately wait.
                return None;
            }
            // Commands can have arbitrary side effects. Do not infer that identical
            // stdout means the project or an external process is unchanged.
            if matches!(
                name,
                "edit_file" | "write_file" | "set_task" | "finish_task"
            ) {
                self.progress();
            } else {
                self.recent.clear();
                self.clear_suspicion();
            }
            return None;
        }
        let fingerprint_args = if name == "finish_task" {
            &Value::Null
        } else {
            args
        };
        let fingerprint = hash(serde_json::json!([name, fingerprint_args, result]).to_string());

        // Older saved conversations have only `recent` and `warned_pattern`.
        // Their existing evidence remains useful even before coverage is rebuilt.
        if self.known_actions.is_empty() {
            self.known_actions.extend(self.recent.iter().copied());
            self.known_actions
                .extend(self.warned_pattern.iter().copied());
            self.known_actions.sort_unstable();
            self.known_actions.dedup();
        }
        let known = self.known_actions.contains(&fingerprint);
        let read = (name == "read_file" && result["ok"] != false)
            .then(|| read_observation(args, &output))
            .flatten();
        let mut read_measure = None;
        let (new_evidence, stale_signature) = if let Some(read) = read {
            let signature = read.signature;
            read_measure = Some((source_id(read.path, read.bytes), read.units.len() as u64));
            // Rebuilding absent coverage from legacy state is not new evidence
            // if that exact result was already observed. A changed known line
            // still counts as evidence, including a revert to older contents.
            let (novel, changed) = self.observe_read(read);
            (changed || (novel && !known), signature)
        } else {
            (!known, fingerprint)
        };
        if !known {
            self.known_actions.push(fingerprint);
        }
        trim_front(&mut self.known_actions, ACTION_HISTORY_LIMIT);
        if new_evidence {
            self.clear_suspicion();
        } else {
            self.stale_count = self.stale_count.saturating_add(1);
            if !self.stale_actions.contains(&stale_signature)
                && self.stale_actions.len() < STALE_PATTERN_LIMIT
            {
                self.stale_actions.push(stale_signature);
            }
            if let Some((source, units)) = read_measure {
                if !self.stale_read_sources.contains(&source) {
                    self.stale_read_sources.push(source);
                    trim_front(&mut self.stale_read_sources, READ_SOURCE_LIMIT);
                }
                self.stale_read_calls = self.stale_read_calls.saturating_add(1);
                self.stale_read_units = self.stale_read_units.saturating_add(units);
            } else if !self.stale_other_actions.contains(&stale_signature)
                && self.stale_other_actions.len() < STALE_PATTERN_LIMIT
            {
                self.stale_other_actions.push(stale_signature);
            }
        }

        self.recent.push(fingerprint);
        trim_front(&mut self.recent, ACTION_HISTORY_LIMIT);
        // Test candidate periods against the last action first. Most fail here;
        // only plausible candidates require a full comparison of their tails.
        for period in 1..=self.recent.len() / 4 {
            // The first pass may contain new evidence. The three following
            // passes must be stale; genuinely changing A/B contents are progress.
            if self.stale_count < period as u64 * 3 {
                continue;
            }
            let end = self.recent.len();
            if self.recent[end - 1] != self.recent[end - 1 - period] {
                continue;
            }
            let tail = &self.recent[end - period * 4..];
            if !tail.chunks(period).all(|chunk| chunk == &tail[..period]) {
                continue;
            }
            return self.intervene(tail[..period].to_vec());
        }

        // Reordered or differently paginated reads can evade exact cycles. Allow
        // several rereads of the observed set, and demand a sustained run with
        // no new line content, query, result, or source before intervening.
        let mut allowance = self.stale_actions.len() as u64 * 3;
        if self.stale_read_units > 0 {
            let covered_units: u64 = self
                .read_sources
                .iter()
                .filter(|source| {
                    self.stale_read_sources
                        .contains(&source_id(source.path, source.bytes))
                })
                .map(|source| source.units.len() as u64)
                .sum();
            // Estimate a pass from stable covered content and the average page
            // size. Changing every page boundary cannot inflate this allowance.
            let passes = covered_units
                .saturating_mul(self.stale_read_calls)
                .div_ceil(self.stale_read_units);
            let coverage_allowance = passes
                .saturating_add(self.stale_other_actions.len() as u64)
                .saturating_mul(3);
            allowance = allowance.min(coverage_allowance);
        }
        let allowance = MIN_STALE_ACTIONS.max(allowance);
        if self.stale_count >= allowance {
            return self.intervene(self.stale_actions.clone());
        }
        None
    }

    fn observe_read(&mut self, read: ReadObservation) -> (bool, bool) {
        let source = self
            .read_sources
            .iter()
            .position(|source| source.path == read.path && source.bytes == read.bytes);
        let existed = source.is_some();
        let mut source = source
            .map(|index| self.read_sources.remove(index))
            .unwrap_or_default();
        let changed = source.total != read.total || source.revision != read.revision;
        if changed {
            source.units.clear();
        }
        source.path = read.path;
        source.bytes = read.bytes;
        source.total = read.total;
        source.revision = read.revision;
        let mut new_evidence = changed;
        let mut changed_content = changed && existed;
        if let Some((offset, text)) = &read.text {
            let end = offset.saturating_add(text.len() as u64);
            // A larger excerpt can verify a previously observed partial block.
            // This detects content reverts across different byte page lengths.
            let overlap_changed = source.units.iter().any(|(key, unit)| {
                let position = (*key >> 64) as u64;
                position >= *offset
                    && position.saturating_add(unit.length) <= end
                    && unit.hash
                        != hash(
                            &text.as_bytes()[(position - offset) as usize
                                ..(position - offset + unit.length) as usize],
                        )
            });
            if overlap_changed {
                source.units.retain(|key, unit| {
                    let position = (*key >> 64) as u64;
                    position >= end || position.saturating_add(unit.length) <= *offset
                });
                new_evidence = true;
                changed_content = true;
            }
        }
        for (position, unit) in read.units {
            let key = (u128::from(position) << 64) | u128::from(unit.length);
            let unchanged = source
                .units
                .get(&key)
                .is_some_and(|old| old.hash == unit.hash && old.length == unit.length);
            if !unchanged {
                changed_content |= source.units.contains_key(&key);
                new_evidence = true;
                source.units.insert(key, unit);
            }
        }
        while source.units.len() > READ_UNIT_LIMIT {
            source.units.pop_first();
        }
        // Keep recently read sources. Evicted coverage can only make a later
        // inspection appear new, so the bounds favor avoiding false positives.
        self.read_sources.push(source);
        while self.read_sources.len() > READ_SOURCE_LIMIT
            || self
                .read_sources
                .iter()
                .map(|source| source.units.len())
                .sum::<usize>()
                > TOTAL_READ_UNIT_LIMIT
        {
            self.read_sources.remove(0);
        }
        (new_evidence, changed_content)
    }

    fn intervene(&mut self, mut pattern: Vec<u64>) -> Option<Intervention> {
        if self.pending_refresh {
            return None;
        }
        pattern.sort_unstable();
        pattern.dedup();
        let repeated = self.stale_warned || self.warned_pattern == pattern;
        self.warned_pattern = pattern;
        self.stale_warned = true;
        self.recent.clear();
        self.stale_actions.clear();
        self.stale_read_sources.clear();
        self.stale_other_actions.clear();
        self.stale_read_calls = 0;
        self.stale_read_units = 0;
        self.stale_count = 0;
        self.interventions += 1;
        if repeated {
            self.pending_refresh = true;
            Some(Intervention::Refresh)
        } else {
            self.notice_pending = true;
            Some(Intervention::Notice)
        }
    }

    pub fn recovered(&mut self) {
        self.progress();
    }
}

fn trim_front<T>(items: &mut Vec<T>, limit: usize) {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
}

// Stable FNV-1a fingerprint; this is a heuristic, never an authorization or
// integrity decision. No source text is retained in persisted observations.
fn hash(input: impl AsRef<[u8]>) -> u64 {
    input.as_ref().iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

fn source_id(path: u64, bytes: bool) -> u64 {
    hash(serde_json::json!([path, bytes]).to_string())
}

fn read_observation(args: &Value, output: &Value) -> Option<ReadObservation> {
    let path = args["path"].as_str()?;
    let text = output.as_str().or_else(|| output["text"].as_str())?;
    let revision = output.get("version").map(|value| hash(value.to_string()));
    let mut units = Vec::new();
    let mut byte_text = None;
    let (bytes, total) = if let Some(offset) = output["byte_offset"].as_u64() {
        let total = output["total_bytes"].as_u64()?;
        byte_text = Some((offset, text.to_owned()));
        let mut position = offset;
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            let length = (BYTE_UNIT_SIZE - position as usize % BYTE_UNIT_SIZE).min(remaining.len());
            units.push((
                position,
                ReadUnit {
                    length: length as u64,
                    hash: hash(&remaining[..length]),
                },
            ));
            position = position.checked_add(length as u64)?;
            remaining = &remaining[length..];
        }
        (true, total)
    } else {
        // read_lines returns a numbered, explicitly bounded excerpt. Only parse
        // that documented format; arbitrary text with a line-like prefix is not
        // evidence that a requested range was actually returned.
        let (header, shown) = text.split_once('\n')?;
        let (total, start) = header
            .strip_prefix(&format!("{path}: "))?
            .split_once(" lines total; showing from line ")?;
        let total: u64 = total.parse().ok()?;
        let start: u64 = start.parse().ok()?;
        for line in shown.lines() {
            let Some((number, content)) = line.split_once(": ") else {
                continue;
            };
            let Ok(position) = number.parse::<u64>() else {
                continue;
            };
            if position >= start && position <= total {
                units.push((
                    position,
                    ReadUnit {
                        length: 1,
                        hash: hash(content),
                    },
                ));
            }
        }
        (false, total)
    };
    if units.is_empty() {
        return None;
    }
    let path = hash(path);
    let signature = hash(
        serde_json::json!([path, bytes, units.first()?.0, units.last()?.0, &units]).to_string(),
    );
    Some(ReadObservation {
        path,
        bytes,
        total,
        revision,
        units,
        signature,
        text: byte_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(path: &str, total: u64, start: u64, count: u64, revision: u64) -> Value {
        let mut text = format!("{path}: {total} lines total; showing from line {start}\n");
        for line in start..=total.min(start + count - 1) {
            text.push_str(&format!(
                "{line}: source fixture {revision} at line {line}\n"
            ));
        }
        json!({"ok":true,"result":text})
    }

    fn page(watch: &mut ActionWatch, start: u64, count: u64) -> Option<Intervention> {
        watch.observe(
            "read_file",
            &json!({"path":"src/editor.rs","start_line":start,"line_count":count}),
            &lines("src/editor.rs", 6321, start, count, 1),
        )
    }

    #[test]
    fn alternating_actual_contents_are_progress() {
        let mut watch = ActionWatch::default();
        let args = json!({"path":"src/editor.rs","start_line":1,"line_count":80});
        for revision in (0..40).map(|index| index % 2) {
            assert_eq!(
                watch.observe(
                    "read_file",
                    &args,
                    &lines("src/editor.rs", 6321, 1, 80, revision)
                ),
                None
            );
        }
        assert!(!watch.pending_refresh);
        assert!(!watch.take_notice());
    }

    #[test]
    fn changing_all_page_boundaries_does_not_extend_the_read_budget() {
        let mut watch = ActionWatch::default();
        for start in (1..=6321).step_by(80) {
            assert_eq!(page(&mut watch, start, 80), None);
        }
        let mut warned = false;
        let mut requests = 0;
        for count in 81..=84 {
            for start in (1..=6321).step_by(count as usize) {
                requests += 1;
                let intervention = watch.observe(
                    "read_file",
                    &json!({"path":"src/editor.rs","start_line":start,"line_count":count,"request":requests}),
                    &lines("src/editor.rs", 6321, start, count, 1),
                );
                assert_ne!(intervention, Some(Intervention::Refresh));
                warned |= intervention == Some(Intervention::Notice);
            }
        }
        assert!(warned);
        assert!(requests < 320);
    }

    #[test]
    fn mixed_old_reads_and_searches_repeat_without_a_fixed_cycle() {
        let mut watch = ActionWatch::default();
        let mut warned = false;
        for pass in 0..4 {
            for index in 0..80 {
                let start = ((index * 37 + pass) % 80) * 80 + 1;
                let intervention = watch.observe(
                    "read_file",
                    &json!({"path":"src/editor.rs","start_line":start,"line_count":80,"request":pass * 80 + index}),
                    &lines("src/editor.rs", 6321, start, 80, 1),
                );
                warned |= intervention == Some(Intervention::Notice);
                assert_ne!(intervention, Some(Intervention::Refresh));
                if index % 10 == 0 {
                    let intervention = watch.observe(
                        "search",
                        &json!({"text":index}),
                        &json!({"ok":true,"result":"same source matches"}),
                    );
                    warned |= intervention == Some(Intervention::Notice);
                    assert_ne!(intervention, Some(Intervention::Refresh));
                }
            }
        }
        assert!(warned);
    }

    #[test]
    fn passive_polls_and_note_writes_preserve_read_history() {
        let mut watch = ActionWatch::default();
        for pass in 0..4 {
            for start in (1..=6321).step_by(80) {
                let intervention = page(&mut watch, start, 80);
                assert_eq!(
                    intervention,
                    (pass == 3 && start == 6321).then_some(Intervention::Notice)
                );
                assert_eq!(
                    watch.observe(
                        "command_status",
                        &json!({"command_id":"running"}),
                        &json!({"ok":true,"result":"waiting"})
                    ),
                    None
                );
                assert_eq!(
                    watch.observe(
                        "save_progress_note",
                        &json!({"note":format!("reading pass {pass} at {start}")}),
                        &json!({"ok":true,"result":"saved"})
                    ),
                    None
                );
            }
        }
        assert!(watch.take_notice());
    }

    #[test]
    fn commands_reset_suspicion_but_preserve_read_coverage() {
        let mut watch = ActionWatch::default();
        for _ in 0..4 {
            page(&mut watch, 1, 80);
        }
        assert!(watch.take_notice());
        assert_eq!(
            watch.observe(
                "run_checks",
                &json!({}),
                &json!({"ok":true,"result":"checks passed"})
            ),
            None
        );
        assert!(!watch.stale_warned);
        assert_eq!(watch.read_sources.len(), 1);
        assert_eq!(page(&mut watch, 1, 80), None);
        assert_eq!(watch.stale_count, 1);
        watch.progress();
        assert!(watch.read_sources.is_empty());
    }

    #[test]
    fn different_byte_page_lengths_are_known_and_reverts_are_progress() {
        let mut watch = ActionWatch::default();
        for request in 0..52 {
            let count = 127 + request % 2;
            let intervention = watch.observe(
                "read_file",
                &json!({"path":"source.txt","byte_offset":0,"request":request}),
                &json!({"ok":true,"result":{
                    "byte_offset":0,"total_bytes":128,"text":"a".repeat(count)
                }}),
            );
            assert_ne!(intervention, Some(Intervention::Refresh));
        }
        assert!(watch.take_notice());
        let args = json!({"path":"source.txt","byte_offset":0});
        let full =
            json!({"ok":true,"result":{"byte_offset":0,"total_bytes":128,"text":"a".repeat(128)}});
        let partial_changed = json!({"ok":true,"result":{"byte_offset":0,"total_bytes":128,"text":format!("{}b", "a".repeat(126))}});
        assert_eq!(watch.observe("read_file", &args, &partial_changed), None);
        assert_eq!(watch.observe("read_file", &args, &full), None);
        assert!(!watch.stale_warned);
        assert!(!watch.pending_refresh);
        // Composite range keys must remain serializable in conversation JSON.
        let mut watch: ActionWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        assert_eq!(watch.observe("read_file", &args, &full), None);
        assert_eq!(watch.stale_count, 1);
    }

    #[test]
    fn whole_file_pagination_escalates_across_restarts() {
        let mut watch = ActionWatch::default();
        for _ in 0..3 {
            for start in (1..=6321).step_by(80) {
                assert_eq!(page(&mut watch, start, 80), None);
            }
        }
        let mut watch: ActionWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        for start in (1..=6321).step_by(80) {
            let intervention = page(&mut watch, start, 80);
            assert_eq!(
                intervention,
                (start == 6321).then_some(Intervention::Notice)
            );
        }
        assert!(watch.take_notice());
        let mut watch: ActionWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        let mut refreshed = false;
        for _ in 0..3 {
            for start in (1..=6321).step_by(80) {
                if page(&mut watch, start, 80) == Some(Intervention::Refresh) {
                    refreshed = true;
                    break;
                }
            }
            if refreshed {
                break;
            }
        }
        assert!(refreshed);
        assert!(watch.pending_refresh);
        assert_eq!(watch.interventions, 2);
    }

    #[test]
    fn reordered_reads_and_irrelevant_argument_changes_still_repeat() {
        let mut watch = ActionWatch::default();
        for start in (1..=6321).step_by(80) {
            assert_eq!(page(&mut watch, start, 80), None);
        }
        let mut warned = false;
        // Each pass changes the ordering and a harmless extra argument, so no
        // four identical action sequences can exist in the raw history.
        for pass in 0..3 {
            for index in 0..80 {
                let start = ((index * 37 + pass) % 80) * 80 + 1;
                let intervention = watch.observe(
                    "read_file",
                    &json!({"path":"src/editor.rs","start_line":start,"line_count":80,"request":pass * 80 + index}),
                    &lines("src/editor.rs", 6321, start, 80, 1),
                );
                if intervention == Some(Intervention::Notice) {
                    warned = true;
                } else {
                    assert_eq!(intervention, None);
                }
            }
        }
        assert!(warned);
    }

    #[test]
    fn overlapping_pagination_is_new_only_when_it_contains_new_lines() {
        let mut watch = ActionWatch::default();
        // Every initial overlapping page contributes new evidence.
        for start in (1..=6241).step_by(40) {
            assert_eq!(page(&mut watch, start, 120), None);
        }
        assert_eq!(page(&mut watch, 6281, 41), None);
        let mut warned = false;
        for request in 0..72 {
            let count = 20 + request % 20;
            let start = 800 + request % 5;
            let intervention = watch.observe(
                "read_file",
                &json!({"path":"src/editor.rs","start_line":start,"line_count":count,"request":request}),
                &lines("src/editor.rs", 6321, start, count, 1),
            );
            warned |= intervention == Some(Intervention::Notice);
        }
        assert!(warned);
    }

    #[test]
    fn changed_content_and_scoped_research_clear_suspicion() {
        let mut watch = ActionWatch::default();
        for _ in 0..3 {
            assert_eq!(page(&mut watch, 1, 80), None);
        }
        assert_eq!(page(&mut watch, 1, 80), Some(Intervention::Notice));
        assert_eq!(
            watch.observe(
                "read_file",
                &json!({"path":"src/editor.rs","start_line":1,"line_count":80}),
                &lines("src/editor.rs", 6321, 1, 80, 2),
            ),
            None
        );
        assert!(!watch.take_notice());
        assert!(!watch.pending_refresh);
        for query in 0..100 {
            assert_eq!(
                watch.observe(
                    "search",
                    &json!({"text":query}),
                    &json!({"ok":true,"result":"no matches"})
                ),
                None
            );
        }
        for _ in 0..4 {
            let intervention = page(&mut watch, 1, 80);
            assert_ne!(intervention, Some(Intervention::Refresh));
        }
        assert!(watch.take_notice());
    }

    #[test]
    fn reverting_previously_observed_content_is_progress() {
        let mut watch = ActionWatch::default();
        let args = json!({"path":"src/editor.rs","start_line":1,"line_count":80});
        let first = lines("src/editor.rs", 6321, 1, 80, 1);
        let second = lines("src/editor.rs", 6321, 1, 80, 2);
        assert_eq!(watch.observe("read_file", &args, &first), None);
        for _ in 0..3 {
            assert_eq!(watch.observe("read_file", &args, &second), None);
        }
        assert_eq!(
            watch.observe("read_file", &args, &second),
            Some(Intervention::Notice)
        );
        assert_eq!(watch.observe("read_file", &args, &first), None);
        assert!(!watch.take_notice());
        assert!(!watch.stale_warned);
        assert!(!watch.pending_refresh);
    }

    #[test]
    fn byte_reads_ignore_wrapper_changes_and_detect_content_changes() {
        let mut watch = ActionWatch::default();
        let args = json!({"path":"source.txt","byte_offset":0});
        for request in 0..48 {
            assert_eq!(
                watch.observe(
                    "read_file",
                    &args,
                    &json!({"ok":true,"result":{
                        "path":"source.txt", "byte_offset":0, "total_bytes":128,
                        "text":"a".repeat(128), "observed_at":request
                    }})
                ),
                None
            );
        }
        assert_eq!(
            watch.observe(
                "read_file",
                &args,
                &json!({"ok":true,"result":{
                    "path":"source.txt", "byte_offset":0, "total_bytes":128,
                    "text":"a".repeat(128), "observed_at":48
                }})
            ),
            Some(Intervention::Notice)
        );
        assert_eq!(
            watch.observe(
                "read_file",
                &args,
                &json!({"ok":true,"result":{
                    "path":"source.txt", "byte_offset":0, "total_bytes":128,
                    "text":format!("{}b", "a".repeat(127)), "observed_at":49
                }})
            ),
            None
        );
        assert!(!watch.take_notice());
        assert!(!watch.stale_warned);
    }

    #[test]
    fn legacy_conversations_preserve_outstanding_warning() {
        let args = json!({"text":"same"});
        let result = json!({"ok":true,"result":"same matching lines"});
        let fingerprint = hash(json!(["search", &args, &result]).to_string());
        let mut watch: ActionWatch = serde_json::from_value(json!({
            "recent":[], "warned_pattern":[fingerprint], "notice_pending":false,
            "pending_refresh":false, "interventions":1
        }))
        .unwrap();
        for _ in 0..3 {
            assert_eq!(watch.observe("search", &args, &result), None);
        }
        assert_eq!(
            watch.observe("search", &args, &result),
            Some(Intervention::Refresh)
        );
        assert_eq!(watch.interventions, 2);
        assert!(serde_json::from_value::<ActionWatch>(json!({})).is_ok());
    }

    #[test]
    fn coverage_is_bounded_and_contains_no_file_text() {
        let mut watch = ActionWatch::default();
        for file in 0..READ_SOURCE_LIMIT + 10 {
            let path = format!("source-{file}.rs");
            assert_eq!(
                watch.observe(
                    "read_file",
                    &json!({"path":path}),
                    &lines(&path, 80, 1, 80, 1)
                ),
                None
            );
        }
        assert_eq!(watch.read_sources.len(), READ_SOURCE_LIMIT);
        let serialized = serde_json::to_string(&watch).unwrap();
        assert!(!serialized.contains("source fixture"));
        assert!(!serialized.contains("source-0.rs"));
        watch.recovered();
        assert!(watch.read_sources.is_empty());
        assert!(watch.known_actions.is_empty());
    }

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
