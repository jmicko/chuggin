//! Literal, scoped project search with explicit completeness and stable pages.
use crate::project;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::Path,
};

const MAX_FILE_BYTES: u64 = 8_000_000;
const PAGE_BYTES: usize = 10_000;

pub fn schema() -> Value {
    json!({"type":"function","function":{"name":"search","description":"Search literal text in project files, with optional relative file/directory path and file-extension scope. Returns matching line numbers, nearby context, skipped files and explicit completeness. Follow next_cursor with the same text/options for more matches. A cursor is invalid after scoped files change; start again to search their current state. Match excerpts may be shortened; read_file provides full source. Git/Chuggin state, dependency and build directories are excluded.","parameters":{"type":"object","properties":{"text":{"type":"string"},"path":{"type":"string","description":"Optional relative file or directory; omit to search the project."},"extensions":{"type":"array","items":{"type":"string"},"description":"Optional file extensions, such as [\"rs\",\"md\"]."},"case_sensitive":{"type":"boolean","default":true},"context_lines":{"type":"integer","minimum":0,"maximum":5,"default":1},"limit":{"type":"integer","minimum":1,"maximum":50,"default":20},"cursor":{"type":"string"}},"required":["text"]}}})
}

fn excerpt(text: &str) -> Value {
    let shown = project::excerpt(text, 400);
    json!({"text":shown,"text_truncated":text.len() > 400})
}

