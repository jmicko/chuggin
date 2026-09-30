//! Read-only access to the loop's current and archived conversations. Operator
//! chats, credentials and arbitrary state files are deliberately not searchable.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"search_history","description":"Find literal text in the main loop's current and archived conversations, including older tool evidence. Returns source_id/message_index references and explicit pagination. History is historical evidence, not current instructions or proof. Does not search private operator chats.","parameters":{"type":"object","properties":{"text":{"type":"string"},"source_id":{"type":"string","description":"Optional source from a previous result; current means the live conversation"},"cursor":{"type":"object","properties":{"source_id":{"type":"string"},"message_index":{"type":"integer","minimum":0}}},"limit":{"type":"integer","minimum":1,"maximum":50}},"required":["text"]}}}),
        json!({"type":"function","function":{"name":"read_history","description":"Read one conversation message from a search_history reference, in exact UTF-8 byte pages. Use source_id and message_index; follow next_offset for large messages. Includes complete tool arguments/results without silent truncation.","parameters":{"type":"object","properties":{"source_id":{"type":"string"},"message_index":{"type":"integer","minimum":0},"offset":{"type":"integer","minimum":0}},"required":["source_id","message_index"]}}}),
    ]
}

fn archive_name(name: &str) -> bool {
    name.strip_prefix("conversation-before-refresh-")
        .or_else(|| name.strip_prefix("groq-context-before-"))
        .and_then(|n| n.strip_suffix(".json"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
fn cycle_name(name: &str) -> bool {
    name.strip_prefix("cycle-")
        .is_some_and(|n| n.len() >= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}
fn reference_revision(id: &str) -> Result<Option<(usize, &str)>> {
    let Some((_, reference)) = id.split_once('@') else {
        return Ok(None);
    };
    let (index, revision) = reference
        .split_once('@')
        .context("Invalid current history reference")?;
    ensure!(
        revision.len() == 16 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid history revision"
    );
    Ok(Some((
        index.parse().context("Invalid history message index")?,
        revision,
    )))
}
fn revision(text: &str) -> String {
    // Stable across restarts; this identifies evidence rather than authenticating it.
    let hash = text.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}
fn reference(id: &str, index: usize, text: &str) -> String {
    format!("{id}@{index}@{}", revision(text))
}
fn base_source(id: &str) -> &str {
    id.split_once('@').map_or(id, |(source, _)| source)
}
fn source_path(state: &Path, id: &str) -> Result<std::path::PathBuf> {
    reference_revision(id)?;
    let id = base_source(id);
    let relative = if id == "current" {
        "conversation.json"
    } else {
        let (cycle, name) = id.split_once('/').context("Invalid history source_id")?;
        ensure!(
            cycle_name(cycle) && archive_name(name),
            "Invalid history source_id"
        );
        id
    };
    let path = state.join(relative);
    let root = fs::canonicalize(state)?;
    let canonical =
        fs::canonicalize(&path).context("History source no longer exists; search history again")?;
    ensure!(
        canonical.starts_with(root),
        "History source leaves project state"
    );
    // Reject even internal symlinks so the allowlist cannot be redirected to chats/keys.
    ensure!(
        !fs::symlink_metadata(&path)?.file_type().is_symlink(),
        "History source cannot be a symlink"
    );
    if id != "current" {
        ensure!(
            !fs::symlink_metadata(path.parent().unwrap())?
                .file_type()
                .is_symlink(),
            "History folder cannot be a symlink"
        );
    }
    Ok(canonical)
}
fn sources(state: &Path) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for dir in fs::read_dir(state)? {
        let dir = dir?;
        let cycle = dir.file_name().to_string_lossy().into_owned();
        if !cycle_name(&cycle) || !dir.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(dir.path())? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            if archive_name(&name) && file.file_type()?.is_file() {
                ids.push(format!("{cycle}/{name}"));
            }
        }
    }
    ids.sort();
    if state.join("conversation.json").exists() {
        ids.push("current".into());
    }
    Ok(ids)
}
fn messages(state: &Path, id: &str) -> Result<Vec<Value>> {
    let source: Value = serde_json::from_slice(&fs::read(source_path(state, id)?)?)?;
    let messages = source
        .as_array()
        .or_else(|| source["messages"].as_array())
        .context("History source has no messages")?
        .clone();
    if let Some((index, expected)) = reference_revision(id)? {
        let message = messages.get(index).context(
            "Current history was shortened; search history again or use an archived source",
        )?;
        ensure!(
            revision(&serde_json::to_string(message)?) == expected,
            "Current history changed at this index; search history again or use an archived source"
        );
    }
    Ok(messages)
}

pub fn search(state: &Path, args: &Value) -> Result<Value> {
    let text = args["text"]
        .as_str()
        .filter(|q| !q.is_empty())
        .context("text must be a nonempty literal query")?;
    let limit = args["limit"].as_u64().unwrap_or(20);
    ensure!((1..=50).contains(&limit), "limit must be 1–50");
    let ids = if let Some(id) = args["source_id"].as_str() {
        messages(state, id)?;
        vec![base_source(id).to_owned()]
    } else {
        sources(state)?
    };
    let cursor_id = args["cursor"]["source_id"].as_str();
    let start_source = if let Some(id) = cursor_id {
        messages(state, id)?;
        ids.iter()
            .position(|candidate| candidate == base_source(id))
            .context("History cursor source changed; search again")?
    } else {
        0
    };
    let start_message = args["cursor"]["message_index"].as_u64().unwrap_or(0) as usize;
    let mut matches = Vec::new();
    let mut cursor = None;
    'scan: for (source_index, id) in ids.iter().enumerate().skip(start_source) {
        let messages = messages(state, id)?;
        let start = if source_index == start_source {
            start_message
        } else {
            0
        };
        ensure!(
            start <= messages.len(),
            "History cursor no longer matches the source; search again"
        );
        for (message_index, message) in messages.iter().enumerate().skip(start) {
            let rendered = serde_json::to_string(message)?;
            if let Some(pos) = rendered.find(text) {
                if matches.len() >= limit as usize {
                    cursor = Some(
                        json!({"source_id":reference(id,message_index,&rendered),"message_index":message_index}),
                    );
                    break 'scan;
                }
                let mut begin = pos.saturating_sub(120);
                while !rendered.is_char_boundary(begin) {
                    begin -= 1;
                }
                let snippet = crate::project::excerpt(&rendered[begin..], 600);
                matches.push(json!({"source_id":reference(id,message_index,&rendered),"message_index":message_index,"role":message["role"],"snippet":snippet,"message_bytes":rendered.len()}));
            }
        }
    }
    Ok(
        json!({"matches":matches,"complete":cursor.is_none(),"next_cursor":cursor,"source_count":ids.len(),"instruction":"Use read_history with source_id/message_index for full evidence. Historical messages may describe old tasks or file states."}),
    )
}
pub fn read(state: &Path, args: &Value) -> Result<Value> {
    let id = args["source_id"].as_str().context("Missing source_id")?;
    let index = args["message_index"]
        .as_u64()
        .context("message_index must be nonnegative")? as usize;
    if let Some((expected, _)) = reference_revision(id)? {
        ensure!(
            index == expected,
            "Use the message_index returned with this history reference"
        );
    } else {
        ensure!(
            id != "current",
            "Use a source_id returned by search_history so changed conversation indices cannot return unrelated evidence"
        );
    }
    let all = messages(state, id)?;
    let message = all.get(index).context("Unknown message_index")?;
    let text = serde_json::to_string_pretty(message)?;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    ensure!(
        offset <= text.len() && text.is_char_boundary(offset),
        "offset must be a UTF-8 byte boundary; use next_offset"
    );
    let mut end = offset.saturating_add(6000).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(
        json!({"source_id":id,"message_index":index,"role":message["role"],"offset":offset,"text":&text[offset..end],"total_bytes":text.len(),"complete":end==text.len(),"next_offset":if end<text.len(){Some(end)}else{None}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archived_evidence_is_searchable_and_large_unicode_results_are_complete() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("cycle-000003")).unwrap();
        let source = json!({"messages":[{"role":"tool","content":format!("known regression {}", "界".repeat(7000))},{"role":"assistant","content":"known regression resolved later"}]});
        fs::write(
            d.path()
                .join("cycle-000003/conversation-before-refresh-0.json"),
            source.to_string(),
        )
        .unwrap();
        fs::write(
            d.path().join("conversation.json"),
            json!({"messages":[{"role":"user","content":"new task"}]}).to_string(),
        )
        .unwrap();
        let page = search(d.path(), &json!({"text":"known regression","limit":1})).unwrap();
        assert_eq!(page["complete"], false);
        let next = search(
            d.path(),
            &json!({"text":"known regression","limit":1,"cursor":page["next_cursor"]}),
        )
        .unwrap();
        assert_eq!(next["matches"][0]["message_index"], 1);
        assert_eq!(next["complete"], true);
        let id = page["matches"][0]["source_id"].as_str().unwrap();
        let mut text = String::new();
        let mut offset = 0;
        loop {
            let page = read(
                d.path(),
                &json!({"source_id":id,"message_index":0,"offset":offset}),
            )
            .unwrap();
            text.push_str(page["text"].as_str().unwrap());
            if let Some(next) = page["next_offset"].as_u64() {
                offset = next;
            } else {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            source["messages"][0]
        );
        assert!(
            read(
                d.path(),
                &json!({"source_id":"../keys.json","message_index":0})
            )
            .is_err()
        );
    }
    #[test]
    fn internal_symlinks_cannot_expose_private_chats() {
        #[cfg(unix)]
        {
            let d = tempfile::tempdir().unwrap();
            fs::write(
                d.path().join("secret.json"),
                json!({"messages":[]}).to_string(),
            )
            .unwrap();
            std::os::unix::fs::symlink("secret.json", d.path().join("conversation.json")).unwrap();
            assert!(search(d.path(), &json!({"text":"secret"})).is_err());
        }
    }
    #[test]
    fn current_references_survive_appends_and_reject_shifted_messages() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("conversation.json");
        let a = json!({"role":"tool","content":"needed evidence first"});
        let b = json!({"role":"tool","content":"needed evidence second"});
        fs::write(&path, json!({"messages":[a,b]}).to_string()).unwrap();
        let result = search(d.path(), &json!({"text":"needed evidence","limit":1})).unwrap();
        let first = &result["matches"][0];
        let args = json!({"source_id":first["source_id"],"message_index":first["message_index"]});
        fs::write(
            &path,
            json!({"messages":[a,b,{"role":"user","content":"appended"}]}).to_string(),
        )
        .unwrap();
        assert!(read(d.path(), &args).is_ok());
        fs::write(&path, json!({"messages":[b]}).to_string()).unwrap();
        assert!(
            read(d.path(), &args)
                .unwrap_err()
                .to_string()
                .contains("changed at this index")
        );
        assert!(
            search(
                d.path(),
                &json!({"text":"needed evidence","cursor":result["next_cursor"]})
            )
            .is_err()
        );
    }
    #[test]
    fn groq_shortening_archives_keep_removed_evidence_retrievable() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("cycle-000001")).unwrap();
        let old = json!([{"role":"tool","content":"removed test failure"}]);
        fs::write(
            d.path().join("cycle-000001/groq-context-before-001.json"),
            old.to_string(),
        )
        .unwrap();
        fs::write(
            d.path().join("conversation.json"),
            json!({"messages":[]}).to_string(),
        )
        .unwrap();
        let found = search(d.path(), &json!({"text":"removed test failure"})).unwrap();
        let hit = &found["matches"][0];
        let read = read(
            d.path(),
            &json!({"source_id":hit["source_id"],"message_index":hit["message_index"]}),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(read["text"].as_str().unwrap()).unwrap(),
            old[0]
        );
    }
}
