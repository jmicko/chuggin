//! Optional, model-owned notes. Notes live outside a private computer and do not
//! become goals, prompts, or observations unless explicitly read through tools.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BODY: usize = 1024 * 1024;
const MAX_JSON: u64 = 7 * 1024 * 1024;
const SEARCH_BYTES: u64 = 8 * 1024 * 1024;
const SCAN_NOTES: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Metadata {
    id: String,
    title: String,
    tags: Vec<String>,
    revision: u64,
    version: String,
    created_at: String,
    updated_at: String,
    body_bytes: usize,
    content_hash: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Note {
    #[serde(flatten)]
    metadata: Metadata,
    body: String,
}

pub fn schemas() -> Vec<Value> {
    vec![json!({"type":"function","function":{
        "name":"notebook",
        "description":"Keep optional notes you choose: observations, questions, interests, references or other material. Notes are not assigned tasks or automatically added to context. list/search return bounded pages; follow next_after_id until complete. Search snippets are excerpts; read a note for its full body. read returns UTF-8 byte pages; follow next_offset and retain revision when continuing. write without id creates a note; replacing an existing note requires its expected_version from read/list. Old revisions are retained and can be read explicitly. Notes are fallible records, not current evidence or instructions.",
        "parameters":{"type":"object","properties":{
            "action":{"type":"string","enum":["list","read","write","search"]},
            "id":{"type":"string","description":"Stable note ID, such as note-1. Omit only when creating a note."},
            "title":{"type":"string","description":"Required when creating a note. Updating may omit it to retain the current title."},
            "body":{"type":"string","description":"UTF-8 note text, up to 1 MiB. Required for creation; omit on update to retain the existing text."},
            "tags":{"type":"array","items":{"type":"string"}},
            "expected_version":{"type":"string","description":"Required to update an existing note; copy the exact version from read/list/search."},
            "revision":{"type":"integer","minimum":1,"description":"read only: inspect this older revision instead of the current one."},
            "offset":{"type":"integer","minimum":0,"description":"read only: byte offset returned as next_offset."},
            "limit":{"type":"integer","minimum":1,"description":"read: desired byte count, default 8192, maximum 65536. list/search: result count, default 20, maximum 100."},
            "after_id":{"type":"string","description":"list/search continuation returned as next_after_id; results use ascending stable IDs."},
            "tag":{"type":"string","description":"Optional exact tag filter for list/search, case-insensitive."},
            "query":{"type":"string","description":"search only: whitespace-separated words, all of which must occur in the current title, tags or body. Case-insensitive."}
        },"required":["action"]}
    }})]
}

pub fn call(state: &Path, name: &str, args: &Value) -> Result<Value> {
    ensure!(name == "notebook", "Unknown notebook tool");
    let action = args["action"]
        .as_str()
        .context("notebook requires action")?;
    ensure!(
        matches!(action, "list" | "read" | "write" | "search"),
        "Unknown notebook action"
    );
    let root = state.join("notebook");
    fs::create_dir_all(root.join("entries"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("lock"))?;
    lock.lock()?;
    match action {
        "list" => enumerate(&root, args, false),
        "search" => enumerate(&root, args, true),
        "read" => read(&root, args),
        "write" => write(&root, args),
        _ => unreachable!(),
    }
}

fn id_number(id: &str) -> Result<u64> {
    let number = id
        .strip_prefix("note-")
        .context("Use a note ID such as note-1, not a file path")?;
    ensure!(
        !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()),
        "Invalid note ID"
    );
    let n = number.parse::<u64>().context("Invalid note ID")?;
    ensure!(n > 0 && id == format!("note-{n}"), "Invalid note ID");
    Ok(n)
}
fn entry(root: &Path, id: &str) -> Result<PathBuf> {
    id_number(id)?;
    Ok(root.join("entries").join(id))
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| {
        format!(
            "Could not read saved notebook data: {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;
    ensure!(
        file.metadata()?.len() <= MAX_JSON,
        "Saved notebook data is unexpectedly large"
    );
    let mut bytes = Vec::new();
    file.take(MAX_JSON + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_JSON,
        "Saved notebook data is unexpectedly large"
    );
    serde_json::from_slice(&bytes)
        .context("Saved notebook data is invalid; existing records were retained")
}
fn atomic_save(path: &Path, value: &impl Serialize, immutable: bool) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    if immutable {
        temporary.persist_noclobber(path).map_err(|e| e.error)?;
    } else {
        temporary.persist(path).map_err(|e| e.error)?;
    }
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn metadata(root: &Path, id: &str) -> Result<Metadata> {
    let saved: Metadata = load(&entry(root, id)?.join("head.json"))?;
    ensure!(
        saved.id == id && saved.revision > 0,
        "Saved note identity is invalid"
    );
    Ok(saved)
}
fn document(root: &Path, meta: &Metadata, revision: u64) -> Result<Note> {
    ensure!(
        revision > 0 && revision <= meta.revision,
        "Requested note revision is not published"
    );
    let note: Note = load(
        &entry(root, &meta.id)?
            .join("revisions")
            .join(format!("{revision:016}.json")),
    )?;
    ensure!(
        note.metadata.id == meta.id && note.metadata.revision == revision,
        "Saved note revision is invalid"
    );
    ensure!(
        note.body.len() == note.metadata.body_bytes
            && hash(&note.metadata.title, &note.body, &note.metadata.tags)
                == note.metadata.content_hash,
        "Saved note content does not match its version; existing data was retained"
    );
    if revision == meta.revision {
        ensure!(
            note.metadata.version == meta.version,
            "Saved note version is inconsistent"
        );
    }
    Ok(note)
}
fn summary(meta: &Metadata) -> Value {
    json!({"id":meta.id,"title":meta.title,"tags":meta.tags,"revision":meta.revision,"version":meta.version,"created_at":meta.created_at,"updated_at":meta.updated_at,"body_bytes":meta.body_bytes})
}
fn number(args: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    let Some(value) = args.get(key) else {
        return Ok(default);
    };
    let n = value
        .as_u64()
        .with_context(|| format!("{key} must be a nonnegative integer"))?;
    ensure!(n <= max, "{key} exceeds {max}");
    Ok(n)
}
fn list_ids(root: &Path) -> Result<Vec<(u64, String)>> {
    let mut ids = Vec::new();
    for dir in fs::read_dir(root.join("entries"))? {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        let id = dir.file_name().to_string_lossy().into_owned();
        if let Ok(number) = id_number(&id) {
            ids.push((number, id));
        }
    }
    ids.sort_by_key(|(n, _)| *n);
    Ok(ids)
}
fn read(root: &Path, args: &Value) -> Result<Value> {
    let id = args["id"].as_str().context("read requires id")?;
    let meta = metadata(root, id)?;
    let revision = number(args, "revision", meta.revision, u64::MAX)?;
    let note = document(root, &meta, revision)?;
    let offset = usize::try_from(number(args, "offset", 0, note.body.len() as u64)?)?;
    ensure!(
        note.body.is_char_boundary(offset),
        "offset is inside a UTF-8 character; use next_offset from the previous page"
    );
    let limit = number(args, "limit", 8192, 65536)? as usize;
    ensure!(limit > 0, "limit must be positive");
    let mut end = offset.saturating_add(limit).min(note.body.len());
    while !note.body.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < note.body.len() {
        end = offset + note.body[offset..].chars().next().unwrap().len_utf8();
    }
    let mut out = summary(&note.metadata);
    out["body"] = json!(&note.body[offset..end]);
    out["offset"] = json!(offset);
    out["bytes_returned"] = json!(end - offset);
    out["next_offset"] = if end < note.body.len() {
        json!(end)
    } else {
        Value::Null
    };
    out["complete"] = json!(end == note.body.len());
    out["historical_revision"] = json!(revision != meta.revision);
    Ok(out)
}
fn title(value: &Value) -> Result<String> {
    let title = value.as_str().context("title must be a string")?.trim();
    ensure!(
        !title.is_empty() && title.len() <= 512 && !title.chars().any(char::is_control),
        "title must contain 1–512 bytes without control characters"
    );
    Ok(title.into())
}
fn tags(value: &Value) -> Result<Vec<String>> {
    let values = value
        .as_array()
        .context("tags must be an array of strings")?;
    ensure!(values.len() <= 32, "Supply at most 32 tags");
    let mut tags = Vec::new();
    for value in values {
        let tag = value.as_str().context("Each tag must be a string")?.trim();
        ensure!(
            !tag.is_empty() && tag.len() <= 100 && !tag.chars().any(char::is_control),
            "Tags must contain 1–100 bytes without control characters"
        );
        tags.push(tag.into());
    }
    tags.sort();
    tags.dedup();
    Ok(tags)
}
fn hash(title: &str, body: &str, tags: &[String]) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!({"title":title,"body":body,"tags":tags})).unwrap()
        )
    )
}
fn allocate(root: &Path) -> Result<String> {
    let path = root.join("counter.json");
    let saved = if path.exists() {
        load::<Value>(&path)?["last_id"]
            .as_u64()
            .context("Notebook counter is invalid")?
    } else {
        0
    };
    let max = list_ids(root)?
        .last()
        .map(|(n, _)| *n)
        .unwrap_or(0)
        .max(saved);
    let next = max
        .checked_add(1)
        .context("Notebook ID space is exhausted")?;
    atomic_save(&path, &json!({"last_id":next}), false)?;
    Ok(format!("note-{next}"))
}
fn next_revision(path: &Path) -> Result<u64> {
    let mut last = 0;
    for file in fs::read_dir(path)? {
        let name = file?.file_name().to_string_lossy().into_owned();
        if let Some(n) = name
            .strip_suffix(".json")
            .and_then(|s| s.parse::<u64>().ok())
        {
            last = last.max(n);
        }
    }
    last.checked_add(1)
        .context("Note revision space is exhausted")
}
fn write(root: &Path, args: &Value) -> Result<Value> {
    let previous = if let Some(id) = args.get("id") {
        let id = id.as_str().context("id must be a note ID string")?;
        let meta = metadata(root, id)?;
        Some(document(root, &meta, meta.revision)?)
    } else {
        None
    };
    let expected = if previous.is_some() {
        Some(
            args["expected_version"]
                .as_str()
                .context("Read the note first and supply expected_version; no note was changed")?,
        )
    } else {
        None
    };
    ensure!(
        previous.is_some() || args.get("expected_version").is_none(),
        "expected_version requires an existing note id"
    );
    let title = match args.get("title") {
        Some(v) => title(v)?,
        None => previous
            .as_ref()
            .map(|n| n.metadata.title.clone())
            .context("Creating a note requires title")?,
    };
    let body = match args.get("body") {
        Some(v) => v.as_str().context("body must be a string")?.to_owned(),
        None => previous
            .as_ref()
            .map(|n| n.body.clone())
            .context("Creating a note requires body")?,
    };
    ensure!(
        body.len() <= MAX_BODY,
        "A note body must not exceed 1 MiB; split larger material into notes"
    );
    let tags = match args.get("tags") {
        Some(v) => tags(v)?,
        None => previous
            .as_ref()
            .map(|n| n.metadata.tags.clone())
            .unwrap_or_default(),
    };
    let content_hash = hash(&title, &body, &tags);
    if let Some(note) = &previous {
        if note.metadata.content_hash == content_hash {
            return Ok(
                json!({"status":"already_saved","changed":false,"note":summary(&note.metadata)}),
            );
        }
        ensure!(
            expected == Some(note.metadata.version.as_str()),
            "Note {} changed since you read it. Read it again and use the new version; no note was changed",
            note.metadata.id
        );
    } else {
        // An identical create retry returns the existing ID, not another copy.
        for (_, id) in list_ids(root)? {
            if !entry(root, &id)?.join("head.json").exists() {
                continue;
            }
            let meta = metadata(root, &id)?;
            if meta.content_hash == content_hash {
                return Ok(json!({"status":"already_saved","changed":false,"note":summary(&meta)}));
            }
        }
    }
    let id = match &previous {
        Some(note) => note.metadata.id.clone(),
        None => allocate(root)?,
    };
    let revisions = entry(root, &id)?.join("revisions");
    fs::create_dir_all(&revisions)?;
    let revision = next_revision(&revisions)?;
    let now = chrono::Utc::now().to_rfc3339();
    let version = format!("{id}:r{revision}:{}", &content_hash[..16]);
    let meta = Metadata {
        id: id.clone(),
        title,
        tags,
        revision,
        version,
        created_at: previous
            .as_ref()
            .map(|n| n.metadata.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
        body_bytes: body.len(),
        content_hash,
    };
    let note = Note {
        metadata: meta.clone(),
        body,
    };
    // Publish immutable data first, then atomically advance the head. A crash
    // cannot replace an old revision or expose a partially-written current note.
    atomic_save(&revisions.join(format!("{revision:016}.json")), &note, true)?;
    atomic_save(&entry(root, &id)?.join("head.json"), &meta, false)?;
    Ok(json!({"status":"saved","changed":true,"created":previous.is_none(),"note":summary(&meta)}))
}

fn snippet(body: &str, terms: &[String]) -> (usize, String) {
    let folded = body.to_lowercase();
    let matched = terms
        .iter()
        .filter_map(|term| folded.find(term))
        .min()
        .unwrap_or(0);
    let mut folded_bytes = 0;
    let at = body
        .char_indices()
        .find_map(|(index, ch)| {
            let size = ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            if folded_bytes + size > matched {
                Some(index)
            } else {
                folded_bytes += size;
                None
            }
        })
        .unwrap_or(0);
    let mut start = at.saturating_sub(120);
    while !body.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start.saturating_add(480).min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (start, body[start..end].into())
}
fn enumerate(root: &Path, args: &Value, search: bool) -> Result<Value> {
    let after = args
        .get("after_id")
        .map(|v| {
            v.as_str()
                .context("after_id must be a note ID string")
                .and_then(id_number)
        })
        .transpose()?
        .unwrap_or(0);
    let limit = number(args, "limit", 20, 100)? as usize;
    ensure!(limit > 0, "limit must be positive");
    let tag = args
        .get("tag")
        .map(|v| {
            v.as_str()
                .context("tag must be a string")
                .map(str::to_lowercase)
        })
        .transpose()?;
    let terms = if search {
        let query = args["query"].as_str().context("search requires query")?;
        ensure!(
            !query.trim().is_empty() && query.len() <= 1024,
            "query must contain 1–1024 bytes"
        );
        query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let ids: Vec<_> = list_ids(root)?
        .into_iter()
        .filter(|(n, _)| *n > after)
        .collect();
    let mut notes = Vec::new();
    let mut scanned = 0;
    let mut bytes = 0;
    let mut cursor = None;
    for (_, id) in &ids {
        if scanned >= SCAN_NOTES || notes.len() >= limit {
            break;
        }
        let path = entry(root, id)?;
        if !path.join("head.json").exists() {
            scanned += 1;
            cursor = Some(id.clone());
            continue;
        }
        let meta = metadata(root, id)?;
        if search {
            let size = fs::metadata(
                path.join("revisions")
                    .join(format!("{:016}.json", meta.revision)),
            )?
            .len();
            ensure!(
                size <= MAX_JSON,
                "Saved notebook data is unexpectedly large"
            );
            if bytes + size > SEARCH_BYTES {
                break;
            }
            bytes += size;
        }
        scanned += 1;
        cursor = Some(id.clone());
        if tag
            .as_ref()
            .is_some_and(|tag| !meta.tags.iter().any(|t| t.to_lowercase() == *tag))
        {
            continue;
        }
        let mut item = summary(&meta);
        if search {
            let note = document(root, &meta, meta.revision)?;
            let haystack =
                format!("{}\n{}\n{}", meta.title, meta.tags.join("\n"), note.body).to_lowercase();
            if !terms.iter().all(|term| haystack.contains(term)) {
                continue;
            }
            let (offset, text) = snippet(&note.body, &terms);
            let end = offset + text.len();
            item["snippet_bytes"] = json!(text.len());
            item["snippet_complete"] = json!(offset == 0 && end == note.body.len());
            item["snippet_next_offset"] = if end < note.body.len() {
                json!(end)
            } else {
                Value::Null
            };
            item["snippet"] = json!(text);
            item["snippet_offset"] = json!(offset);
        }
        notes.push(item);
    }
    let complete = scanned == ids.len();
    Ok(
        json!({"notes":notes,"scanned":scanned,"scanned_bytes":bytes,"complete":complete,"next_after_id":if complete {None}else{cursor},"changed":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invoke(root: &Path, args: Value) -> Value {
        call(root, "notebook", &args).unwrap()
    }
    #[test]
    fn notes_and_old_revisions_survive_restart_without_touching_project_files() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("private-state");
        let created = invoke(
            &state,
            json!({"action":"write","title":"A question","body":"Why does this happen?","tags":["ideas"]}),
        );
        let meta = created["note"].clone();
        let updated = invoke(
            &state,
            json!({"action":"write","id":meta["id"],"expected_version":meta["version"],"body":"A tentative explanation"}),
        );
        assert_eq!(updated["note"]["revision"], 2);
        assert_eq!(
            invoke(&state, json!({"action":"read","id":meta["id"]}))["body"],
            "A tentative explanation"
        );
        let old = invoke(
            &state,
            json!({"action":"read","id":meta["id"],"revision":1}),
        );
        assert_eq!(old["body"], "Why does this happen?");
        assert_eq!(old["historical_revision"], true);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert!(!root.path().join("A question").exists());
        assert!(
            call(
                &state,
                "notebook",
                &json!({"action":"read","id":"../../outside"})
            )
            .is_err()
        );
    }
    #[test]
    fn stale_and_concurrent_updates_do_not_overwrite_each_other() {
        let root = tempfile::tempdir().unwrap();
        let meta = invoke(
            root.path(),
            json!({"action":"write","title":"Observation","body":"original"}),
        )["note"]
            .clone();
        let mut jobs = Vec::new();
        for body in ["first revision", "second revision"] {
            let root = root.path().to_owned();
            let meta = meta.clone();
            jobs.push(std::thread::spawn(move||call(&root,"notebook",&json!({"action":"write","id":meta["id"],"expected_version":meta["version"],"body":body})).is_ok()));
        }
        assert_eq!(
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .filter(|ok| *ok)
                .count(),
            1
        );
        assert_eq!(
            invoke(
                root.path(),
                json!({"action":"read","id":meta["id"],"revision":1})
            )["body"],
            "original"
        );
        assert!(
            call(
                root.path(),
                "notebook",
                &json!({"action":"write","id":meta["id"],"body":"unversioned"})
            )
            .is_err()
        );
    }
    #[test]
    fn identical_creates_are_idempotent_and_updates_do_not_create_extra_revisions() {
        let root = tempfile::tempdir().unwrap();
        let args = json!({"action":"write","title":"Remember","body":"Saved once"});
        let first = invoke(root.path(), args.clone());
        let second = invoke(root.path(), args);
        assert_eq!(first["note"]["id"], second["note"]["id"]);
        assert_eq!(second["changed"], false);
        let same = invoke(
            root.path(),
            json!({"action":"write","id":first["note"]["id"],"expected_version":first["note"]["version"],"body":"Saved once"}),
        );
        assert_eq!(same["changed"], false);
        assert_eq!(same["note"]["revision"], 1);
    }
    #[test]
    fn unicode_reads_and_search_are_paginated_without_breaking_characters() {
        let root = tempfile::tempdir().unwrap();
        let meta=invoke(root.path(),json!({"action":"write","title":"Café 🌱","body":"İİİİİ 🌱 bright COLOURS and café","tags":["Art"]}))["note"].clone();
        let mut offset = 0;
        let mut text = String::new();
        loop {
            let page = invoke(
                root.path(),
                json!({"action":"read","id":meta["id"],"offset":offset,"limit":1}),
            );
            text.push_str(page["body"].as_str().unwrap());
            if let Some(next) = page["next_offset"].as_u64() {
                assert!(next > offset);
                offset = next;
            } else {
                break;
            }
        }
        assert_eq!(text, "İİİİİ 🌱 bright COLOURS and café");
        let found = invoke(
            root.path(),
            json!({"action":"search","query":"COLOURS CAFÉ","tag":"art"}),
        );
        assert_eq!(found["notes"].as_array().unwrap().len(), 1);
        assert!(
            found["notes"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("COLOURS")
        );
    }
    #[test]
    fn list_and_search_cursors_follow_stable_ids_and_report_remaining_work() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..3 {
            invoke(
                root.path(),
                json!({"action":"write","title":format!("Topic {n}"),"body":"Shared keyword"}),
            );
        }
        for action in ["list", "search"] {
            let mut cursor = Value::Null;
            let mut ids = Vec::new();
            loop {
                let mut args = json!({"action":action,"limit":1,"query":"keyword"});
                if !cursor.is_null() {
                    args["after_id"] = cursor;
                }
                let page = invoke(root.path(), args);
                ids.extend(
                    page["notes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|n| n["id"].as_str().unwrap().to_owned()),
                );
                if page["complete"] == true {
                    break;
                }
                cursor = page["next_after_id"].clone();
                assert!(!cursor.is_null());
            }
            assert_eq!(ids, vec!["note-1", "note-2", "note-3"]);
        }
    }
    #[test]
    fn interrupted_publish_keeps_the_current_note_and_does_not_reuse_revisions() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();
        let saved = invoke(
            state,
            json!({"action":"write","title":"Original","body":"Published"}),
        );
        let note_root = state.join("notebook/entries/note-1");
        let first = fs::read(note_root.join("revisions/0000000000000001.json")).unwrap();
        let orphan = note_root.join("revisions/0000000000000002.json");
        fs::write(&orphan, b"interrupted unpublished revision").unwrap();
        assert_eq!(
            invoke(state, json!({"action":"read","id":"note-1"}))["body"],
            "Published"
        );
        let updated = invoke(
            state,
            json!({"action":"write","id":"note-1","expected_version":saved["note"]["version"],"body":"Next published version"}),
        );
        assert_eq!(updated["note"]["revision"], 3);
        assert_eq!(
            fs::read(note_root.join("revisions/0000000000000001.json")).unwrap(),
            first
        );
        assert_eq!(
            fs::read(orphan).unwrap(),
            b"interrupted unpublished revision"
        );
        assert_eq!(
            invoke(state, json!({"action":"read","id":"note-1"}))["body"],
            "Next published version"
        );
    }
    #[test]
    fn empty_scan_pages_and_search_excerpts_report_continuations() {
        let root = tempfile::tempdir().unwrap();
        invoke(root.path(), json!({"action":"list"}));
        for id in 1..=201 {
            fs::create_dir(root.path().join(format!("notebook/entries/note-{id}"))).unwrap();
        }
        let first = invoke(root.path(), json!({"action":"list"}));
        assert_eq!(first["scanned"], 200);
        assert_eq!(first["complete"], false);
        assert_eq!(first["next_after_id"], "note-200");
        assert_eq!(
            invoke(root.path(), json!({"action":"list","after_id":"note-200"}))["complete"],
            true
        );
        invoke(
            root.path(),
            json!({"action":"write","title":"A long note","body":format!("{} needle {}","🌱".repeat(200),"é".repeat(400))}),
        );
        let page = invoke(
            root.path(),
            json!({"action":"search","query":"needle","after_id":"note-201"}),
        );
        let item = &page["notes"][0];
        assert!(item["snippet"].as_str().unwrap().contains("needle"));
        assert_eq!(item["snippet_complete"], false);
        assert!(item["snippet_offset"].as_u64().unwrap() > 0);
        assert!(
            item["snippet_next_offset"].as_u64().unwrap()
                > item["snippet_offset"].as_u64().unwrap()
        );
    }
}
