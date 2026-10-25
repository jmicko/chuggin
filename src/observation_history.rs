//! Display history is independent of the model's working context. Reading it
//! never executes tools or restores model state.
use crate::{engine::Record, events::Event};
use anyhow::Result;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const FILE: &str = "live-events.jsonl";
const READ_BYTES: u64 = 8 * 1024 * 1024;

pub struct Journal(File);
impl Journal {
    pub fn open(dir: &Path) -> Result<(Self, Vec<Record>)> {
        fs::create_dir_all(dir)?;
        let path = dir.join(FILE);
        let empty = fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut journal = Self(options.open(&path)?);
        if empty {
            for record in legacy(dir)? {
                journal.append(&record)?;
            }
        } else {
            // A killed writer may leave a partial last line. Do not concatenate
            // the next event onto it; the reader ignores incomplete records.
            let mut file = File::open(&path)?;
            file.seek(SeekFrom::End(-1))?;
            let mut byte = [0];
            file.read_exact(&mut byte)?;
            if byte[0] != b'\n' {
                journal.0.write_all(b"\n")?;
            }
        }
        Ok((journal, read(dir)?))
    }
    pub fn append(&mut self, record: &Record) -> Result<()> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        self.0.write_all(&bytes)?;
        Ok(())
    }
}

pub fn read(dir: &Path) -> Result<Vec<Record>> {
    let path = dir.join(FILE);
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    for line in tail(file, READ_BYTES)?.split(|b| *b == b'\n') {
        if let Ok(record) = serde_json::from_slice::<Record>(line) {
            records.push(record);
        }
    }
    Ok(records)
}