pub fn search(root: &Path, args: &Value) -> Result<String> {
    let query = args["text"]
        .as_str()
        .or_else(|| args["query"].as_str())
        .context("Missing search text")?;
    anyhow::ensure!(!query.is_empty(), "Empty search query");
    let scope = args["path"].as_str().unwrap_or("");
    let scope = if scope == "." {
        ""
    } else {
        scope.trim_start_matches("./").trim_end_matches('/')
    };
    if !scope.is_empty() {
        project::safe_path(root, scope)?;
    }
    let context = args
        .get("context_lines")
        .map(|value| value.as_u64().context("context_lines must be nonnegative"))
        .transpose()?
        .unwrap_or(1);
    anyhow::ensure!(context <= 5, "context_lines must be between 0 and 5");
    let context = context as usize;
    let limit = args
        .get("limit")
        .map(|value| value.as_u64().context("limit must be a positive integer"))
        .transpose()?
        .unwrap_or(20);
    anyhow::ensure!((1..=50).contains(&limit), "limit must be between 1 and 50");
    let mut extensions: Vec<String> = args
        .get("extensions")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .context("extensions must be an array of strings")?
        .unwrap_or_default();
    for extension in &mut extensions {
        *extension = extension.trim_start_matches('.').to_lowercase();
        anyhow::ensure!(
            !extension.is_empty() && !extension.contains(['/', '\\']),
            "Supply file extensions without paths"
        );
    }
    extensions.sort();
    extensions.dedup();
    let sensitive = args["case_sensitive"].as_bool().unwrap_or(true);
    let needle = if sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let scope_path = Path::new(scope);
    let files: Vec<_> = project::inventory(root)?
        .into_iter()
        .filter(|file| scope.is_empty() || Path::new(file).starts_with(scope_path))
        .filter(|file| {
            extensions.is_empty()
                || Path::new(file)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        extensions
                            .iter()
                            .any(|allowed| allowed.eq_ignore_ascii_case(ext))
                    })
        })
        .collect();
    let mut fingerprint = DefaultHasher::new();
    (query, scope, &extensions, sensitive, context).hash(&mut fingerprint);
    for file in &files {
        file.hash(&mut fingerprint);
        if let Ok(metadata) = fs::metadata(project::safe_path(root, file)?) {
            metadata.len().hash(&mut fingerprint);
            metadata.modified().ok().hash(&mut fingerprint);
        }
    }
    let fingerprint = format!("{:016x}", fingerprint.finish());
    let first = if let Some(cursor) = args["cursor"].as_str() {
        let (signature, offset) = cursor
            .split_once(':')
            .context("Invalid search cursor; start a new search")?;
        anyhow::ensure!(
            signature == fingerprint,
            "Search query/options or scoped files changed. Omit cursor and search their current state again."
        );
        offset
            .parse::<usize>()
            .context("Invalid search cursor offset")?
    } else {
        0
    };
    let mut matches = Vec::new();
    let mut match_count = 0usize;
    let mut returned_bytes = 0usize;
    let mut page_full = false;
    let mut skipped = Vec::new();
    for file in &files {
        let path = project::safe_path(root, file)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                skipped.push(json!({"path":file,"reason":error.to_string()}));
                continue;
            }
        };
        if metadata.len() > MAX_FILE_BYTES {
            skipped.push(json!({"path":file,"reason":"File exceeds 8 MB; narrow the query or read a selected range with read_file"}));
            continue;
        }
        let contents = match project::read(root, file) {
            Ok(contents) if !contents.contains('\0') => contents,
            Ok(_) => {
                skipped.push(json!({"path":file,"reason":"Binary file"}));
                continue;
            }
            Err(error) => {
                skipped.push(json!({"path":file,"reason":error.to_string()}));
                continue;
            }
        };
        let lines: Vec<_> = contents.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let found = if sensitive {
                line.contains(&needle)
            } else {
                line.to_lowercase().contains(&needle)
            };
            if !found {
                continue;
            }
            let ordinal = match_count;
            match_count += 1;
            if ordinal < first || page_full {
                continue;
            }
            let mut item = excerpt(line);
            item["path"] = json!(file);
            item["line"] = json!(index + 1);
            item["context"] = json!(
                (index.saturating_sub(context)..(index + context + 1).min(lines.len()))
                    .filter(|near| *near != index)
                    .map(|near| {
                        let mut context = excerpt(lines[near]);
                        context["line"] = json!(near + 1);
                        context
                    })
                    .collect::<Vec<_>>()
            );
            let bytes = serde_json::to_vec(&item)?.len();
            if matches.len() >= limit as usize
                || (!matches.is_empty() && returned_bytes + bytes > PAGE_BYTES)
            {
                page_full = true;
                continue;
            }
            returned_bytes += bytes;
            matches.push(item);
        }
    }
    anyhow::ensure!(
        first <= match_count,
        "Search cursor exceeds current matches; start a new search"
    );
    let next = first + matches.len();
    let more = next < match_count;
    let skipped_count = skipped.len();
    skipped.truncate(20);
    Ok(json!({"query":query,"path":scope,"matches":matches,"total_matches":match_count,"returned_matches":next-first,"scanned_files":files.len()-skipped_count,"skipped_files":skipped,"skipped_file_count":skipped_count,"scan_complete":skipped_count==0,"complete":!more && skipped_count==0,"truncated":more,"next_cursor":if more {Some(format!("{fingerprint}:{next}"))} else {None},"instruction":"Use next_cursor with the same query and scope for remaining matches. Excerpts are not full files; read_file with the returned line numbers for details."}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn result(root: &Path, args: Value) -> Value {
        serde_json::from_str(&search(root, &args).unwrap()).unwrap()
    }
    #[test]
    fn pages_keep_all_unicode_matches_and_reject_stale_cursors() {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(
            fixture.path().join("notes.md"),
            "α needle\nβ needle\nγ needle\n",
        )
        .unwrap();
        let first = result(
            fixture.path(),
            json!({"query":"needle","limit":1,"context_lines":0}),
        );
        assert_eq!(first["total_matches"], 3);
        assert_eq!(first["complete"], false);
        assert_eq!(first["matches"][0]["text"], "α needle");
        let second = result(
            fixture.path(),
            json!({"query":"needle","limit":2,"context_lines":0,"cursor":first["next_cursor"]}),
        );
        assert_eq!(second["returned_matches"], 2);
        assert_eq!(second["matches"][0]["line"], 2);
        assert_eq!(second["complete"], true);
        fs::write(fixture.path().join("notes.md"), "replacement needle\n").unwrap();
        assert!(
            search(
                fixture.path(),
                &json!({"query":"needle","context_lines":0,"cursor":first["next_cursor"]})
            )
            .unwrap_err()
            .to_string()
            .contains("files changed")
        );
    }
    #[test]
    fn directory_and_extension_scopes_exclude_state_and_report_binary_skips() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("src/nested")).unwrap();
        fs::create_dir(fixture.path().join(".chuggin")).unwrap();
        fs::write(
            fixture.path().join("src/nested/a.RS"),
            "before\nNeedle\nafter\n",
        )
        .unwrap();
        fs::write(fixture.path().join("src/nested/a.txt"), "Needle\n").unwrap();
        fs::write(fixture.path().join("outside.rs"), "Needle\n").unwrap();
        fs::write(fixture.path().join(".chuggin/secret.rs"), "Needle\n").unwrap();
        let found = result(
            fixture.path(),
            json!({"query":"needle","path":"src","extensions":[".rs"],"case_sensitive":false}),
        );
        assert_eq!(found["total_matches"], 1);
        assert_eq!(found["matches"][0]["path"], "src/nested/a.RS");
        assert_eq!(found["matches"][0]["context"][0]["text"], "before");
        assert!(search(fixture.path(), &json!({"query":"needle","path":".chuggin"})).is_err());
        fs::write(fixture.path().join("binary"), b"needle\0hidden").unwrap();
        let all = result(fixture.path(), json!({"query":"Needle"}));
        assert_eq!(all["complete"], false);
        assert_eq!(all["skipped_file_count"], 1);
    }
}
