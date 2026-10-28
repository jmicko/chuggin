//! Cross-response prose repetition. Tool arguments and source code are excluded.
//! Short labels accompanying tools are allowed; repeated text-only outputs are
//! checked separately. New evidence clears suspicion; elapsed time does not.
use serde::{Deserialize, Serialize};
use serde_json::Value;

const HISTORY: usize = 384;
const SKETCH: usize = 512;
const REPEATS: usize = 4;
const SHORT_HISTORY: usize = 16;
const SHORT_REPEATS: usize = 8;
const SHORT_CHARACTERS: usize = 180;

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
    /// Text-only responses have no tool result to distinguish an acknowledgement
    /// from idle narration. Require an exact normalized recurrence, more often
    /// and within a smaller window than substantial reasoning.
    short_recent: Vec<Option<u64>>,
    short_warned_patterns: Vec<u64>,
    short_notice_pending: bool,
    short_refresh_pending: bool,
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
        self.short_progress();
        self.notice_pending = false;
        self.pending_refresh = false;
        self.initialized = true;
    }

    pub fn take_notice(&mut self) -> bool {
        std::mem::take(&mut self.notice_pending) | std::mem::take(&mut self.short_notice_pending)
    }

    /// New observations and completed actions justify the same brief status
    /// again, while substantial reasoning may still be repeating. Do not call
    /// this for unchanged rereads or merely submitting a pending tool call.
    pub fn short_progress(&mut self) {
        self.short_recent.clear();
        self.short_warned_patterns.clear();
        self.short_notice_pending = false;
        if std::mem::take(&mut self.short_refresh_pending) {
            self.pending_refresh = false;
        }
    }

    #[cfg(test)]
    pub fn suspected(&self) -> bool {
        self.warned.is_some() || !self.short_warned_patterns.is_empty() || self.pending_refresh
    }

    /// Check user-facing output, accounting for whether the response requested
    /// tools. A brief label attached to pending tools is not an idle reply.
    /// Call `short_progress` when intervening tools supply new observations or
    /// complete an action; unchanged rereads must not clear this history.
    pub fn observe_response(
        &mut self,
        prose: &str,
        changed: bool,
        has_tool_calls: bool,
    ) -> Option<Intervention> {
        let intervention = self.observe(prose, changed);
        if intervention.is_some() || changed || self.pending_refresh || has_tool_calls {
            return intervention;
        }
        if prose.trim().is_empty() {
            return None;
        }
        let short = short_signature(prose);
        self.short_recent.push(short);
        if self.short_recent.len() > SHORT_HISTORY {
            self.short_recent.remove(0);
        }
        let short = short?;
        if self
            .short_recent
            .iter()
            .filter(|old| **old == Some(short))
            .count()
            < SHORT_REPEATS
        {
            return None;
        }
        let escalated = self.short_warned_patterns.contains(&short);
        if !escalated {
            self.short_warned_patterns.push(short);
            if self.short_warned_patterns.len() > HISTORY {
                self.short_warned_patterns.remove(0);
            }
        }
        self.short_recent.clear();
        Some(self.intervene(escalated, true))
    }

    /// Substantial prose only: internal thinking and existing callers retain
    /// their original behavior. Use `observe_response` for assistant content.
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
        Some(self.intervene(escalated, false))
    }

    fn intervene(&mut self, escalated: bool, short: bool) -> Intervention {
        self.interventions = self.interventions.saturating_add(1);
        if escalated {
            self.pending_refresh = true;
            self.short_refresh_pending = short;
            Intervention::Refresh
        } else {
            if short {
                self.short_notice_pending = true;
            } else {
                self.notice_pending = true;
            }
            Intervention::Notice
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
            let has_tool_calls = message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
            self.observe_response(
                message["content"].as_str().unwrap_or(""),
                mutating,
                has_tool_calls,
            );
        }
        self.initialized = true;
    }
}

