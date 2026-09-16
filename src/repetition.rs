//! Detect sustained periodic prose at the streaming tail, not repeated code/data fields.

pub fn start(text: &str) -> Option<usize> {
    if text.trim_start().starts_with(['{', '[']) {
        return json_prose_start(text);
    }
    prose_start(text, true)
}

// Streaming JSON is often incomplete. Scan individual string values without
// joining separate fields; old/new source snippets legitimately repeat.
fn json_prose_start(text: &str) -> Option<usize> {
    let mut key = "";
    let mut offset = 0;
    let bytes = text.as_bytes();
    while offset < bytes.len() {
        if bytes[offset] != b'"' {
            offset += 1;
            continue;
        }
        let begin = offset + 1;
        offset = begin;
        while offset < bytes.len() {
            if bytes[offset] == b'\\' {
                offset = (offset + 2).min(bytes.len());
            } else if bytes[offset] == b'"' {
                break;
            } else {
                offset += 1;
            }
        }
        let end = offset;
        let closed = end < bytes.len();
        if closed && text[end + 1..].trim_start().starts_with(':') {
            key = &text[begin..end];
        } else if !matches!(key, "old_text" | "new_text" | "code" | "content")
            && let Some(start) = prose_start(&text[begin..end], false)
        {
            return Some(begin + start);
        }
        offset = (offset + 1).min(bytes.len());
    }
    None
}

fn prose_start(text: &str, skip_source_lines: bool) -> Option<usize> {
    // Only inspect the latest prose segment. Fenced code, JSON fields and obvious
    // source lines are not evidence of a reasoning loop.
    let mut offset = 0;
    let mut segment = 0;
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if fence {
            fenced = !fenced;
        }
        if fence
            || fenced
            || trimmed.starts_with(['"', '{', '['])
            || (skip_source_lines && line.contains(';'))
        {
            segment = offset + line.len();
        }
        offset += line.len();
    }
    let mut tail_start = text.len().saturating_sub(12000).max(segment);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = &text[tail_start..];
    let words: Vec<_> = tail.split_whitespace().collect();
    // Ignore up to one word at the end because a streaming chunk may end midway
    // through it. Require sustained repeats, not two coincidentally equal phrases.
    for end in (words.len().saturating_sub(1)..=words.len()).rev() {
        for period in 1..=512.min(end / 3) {
            let block = &words[end - period..end];
            let mut begin = end - period;
            let mut repeats = 1;
            while begin >= period && &words[begin - period..begin] == block {
                begin -= period;
                repeats += 1;
            }
            let minimum = if period == 1 {
                12
            } else if period >= 12 {
                3
            } else {
                4
            };
            if repeats < minimum {
                continue;
            }
            let first = words[begin].as_ptr() as usize - text.as_ptr() as usize;
            let last =
                words[end - 1].as_ptr() as usize - text.as_ptr() as usize + words[end - 1].len();
            if last - first >= 128 {
                return Some(first);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_words_phrases_and_sentence_cycles_with_partial_tail() {
        for cycle in [
            "again ",
            "I'll try that now. Wait, let me think about it. ",
            "是的 ",
        ] {
            let prefix = "The test identified the failing case.\n";
            let text = format!("{prefix}{}Wa", cycle.repeat(30));
            let start = start(&text).expect("loop detected");
            assert_eq!(&text[..start], prefix);
            assert!(text.is_char_boundary(start));
        }
    }

    #[test]
    fn permits_code_json_and_normal_repeated_terms() {
        assert!(start("Read the file. Edit the file. Test the file. Read the log.").is_none());
        assert!(start(&format!("```rust\n{}\n```", "let x = 1;\n".repeat(100))).is_none());
        assert!(
            start(&serde_json::json!({"values": vec!["Repeated field value"; 100]}).to_string())
                .is_none()
        );
        assert!(start(&"Normal repeated sentence. ".repeat(2)).is_none());
    }

    #[test]
    fn detects_loops_within_json_prose_without_confusing_source_replacements() {
        let sentence = "I'll try that now. Wait, let me think about it. ";
        assert!(start(&format!("{{\"why_now\":\"{}", sentence.repeat(10))).is_some());
        assert!(
            start(
                &serde_json::json!({"old_text":sentence.repeat(20),"new_text":sentence.repeat(20)})
                    .to_string()
            )
            .is_none()
        );
        assert!(start(&serde_json::json!({"reason":sentence.repeat(20)}).to_string()).is_some());
    }

    #[test]
    fn detects_prose_after_code_but_not_repetitions_in_old_history() {
        assert!(
            start(&format!(
                "```\n{}\n```\n{}",
                "echo code\n".repeat(100),
                "Keep thinking. ".repeat(20)
            ))
            .is_some()
        );
        assert!(
            start(&format!(
                "{}\nA new action follows this completed quotation.",
                "Quoted phrase. ".repeat(20)
            ))
            .is_none()
        );
    }

    #[test]
    #[ignore = "Read-only replay of local traces; set CHUGGIN_REPLAY_DIR to an experiment's state directory"]
    fn replay_saved_streams() {
        use std::fs;
        let root = std::env::var("CHUGGIN_REPLAY_DIR").expect("Set CHUGGIN_REPLAY_DIR");
        let mut known = 0;
        let mut caught = 0;
        let mut completed = 0;
        let mut flagged_completed = Vec::new();
        for cycle in fs::read_dir(root).unwrap().flatten() {
            let dir = cycle.path();
            if !dir.join("outcome.json").exists() {
                continue;
            }
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(sequence) = name
                    .strip_prefix("response-")
                    .and_then(|s| s.strip_suffix(".ndjson"))
                else {
                    continue;
                };
                let failed =
                    fs::read_to_string(dir.join(format!("request-failure-{sequence}.json")))
                        .unwrap_or_default()
                        .contains("Repeated model output");
                let mut prose = String::new();
                let mut thinking = String::new();
                let mut detected = false;
                let mut done = false;
                for line in fs::read_to_string(&path).unwrap().lines() {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    done |= v["done"] == true;
                    if !detected {
                        prose.push_str(v["message"]["content"].as_str().unwrap_or(""));
                        thinking.push_str(v["message"]["thinking"].as_str().unwrap_or(""));
                        detected = start(&prose).is_some() || start(&thinking).is_some();
                    }
                }
                known += usize::from(failed);
                caught += usize::from(failed && detected);
                completed += usize::from(done);
                if done && detected {
                    flagged_completed.push(path);
                }
            }
        }
        println!(
            "Previously detected loops: {caught}/{known}; completed streams: {completed}; newly flagged completed streams: {flagged_completed:?}"
        );
        assert!(known > 0, "No recorded repetition failures found");
        assert!(caught > 0, "Detector missed all historical loops");
    }
}