fn tail(mut file: File, limit: u64) -> Result<Vec<u8>> {
    let start = file.metadata()?.len().saturating_sub(limit);
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1))?;
    }
    let mut reader = BufReader::new(file);
    if start > 0 {
        let mut previous = [0];
        reader.read_exact(&mut previous)?;
        if previous[0] != b'\n' {
            reader.read_until(b'\n', &mut Vec::new())?;
        }
    }
    let mut bytes = Vec::new();
    reader.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn number(path: &Path, prefix: &str, suffix: &str) -> Option<u64> {
    path.file_name()?
        .to_str()?
        .strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

/// One-time import for older versions which saved reasoning only in wire traces.
/// Read the newest trace window, in chronological order, without modifying it.
fn legacy(dir: &Path) -> Result<Vec<Record>> {
    let mut cycles: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && number(p, "cycle-", "").is_some())
        .collect();
    cycles.sort_by_key(|p| number(p, "cycle-", ""));
    let mut budget = READ_BYTES;
    let mut groups = Vec::new();
    for cycle in cycles.into_iter().rev() {
        let mut files: Vec<_> = fs::read_dir(&cycle)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| number(p, "response-", ".ndjson").is_some())
            .collect();
        files.sort_by_key(|p| number(p, "response-", ".ndjson"));
        let mut responses = Vec::new();
        for path in files.into_iter().rev() {
            if budget == 0 {
                break;
            }
            let file = File::open(&path)?;
            let size = file.metadata()?.len().min(budget);
            budget = budget.saturating_sub(size);
            let bytes = tail(file, size)?;
            let mut events = Vec::new();
            let mut native_calls = Vec::new();
            let mut chat = crate::openai::Stream::default();
            let mut responses_stream = crate::responses::Stream::default();
            let mut protocol = 0;
            for line in bytes.split(|b| *b == b'\n') {
                let Ok(line) = std::str::from_utf8(line) else {
                    continue;
                };
                let data = line.strip_prefix("data:").unwrap_or(line).trim();
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let frame = if value.get("choices").is_some() {
                    protocol = 1;
                    chat.frame(data).ok()
                } else if value.get("type").is_some_and(Value::is_string) {
                    protocol = 2;
                    responses_stream.frame(data).ok()
                } else {
                    Some(value)
                };
                if let Some(frame) = frame {
                    for field in ["thinking", "content"] {
                        if let Some(chunk) =
                            frame["message"][field].as_str().filter(|s| !s.is_empty())
                        {
                            let same = match events.last_mut() {
                                Some(Event::ThinkingDelta(text))
                                    if field == "thinking" && text.len() + chunk.len() <= 8192 =>
                                {
                                    text.push_str(chunk);
                                    true
                                }
                                Some(Event::Delta(text))
                                    if field == "content" && text.len() + chunk.len() <= 8192 =>
                                {
                                    text.push_str(chunk);
                                    true
                                }
                                _ => false,
                            };
                            if !same {
                                events.push(if field == "thinking" {
                                    Event::ThinkingDelta(chunk.into())
                                } else {
                                    Event::Delta(chunk.into())
                                });
                            }
                        }
                    }
                    if let Some(calls) = frame["message"]["tool_calls"].as_array() {
                        native_calls.extend(calls.clone());
                    }
                }
            }
            let calls = match protocol {
                1 => chat.calls().unwrap_or_default(),
                2 => responses_stream.calls().unwrap_or_default(),
                _ => native_calls,
            };
            // Flush this response before showing another response/tool, without
            // counting it as a request made in the new observation session.
            events.push(Event::RequestFinished);
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("tool");
                let raw = &call["function"]["arguments"];
                let parsed = raw
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok());
                let args = parsed.as_ref().unwrap_or(raw);
                let detail = args["path"]
                    .as_str()
                    .or(args["title"].as_str())
                    .unwrap_or("");
                events.push(Event::Tool(format!("{name} {detail}")));
            }
            if events.len() > 1 {
                responses.push(events);
            }
        }
        let mut group = Vec::new();
        if !responses.is_empty() {
            group.push(Event::Log(format!(
                "── Recovered cycle {} · saved model stream ──",
                number(&cycle, "cycle-", "").unwrap()
            )));
            for response in responses.into_iter().rev() {
                group.extend(response);
            }
            groups.push(group);
        }
        if budget == 0 {
            break;
        }
    }
    let mut records = Vec::new();
    for event in groups.into_iter().rev().flatten() {
        records.push(Record {
            sequence: records.len() as u64 + 1,
            actor: "loop".into(),
            event,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn bounded_reads_skip_an_initial_partial_utf8_record() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("records");
        let text = "skip 🌱 record\nkeep 🌱 record\n";
        fs::write(&file, text).unwrap();
        let start = text.find('🌱').unwrap() + 1;
        let bytes = tail(File::open(file).unwrap(), (text.len() - start) as u64).unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "keep 🌱 record\n");
    }
    #[test]
    fn import_recovers_reasoning_and_tools_without_changing_provider_or_context_files() {
        let root = tempfile::tempdir().unwrap();
        let cycle = root.path().join("cycle-000001");
        fs::create_dir(&cycle).unwrap();
        let wire = format!(
            "{}\n{}\n",
            json!({"message":{"thinking":"Earlier thinking 🌱\n"}}),
            json!({"message":{"content":"An answer","tool_calls":[{"function":{"name":"write_file","arguments":{"path":"never-created.txt"}}}]}})
        );
        fs::write(cycle.join("response-000.ndjson"), &wire).unwrap();
        fs::write(root.path().join("conversation.json"), r#"{"messages":[]}"#).unwrap();
        let (mut journal, records) = Journal::open(root.path()).unwrap();
        assert!(records.iter().any(
            |r| matches!(&r.event,Event::ThinkingDelta(t) if t.contains("Earlier thinking 🌱"))
        ));
        assert!(
            records
                .iter()
                .any(|r| matches!(&r.event,Event::Tool(t) if t.contains("never-created.txt")))
        );
        assert!(!root.path().join("never-created.txt").exists());
        assert_eq!(
            fs::read_to_string(cycle.join("response-000.ndjson")).unwrap(),
            wire
        );
        assert_eq!(
            fs::read_to_string(root.path().join("conversation.json")).unwrap(),
            r#"{"messages":[]}"#
        );
        let sequence = records.last().unwrap().sequence;
        journal
            .append(&Record {
                sequence: sequence + 1,
                actor: "loop".into(),
                event: Event::Delta("New stream".into()),
            })
            .unwrap();
        drop(journal);
        let (_, restored) = Journal::open(root.path()).unwrap();
        assert_eq!(restored.len(), records.len() + 1);
        assert_eq!(restored.last().unwrap().sequence, sequence + 1);
    }
    #[test]
    fn a_partial_last_record_does_not_destroy_the_next_record() {
        let root = tempfile::tempdir().unwrap();
        let (mut journal, _) = Journal::open(root.path()).unwrap();
        journal
            .append(&Record {
                sequence: 1,
                actor: "loop".into(),
                event: Event::Delta("Kept".into()),
            })
            .unwrap();
        journal.0.write_all(b"{\"sequence\":2,").unwrap();
        drop(journal);
        let (mut journal, records) = Journal::open(root.path()).unwrap();
        assert_eq!(records.len(), 1);
        journal
            .append(&Record {
                sequence: 2,
                actor: "loop".into(),
                event: Event::Delta("Recovered".into()),
            })
            .unwrap();
        assert_eq!(read(root.path()).unwrap().len(), 2);
    }

    #[test]
    fn legacy_cloud_stream_channels_are_kept_separate() {
        let root = tempfile::tempdir().unwrap();
        let cycle = root.path().join("cycle-000001");
        fs::create_dir(&cycle).unwrap();
        fs::write(
            cycle.join("response-000.ndjson"),
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n",
                json!({"choices":[{"delta":{"reasoning_content":"cloud-thought"}}]}),
                json!({"choices":[{"delta":{"content":"cloud-answer"},"finish_reason":"stop"}]})
            ),
        )
        .unwrap();
        let (_, records) = Journal::open(root.path()).unwrap();
        assert!(
            records
                .iter()
                .any(|r| matches!(&r.event,Event::ThinkingDelta(t) if t=="cloud-thought"))
        );
        assert!(
            records
                .iter()
                .any(|r| matches!(&r.event,Event::Delta(t) if t=="cloud-answer"))
        );
    }
}