fn hash(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn prose_words(text: &str) -> Option<(Vec<u64>, Vec<char>)> {
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
    Some((words, letters))
}

fn substantial_words(text: &str, words: &[u64], letters: &[char]) -> bool {
    text.len() >= 180
        && (words.len() >= 32 || (letters.len() >= 64 && letters.iter().any(|c| !c.is_ascii())))
}

fn short_signature(text: &str) -> Option<u64> {
    if text.chars().take(SHORT_CHARACTERS + 1).count() > SHORT_CHARACTERS {
        return None;
    }
    let (words, letters) = prose_words(text)?;
    if words.is_empty() || substantial_words(text, &words, &letters) {
        return None;
    }
    Some(hash(words.iter().flat_map(|w| w.to_le_bytes())))
}

fn signature(text: &str) -> Option<Signature> {
    let (mut words, letters) = prose_words(text)?;
    // Routine labels accompanying tools legitimately recur.
    if !substantial_words(text, &words, &letters) {
        return None;
    }
    // Scripts without spaces need character shingles instead of word shingles.
    if words.len() < 32 {
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

/// Locate assistant narration to quarantine during a repetition handoff. Match
/// the same normalized prose as the live watcher, including earlier members of
/// each repeated group; changing punctuation or a line number must not leave
/// the original loop in the selected history. Tool arguments and user text are
/// never candidates. The original message indices remain archive references.
pub(crate) fn repeated_narration_indices(messages: &[Value]) -> Vec<usize> {
    enum Candidate {
        Substantial(Signature),
        Short(u64),
    }
    let mut recent = Vec::<(usize, Candidate)>::new();
    let mut repeated = std::collections::BTreeSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "assistant" {
            continue;
        }
        let Some(prose) = message["content"].as_str() else {
            continue;
        };
        let candidate = if let Some(signature) = signature(prose) {
            Candidate::Substantial(signature)
        } else if !message["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
            && let Some(short) = short_signature(prose)
        {
            Candidate::Short(short)
        } else {
            continue;
        };
        let matches: Vec<_> = recent
            .iter()
            .filter(|(_, old)| match (old, &candidate) {
                (Candidate::Substantial(a), Candidate::Substantial(b)) => similar(a, b),
                (Candidate::Short(a), Candidate::Short(b)) => a == b,
                _ => false,
            })
            .map(|(index, _)| *index)
            .collect();
        let threshold = match candidate {
            Candidate::Substantial(_) => REPEATS,
            Candidate::Short(_) => SHORT_REPEATS,
        };
        if matches.len() + 1 >= threshold {
            repeated.extend(matches);
            repeated.insert(index);
        }
        recent.push((index, candidate));
        if recent.len() > HISTORY {
            recent.remove(0);
        }
    }
    repeated.into_iter().collect()
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

    #[test]
    fn repeated_short_text_only_responses_warn_then_refresh_after_restart() {
        let mut watch = ProseWatch::default();
        for _ in 0..SHORT_REPEATS - 1 {
            assert_eq!(
                watch.observe_response("All complete. Standing by.", false, false),
                None
            );
        }
        assert_eq!(
            watch.observe_response("ALL COMPLETE! Standing by", false, false),
            Some(Intervention::Notice)
        );
        assert!(watch.suspected());
        let mut watch: ProseWatch =
            serde_json::from_value(serde_json::to_value(watch).unwrap()).unwrap();
        assert!(watch.take_notice());
        for _ in 0..SHORT_REPEATS - 1 {
            assert_eq!(
                watch.observe_response("All complete. Standing by.", false, false),
                None
            );
        }
        assert_eq!(
            watch.observe_response("All complete. Standing by.", false, false),
            Some(Intervention::Refresh)
        );
        assert!(watch.pending_refresh);
        assert_eq!(watch.interventions, 2);
    }

    #[test]
    fn repeated_short_outputs_are_language_and_wording_independent() {
        for text in [
            "Done.",
            "D'accord, j'attends.",
            "完了しました。待機しています。",
        ] {
            let mut watch = ProseWatch::default();
            for _ in 0..SHORT_REPEATS - 1 {
                assert_eq!(watch.observe_response(text, false, false), None);
            }
            assert_eq!(
                watch.observe_response(text, false, false),
                Some(Intervention::Notice)
            );
        }
    }

    #[test]
    fn short_labels_with_pending_tools_are_not_text_only_replies() {
        let mut watch = ProseWatch::default();
        for _ in 0..100 {
            assert_eq!(
                watch.observe_response("Let me read the next page.", false, true),
                None
            );
        }
        assert!(!watch.suspected());
        for _ in 0..SHORT_REPEATS - 1 {
            assert_eq!(
                watch.observe_response("Let me read the next page.", false, false),
                None
            );
        }
        assert_eq!(
            watch.observe_response("Let me read the next page.", false, false),
            Some(Intervention::Notice)
        );
    }

    #[test]
    fn fresh_evidence_allows_repeated_brief_acknowledgements() {
        let mut watch = ProseWatch::default();
        for _ in 0..100 {
            assert_eq!(watch.observe_response("Done.", false, false), None);
            // A new observation or completed action arrives between replies.
            watch.short_progress();
        }
        assert!(!watch.suspected());
        for _ in 0..SHORT_REPEATS {
            watch.observe_response("Done.", false, false);
        }
        assert!(watch.suspected());
        assert_eq!(watch.observe_response("Done.", true, false), None);
        assert!(!watch.suspected());
        assert!(!watch.take_notice());
    }

    #[test]
    fn short_progress_cancels_short_recovery_but_preserves_repeated_reasoning() {
        let mut watch = ProseWatch::default();
        for _ in 0..SHORT_REPEATS {
            watch.observe_response("Done.", false, false);
        }
        watch.short_progress();
        assert!(!watch.take_notice());
        assert!(!watch.suspected());
        for _ in 0..SHORT_REPEATS * 2 {
            watch.observe_response("Done.", false, false);
        }
        assert!(watch.pending_refresh);
        watch.short_progress();
        assert!(!watch.pending_refresh);

        for _ in 0..REPEATS {
            watch.observe(LOOP, false);
        }
        watch.short_progress();
        assert!(watch.take_notice());
        assert!(watch.suspected());
        for _ in 0..REPEATS {
            watch.observe(LOOP, false);
        }
        watch.short_progress();
        assert!(watch.pending_refresh);
    }

    #[test]
    fn short_outputs_with_new_values_and_sparse_recurrences_are_allowed() {
        let mut watch = ProseWatch::default();
        for i in 0..100 {
            assert_eq!(
                watch.observe_response(&format!("Observed value {i}."), false, false),
                None
            );
            if i % 3 == 0 {
                assert_eq!(watch.observe_response("Done.", false, false), None);
            }
        }
        assert!(!watch.suspected());
    }

    #[test]
    fn short_recurrences_survive_unchanged_read_labels_and_alternating_patterns() {
        let mut watch = ProseWatch::default();
        let mut refreshed = false;
        for i in 0..SHORT_REPEATS * 8 {
            assert_eq!(
                watch.observe_response("Checking current state.", false, true),
                None
            );
            let text = if i % 2 == 0 { "Ready." } else { "Waiting." };
            if watch.observe_response(text, false, false) == Some(Intervention::Refresh) {
                refreshed = true;
                break;
            }
            watch.take_notice();
        }
        assert!(refreshed);
    }

    #[test]
    fn code_and_tool_arguments_do_not_become_short_output_loops() {
        let mut watch = ProseWatch::default();
        for _ in 0..100 {
            for text in ["```rust\nlet done = true;\n```", "{\"done\":true}", "x();"] {
                assert_eq!(watch.observe_response(text, false, false), None);
            }
        }
        assert!(!watch.suspected());
    }

    #[test]
    fn cold_resume_checks_short_text_only_outputs_but_not_pending_tool_labels() {
        let messages: Vec<_> = (0..SHORT_REPEATS * 2)
            .map(|_| json!({"role":"assistant","content":"All complete. Standing by."}))
            .collect();
        let mut watch = ProseWatch::default();
        watch.bootstrap(&messages);
        assert!(watch.pending_refresh);

        let messages: Vec<_> = (0..100)
            .map(|_| json!({"role":"assistant","content":"Reading.","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"file.rs"}}}]}))
            .collect();
        let mut watch = ProseWatch::default();
        watch.bootstrap(&messages);
        assert!(!watch.suspected());
    }

    #[test]
    fn saved_older_detector_state_and_thinking_keep_existing_behavior() {
        let mut watch: ProseWatch = serde_json::from_value(json!({
            "recent":[], "warned":null, "warned_patterns":[], "initialized":true,
            "notice_pending":false, "pending_refresh":false, "interventions":0
        }))
        .unwrap();
        for _ in 0..100 {
            assert_eq!(watch.observe("Done.", false), None);
        }
        for _ in 0..REPEATS - 1 {
            assert_eq!(watch.observe_response(LOOP, false, false), None);
        }
        assert_eq!(
            watch.observe_response(LOOP, false, false),
            Some(Intervention::Notice)
        );
    }

    #[test]
    fn handoff_quarantines_all_near_duplicate_narration_but_preserves_unique_outputs() {
        let mut messages = vec![json!({"role":"user","content":LOOP})];
        for index in 0..REPEATS {
            messages.push(json!({"role":"assistant","content":format!("{LOOP} Next read is line {}.", index * 80)}));
        }
        let expected: Vec<_> = (1..=REPEATS).collect();
        messages.push(
            json!({"role":"assistant","content":"A unique observation about a different sample."}),
        );
        for _ in 0..SHORT_REPEATS {
            messages.push(json!({"role":"assistant","content":"Done.","tool_calls":[{"id":"pending","function":{"name":"read_file","arguments":{}}}]}));
            messages.push(json!({"role":"assistant","content":"```rust\nlet done = true;\n```"}));
            messages.push(json!({"role":"assistant","content":"{\"done\":true}"}));
        }
        assert_eq!(repeated_narration_indices(&messages), expected);
        let first_short = messages.len();
        for _ in 0..SHORT_REPEATS * 2 {
            messages.push(json!({"role":"assistant","content":"Done."}));
        }
        let expected: Vec<_> = expected
            .into_iter()
            .chain(first_short..messages.len())
            .collect();
        assert_eq!(repeated_narration_indices(&messages), expected);
    }
}
