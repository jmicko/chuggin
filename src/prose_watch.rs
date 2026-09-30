//! Cross-response prose repetition. Arguments, source code and short progress
//! labels are not reasoning. File changes clear suspicion; elapsed time does not.
use serde::{Deserialize, Serialize};
use serde_json::Value;

const HISTORY: usize = 384;
const SKETCH: usize = 512;
const REPEATS: usize = 4;

#[derive(Clone, Serialize, Deserialize)]
struct Signature {
    words: usize,
    exact: u64,
    /// The smallest distinct five-word hashes: bounded storage, with the entire
    /// prose scanned rather than just its prefix or a few randomly chosen words.
    shingles: Vec<u64>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProseWatch {
    recent: Vec<Signature>,
    warned: Option<Signature>,
    warned_patterns: Vec<Signature>,
    initialized: bool,
    notice_pending: bool,
    pub pending_refresh: bool,
    pub interventions: u64,
}

#[derive(Debug, PartialEq)]
pub enum Intervention {
    Notice,
    Refresh,
}

impl ProseWatch {
    pub fn progress(&mut self) {
        self.recent.clear();
        self.warned = None;
        self.warned_patterns.clear();
        self.notice_pending = false;
        self.pending_refresh = false;
        self.initialized = true;
    }

    pub fn take_notice(&mut self) -> bool {
        std::mem::take(&mut self.notice_pending)
    }

    pub fn suspected(&self) -> bool {
        self.warned.is_some() || self.pending_refresh
    }

    pub fn observe(&mut self, prose: &str, changed: bool) -> Option<Intervention> {
        self.initialized = true;
        if changed {
            self.progress();
            return None;
        }
        if self.pending_refresh {
            return None;
        }
        let signature = signature(prose)?;
        let matches = self
            .recent
            .iter()
            .rev()
            .take(HISTORY)
            .filter(|old| similar(old, &signature))
            .count();
        self.recent.push(signature.clone());
        if self.recent.len() > HISTORY {
            self.recent.remove(0);
        }
        if matches + 1 < REPEATS {
            return None;
        }
        let escalated = self
            .warned
            .as_ref()
            .is_some_and(|old| similar(old, &signature))
            || self
                .warned_patterns
                .iter()
                .any(|old| similar(old, &signature));
        self.warned = Some(signature.clone());
        if !escalated {
            self.warned_patterns.push(signature);
            if self.warned_patterns.len() > HISTORY {
                self.warned_patterns.remove(0);
            }
        }
        // Give the model another complete set of attempts after the notice.
        self.recent.clear();
        self.interventions = self.interventions.saturating_add(1);
        if escalated {
            self.pending_refresh = true;
            Some(Intervention::Refresh)
        } else {
            self.notice_pending = true;
            Some(Intervention::Notice)
        }
    }

    /// Old installations did not save this detector. Examine the existing
    /// transcript once on cold resume so an already-stuck run is not blindly
    /// continued. A potentially mutating tool breaks this retrospective streak:
    /// its effects cannot safely be reconstructed from narration alone.
    pub fn bootstrap(&mut self, messages: &[Value]) {
        if self.initialized {
            return;
        }
        for message in messages
            .iter()
            .rev()
            .take(384)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            if message["role"] != "assistant" {
                continue;
            }
            let mutating = message["tool_calls"].as_array().is_some_and(|calls| {
                calls.iter().any(|call| {
                    !matches!(
                        call["function"]["name"].as_str().unwrap_or(""),
                        "read_file"
                            | "search"
                            | "list_files"
                            | "project_map"
                            | "lookup_symbol"
                            | "read_progress_note"
                            | "read_task_evidence"
                            | "search_history"
                            | "read_history"
                            | "read_agent_result"
                            | "read_command_log"
                            | "command_status"
                            | "view_image"
                    )
                })
            });
            self.observe(message["content"].as_str().unwrap_or(""), mutating);
        }
        self.initialized = true;
    }
}

fn hash(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn signature(text: &str) -> Option<Signature> {
    if text.trim_start().starts_with(['{', '[']) {
        return None;
    }
    let mut fenced = false;
    let mut words = Vec::new();
    let mut letters = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.ends_with(';') || line.starts_with(['{', '[', '}']) {
            continue;
        }
        letters.extend(
            line.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase),
        );
        words.extend(
            line.split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(|w| hash(w.to_lowercase().bytes())),
        );
    }
    // Routine labels such as "Let me read the next page" legitimately recur.
    if text.len() < 180 {
        return None;
    }
    // Scripts without spaces need character shingles instead of word shingles.
    if words.len() < 32 {
        if letters.len() < 64 || !letters.iter().any(|c| !c.is_ascii()) {
            return None;
        }
        words = letters
            .iter()
            .map(|c| hash(c.to_string().bytes()))
            .collect();
    }
    let exact = hash(words.iter().flat_map(|w| w.to_le_bytes()));
    let mut shingles: Vec<_> = words
        .windows(5)
        .map(|window| hash(window.iter().flat_map(|w| w.to_le_bytes())))
        .collect();
    shingles.sort_unstable();
    shingles.dedup();
    shingles.truncate(SKETCH);
    Some(Signature {
        words: words.len(),
        exact,
        shingles,
    })
}

fn similar(a: &Signature, b: &Signature) -> bool {
    if a.exact == b.exact {
        return true;
    }
    if a.words.min(b.words) * 4 < a.words.max(b.words) * 3 {
        return false;
    }
    // Estimate Jaccard similarity using the smallest hashes of the union. This
    // tolerates punctuation, a changing line number, or a small rephrased clause
    // without equating two different investigations sharing a generic preamble.
    let (mut i, mut j, mut union, mut shared) = (0, 0, 0, 0);
    while union < SKETCH && (i < a.shingles.len() || j < b.shingles.len()) {
        match (a.shingles.get(i), b.shingles.get(j)) {
            (Some(x), Some(y)) if x == y => {
                i += 1;
                j += 1;
                shared += 1;
            }
            (Some(x), Some(y)) if x < y => i += 1,
            (Some(_), Some(_)) => j += 1,
            (Some(_), None) => i += 1,
            (None, Some(_)) => j += 1,
            (None, None) => break,
        }
        union += 1;
    }
    union >= 16 && shared * 100 >= union * 80
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LOOP: &str = "I've now read through the entire editor. Let me analyze the key usability concerns for the active nudge. The most important one I can identify is a concrete bug in the render loop. The document body is rendered inside a ScrollArea, but the caret is drawn at absolute paragraph positions. Let me verify the actual typing and rendering path works by checking the keyboard integration test and confirming the render produces visible text.";

    #[test]
    fn repeated_reasoning_across_calls_warns_then_recovers_after_restart() {
        let mut watch = ProseWatch::default();
        for _ in 0..3 {
            assert_eq!(watch.observe(LOOP, false), None);
        }
        assert_eq!(watch.observe(LOOP, false), Some(Intervention::Notice));
        let mut watch: ProseWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        assert!(watch.take_notice());
        for _ in 0..3 {
            assert_eq!(watch.observe(LOOP, false), None);
        }
        assert_eq!(watch.observe(LOOP, false), Some(Intervention::Refresh));
        assert!(watch.pending_refresh);
        assert_eq!(watch.interventions, 2);
    }

    #[test]
    fn punctuation_and_minor_variants_do_not_disguise_repeated_reasoning() {
        let mut watch = ProseWatch::default();
        for i in 0..3 {
            assert_eq!(
                watch.observe(&format!("{LOOP} Next read is line {}.", i * 80), false),
                None
            );
        }
        assert_eq!(
            watch.observe(
                &format!("{} Next read is line 320.", LOOP.replace('.', "!")),
                false
            ),
            Some(Intervention::Notice)
        );
    }

    #[test]
    fn code_short_labels_and_real_file_changes_are_not_a_reasoning_loop() {
        let mut watch = ProseWatch::default();
        for _ in 0..20 {
            assert_eq!(watch.observe("Let me read the next page.", false), None);
            assert_eq!(watch.observe(&format!("```rust\n{LOOP}\n```"), false), None);
            assert_eq!(
                watch.observe(&json!({"new_text":LOOP}).to_string(), false),
                None
            );
            assert_eq!(watch.observe(LOOP, true), None);
        }
        assert!(!watch.suspected());
    }

    #[test]
    fn different_findings_with_shared_introductions_are_allowed() {
        let mut watch = ProseWatch::default();
        for i in 0..32 {
            let findings = (0..80)
                .map(|j| format!("finding_{i}_{j}"))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                watch.observe(
                    &format!("I've now read through the entire editor. {findings}"),
                    false
                ),
                None
            );
        }
    }

    #[test]
    fn cold_resume_detects_old_stuck_transcript_without_replaying_tools() {
        let mut watch: ProseWatch = serde_json::from_value(json!({})).unwrap();
        let messages: Vec<_> = (0..12).map(|i| json!({"role":"assistant","content":LOOP,"tool_calls":[{"function":{"name":"read_file","arguments":{"path":"editor.rs","start_line":i*80+1}}}]})).collect();
        watch.bootstrap(&messages);
        assert!(watch.pending_refresh);
        let count = watch.interventions;
        watch.bootstrap(&messages);
        assert_eq!(watch.interventions, count);
    }

    #[test]
    fn non_space_delimited_prose_is_checked_too() {
        let text = "我已经检查过整个项目，现在需要确认下一步应该怎样实施。当前的界面仍然存在问题，需要查看编辑器的渲染逻辑以及光标显示方式，然后检查文本输入与显示是否一致。接下来我将继续检查源文件并确认测试结果，再决定应该修改哪个部分。";
        let mut watch = ProseWatch::default();
        for _ in 0..3 {
            assert_eq!(watch.observe(text, false), None);
        }
        assert_eq!(watch.observe(text, false), Some(Intervention::Notice));
    }

    #[test]
    fn alternating_warning_patterns_cannot_avoid_recovery() {
        let other = "The research sources show a discrepancy between the two reported measurements. I should compare the original data and determine which assumptions explain the disagreement. After that comparison I can select a justified approach and record the evidence that supports the next calculation. I will inspect the reference material before deciding which result to use.";
        let mut watch = ProseWatch::default();
        for text in [LOOP, other] {
            for _ in 0..3 {
                assert_eq!(watch.observe(text, false), None);
            }
            assert_eq!(watch.observe(text, false), Some(Intervention::Notice));
        }
        for _ in 0..3 {
            assert_eq!(watch.observe(LOOP, false), None);
        }
        assert_eq!(watch.observe(LOOP, false), Some(Intervention::Refresh));
    }
}
