//! Project execution and bounded access to its evidence across cycles.
use crate::project::{self, Check};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::AtomicBool,
};

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"run_command","description":"Run an executable with argv in the private task workspace. Examples: [\"cargo\",\"test\",\"test_name\"], [\"cargo\",\"fmt\"]. No implicit shell; pipes/redirection are literal arguments. Commands finish or time out; background children are terminated. Returns exit_code, output tail and log_id. Honor task scope. Do not commit, reset Git, modify Chuggin state or operate outside this workspace. Configured final checks still run independently.","parameters":{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":600}},"required":["argv"]}}}),
        json!({"type":"function","function":{"name":"compiler_diagnostics","description":"Run cargo check --all-targets and group Rust errors/warnings with source locations, snippets and compiler suggestions. Use after compiler failure rather than guessing APIs. This does not run tests or replace run_checks. Full raw output is available through log_id.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"read_command_log","description":"Read an earlier command/diagnostics log, including prior cycles, in bounded byte chunks. Use the complete log_id and next_offset returned by tools; do not repeat the same offset.","parameters":{"type":"object","properties":{"log_id":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["log_id"]}}}),
    ]
}
pub fn run(root: &Path, art: &Path, id: &str, args: &Value, stop: &AtomicBool) -> Result<Value> {
    let argv: Vec<String> =
        serde_json::from_value(args["argv"].clone()).context("argv must be an array of strings")?;
    anyhow::ensure!(
        !argv.is_empty() && !argv[0].is_empty() && argv.len() <= 128,
        "Supply an executable and at most 127 arguments"
    );
    anyhow::ensure!(
        argv.iter().all(|s| !s.contains('\0'))
            && argv.iter().map(String::len).sum::<usize>() <= 16000,
        "Command arguments too large or contain NUL"
    );
    let timeout = match args.get("timeout_seconds") {
        Some(v) => v
            .as_u64()
            .context("timeout_seconds must be a positive integer")?,
        None => 120,
    };
    anyhow::ensure!(
        (1..=600).contains(&timeout),
        "timeout_seconds must be 1–600"
    );
    let filename = format!("command-{id}.log");
    let log = art.join(&filename);
    let result = project::check(
        root,
        &Check {
            argv,
            timeout_seconds: timeout,
        },
        &log,
        stop,
    )?;
    let bytes = fs::metadata(log)?.len();
    let log_id = qualified_log_id(art, &filename);
    Ok(
        json!({"exit_code":result.exit_code,"passed":result.passed,"timed_out":result.timed_out,"output_tail":output_tail(&result.output,7000),"log_id":log_id,"log_bytes":bytes,"instruction":"Use read_command_log for full output. A successful command does not replace configured final checks."}),
    )
}
fn output_tail(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
fn cycle_name(name: &str) -> bool {
    name.strip_prefix("cycle-")
        .is_some_and(|digits| digits.len() >= 6 && digits.bytes().all(|b| b.is_ascii_digit()))
}
fn qualified_log_id(art: &Path, filename: &str) -> String {
    match art.file_name().and_then(|name| name.to_str()) {
        Some(cycle) if cycle_name(cycle) => format!("{cycle}/{filename}"),
        _ => filename.to_owned(),
    }
}
fn log_filename(name: &str) -> bool {
    (name.starts_with("command-") || name.starts_with("diagnostics-"))
        && name.ends_with(".log")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}
pub fn read_log(art: &Path, args: &Value) -> Result<Value> {
    let id = args["log_id"].as_str().context("Missing log_id")?;
    anyhow::ensure!(
        !fs::symlink_metadata(art)?.file_type().is_symlink(),
        "Symlinks are not available to log tools"
    );
    let root = if let Some((cycle, filename)) = id.split_once('/') {
        anyhow::ensure!(
            cycle_name(cycle)
                && log_filename(filename)
                && art
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(cycle_name),
            "Use the complete command/diagnostics log_id returned by a tool"
        );
        let state = art.parent().context("Cycle directory has no parent")?;
        anyhow::ensure!(
            !fs::symlink_metadata(state)?.file_type().is_symlink(),
            "Symlinks are not available to log tools"
        );
        state
    } else {
        anyhow::ensure!(
            log_filename(id),
            "Use a command/diagnostics log_id returned by a tool"
        );
        art
    };
    let path = project::safe_path(root, id)?;
    anyhow::ensure!(fs::metadata(&path)?.is_file(), "Log is not a regular file");
    let mut file = fs::File::open(path)?;
    let total = file.metadata()?.len();
    let offset = match args.get("offset") {
        Some(v) => v.as_u64().context("offset must be nonnegative")?,
        None => 0,
    };
    anyhow::ensure!(offset <= total, "offset exceeds log size");
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(6000).read_to_end(&mut bytes)?;
    let next = offset + bytes.len() as u64;
    Ok(
        json!({"log_id":id,"offset":offset,"total_bytes":total,"text":String::from_utf8_lossy(&bytes),"next_offset":if next<total{Some(next)}else{None}}),
    )
}
pub fn diagnostics(root: &Path, art: &Path, id: &str, stop: &AtomicBool) -> Result<Value> {
    anyhow::ensure!(
        root.join("Cargo.toml").is_file(),
        "No Cargo.toml in task workspace"
    );
    let filename = format!("diagnostics-{id}.log");
    let log = art.join(&filename);
    let log_id = qualified_log_id(art, &filename);
    let result = project::check(
        root,
        &Check {
            argv: vec![
                "cargo".into(),
                "check".into(),
                "--all-targets".into(),
                "--message-format=json".into(),
            ],
            timeout_seconds: 120,
        },
        &log,
        stop,
    )?;
    let mut bytes = Vec::new();
    fs::File::open(&log)?
        .take(8_000_000)
        .read_to_end(&mut bytes)?;
    let mut summary = summarize(root, &String::from_utf8_lossy(&bytes));
    summary["exit_code"] = json!(result.exit_code);
    summary["passed"] = json!(result.passed);
    summary["timed_out"] = json!(result.timed_out);
    summary["log_id"] = json!(log_id);
    summary["log_scan_truncated"] = json!(fs::metadata(log)?.len() > bytes.len() as u64);
    if summary["diagnostics"]
        .as_array()
        .is_none_or(|d| d.is_empty())
    {
        summary["output_tail"] = json!(output_tail(&result.output, 3000));
    }
    Ok(summary)
}
fn summarize(root: &Path, raw: &str) -> Value {
    let mut grouped: BTreeMap<String, Value> = BTreeMap::new();
    let mut total = 0;
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let m = &v["message"];
        if !matches!(m["level"].as_str(), Some("error" | "warning")) {
            continue;
        }
        total += 1;
        let key = format!("{}:{}:{}", m["level"], m["code"]["code"], m["message"]);
        let entry=grouped.entry(key).or_insert_with(||json!({"level":m["level"],"code":m["code"]["code"],"message":project::excerpt(m["message"].as_str().unwrap_or(""),800),"occurrences":0,"locations":[],"suggestions":[]}));
        entry["occurrences"] = json!(entry["occurrences"].as_u64().unwrap_or(0) + 1);
        for span in m["spans"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["is_primary"] == true)
            .take(3)
        {
            let mut loc = json!({"file":span["file_name"],"line":span["line_start"],"column":span["column_start"],"label":span["label"]});
            if let Some(path) = span["file_name"].as_str() {
                let relative = Path::new(path)
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .unwrap_or(path);
                if let Ok(source) = project::read_lines(
                    root,
                    relative,
                    span["line_start"]
                        .as_u64()
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .max(1) as usize,
                    3,
                ) {
                    loc["source"] = json!(project::excerpt(&source, 600));
                }
            }
            let locations = entry["locations"].as_array_mut().unwrap();
            if locations.len() < 4 && !locations.contains(&loc) {
                locations.push(loc);
            }
        }
        for child in m["children"].as_array().into_iter().flatten().take(4) {
            let suggestion = json!({"message":project::excerpt(child["message"].as_str().unwrap_or(""),500),"edits":child["spans"].as_array().into_iter().flatten().filter(|s|s["suggested_replacement"].is_string()).take(3).map(|s|json!({"file":s["file_name"],"line":s["line_start"],"column":s["column_start"],"line_end":s["line_end"],"column_end":s["column_end"],"replacement":s["suggested_replacement"],"applicability":s["suggestion_applicability"]})).collect::<Vec<_>>()});
            let suggestions = entry["suggestions"].as_array_mut().unwrap();
            if suggestions.len() < 4 && !suggestions.contains(&suggestion) {
                suggestions.push(suggestion);
            }
        }
    }
    let unique = grouped.len();
    let mut rows: Vec<_> = grouped.into_values().collect();
    rows.sort_by_key(|d| d["level"] != "error");
    let mut budget = 0;
    rows.retain(|v| {
        budget += v.to_string().len();
        budget <= 8500
    });
    json!({"reported_messages":total,"unique_diagnostics":unique,"truncated":rows.len()<unique,"diagnostics":rows})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_preserves_arguments_exit_status_and_log_continuation() {
        let root = tempfile::tempdir().unwrap();
        let art = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let result = run(
            root.path(),
            art.path(),
            "0-0",
            &json!({"argv":["printf","%s","literal $(touch unexpected) ; | >"]}),
            &stop,
        )
        .unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["output_tail"], "literal $(touch unexpected) ; | >");
        assert!(!root.path().join("unexpected").exists());
        let fail = run(
            root.path(),
            art.path(),
            "0-1",
            &json!({"argv":["sh","-c","echo failure >&2; exit 7"]}),
            &stop,
        )
        .unwrap();
        assert_eq!(fail["exit_code"], 7);
        assert_eq!(fail["passed"], false);
        fs::write(art.path().join("command-large.log"), "a".repeat(15000)).unwrap();
        let first = read_log(art.path(), &json!({"log_id":"command-large.log"})).unwrap();
        assert_eq!(first["next_offset"], 6000);
        let next = read_log(
            art.path(),
            &json!({"log_id":"command-large.log","offset":12000}),
        )
        .unwrap();
        assert!(next["next_offset"].is_null());
        assert_eq!(next["text"].as_str().unwrap().len(), 3000);
        assert!(read_log(art.path(), &json!({"log_id":"command-../secret.log"})).is_err());
        assert!(
            run(
                root.path(),
                art.path(),
                "0-2",
                &json!({"argv":["true"],"timeout_seconds":0}),
                &stop
            )
            .is_err()
        );
    }
    #[test]
    fn qualified_logs_keep_their_cycle_when_command_ids_repeat() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first_cycle = state.path().join("cycle-000001");
        let second_cycle = state.path().join("cycle-000002");
        fs::create_dir(&first_cycle).unwrap();
        fs::create_dir(&second_cycle).unwrap();
        let stop = AtomicBool::new(false);
        let first = run(
            root.path(),
            &first_cycle,
            "0-0",
            &json!({"argv":["printf","%s","earlier failure evidence"]}),
            &stop,
        )
        .unwrap();
        let second = run(
            root.path(),
            &second_cycle,
            "0-0",
            &json!({"argv":["printf","%s","later successful result"]}),
            &stop,
        )
        .unwrap();
        assert_eq!(first["log_id"], "cycle-000001/command-0-0.log");
        assert_eq!(second["log_id"], "cycle-000002/command-0-0.log");
        assert_eq!(
            read_log(&second_cycle, &json!({"log_id":first["log_id"]})).unwrap()["text"],
            "earlier failure evidence"
        );
        assert_eq!(
            read_log(&second_cycle, &json!({"log_id":second["log_id"]})).unwrap()["text"],
            "later successful result"
        );
        // Existing unqualified IDs remain explicitly local to the current cycle.
        assert_eq!(
            read_log(&second_cycle, &json!({"log_id":"command-0-0.log"})).unwrap()["text"],
            "later successful result"
        );
        for id in [
            "../cycle-000001/command-0-0.log",
            "cycle-000001/../command-0-0.log",
            "cycle-000001//command-0-0.log",
            "cycle-000001/nested/command-0-0.log",
            "/cycle-000001/command-0-0.log",
            "other-project/command-0-0.log",
            "cycle-000001/state.json",
        ] {
            assert!(
                read_log(&second_cycle, &json!({"log_id":id})).is_err(),
                "accepted {id}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn log_reads_reject_symlinked_cycles_and_files() {
        use std::os::unix::fs::symlink;
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let current = state.path().join("cycle-000002");
        fs::create_dir(&current).unwrap();
        fs::write(outside.path().join("command-0-0.log"), "outside evidence").unwrap();
        symlink(outside.path(), state.path().join("cycle-000001")).unwrap();
        assert!(read_log(&current, &json!({"log_id":"cycle-000001/command-0-0.log"})).is_err());
        symlink(
            outside.path().join("command-0-0.log"),
            current.join("command-0-0.log"),
        )
        .unwrap();
        assert!(read_log(&current, &json!({"log_id":"command-0-0.log"})).is_err());
        assert!(read_log(&current, &json!({"log_id":"cycle-000002/command-0-0.log"})).is_err());
    }
    #[test]
    fn timeout_stops_descendants() {
        let root = tempfile::tempdir().unwrap();
        let art = tempfile::tempdir().unwrap();
        let result = run(
            root.path(),
            art.path(),
            "0-0",
            &json!({"argv":["sh","-c","(sleep 2; touch escaped) & wait"],"timeout_seconds":1}),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result["timed_out"], true);
        std::thread::sleep(std::time::Duration::from_millis(1300));
        assert!(!root.path().join("escaped").exists());
    }
    #[test]
    fn real_compiler_errors_include_locations_and_logs() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let first_cycle = state.path().join("cycle-000001");
        let second_cycle = state.path().join("cycle-000002");
        fs::create_dir(&first_cycle).unwrap();
        fs::create_dir(&second_cycle).unwrap();
        project::write(
            root.path(),
            "Cargo.toml",
            "[package]\nname=\"diagnostic_fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        project::write(
            root.path(),
            "src/lib.rs",
            "pub fn broken() -> u32 { \"wrong\" }\n",
        )
        .unwrap();
        let result =
            diagnostics(root.path(), &first_cycle, "0-0", &AtomicBool::new(false)).unwrap();
        assert_eq!(result["log_id"], "cycle-000001/diagnostics-0-0.log");
        assert_eq!(result["passed"], false);
        let errors = result["diagnostics"].as_array().unwrap();
        assert!(
            errors.iter().any(|d| d["code"] == "E0308"
                && d["locations"][0]["line"] == 1
                && d["locations"][0]["source"]
                    .as_str()
                    .unwrap()
                    .contains("broken")),
            "{result}"
        );
        assert!(
            read_log(&second_cycle, &json!({"log_id":result["log_id"]})).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("compiler-message")
        );
    }
    #[test]
    fn duplicate_diagnostics_keep_suggestions_and_count() {
        let root = tempfile::tempdir().unwrap();
        let msg = json!({"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[],"children":[{"message":"convert this","spans":[{"file_name":"src/lib.rs","line_start":1,"column_start":4,"suggested_replacement":".into()","suggestion_applicability":"MachineApplicable"}]}]}});
        let result = summarize(root.path(), &format!("{msg}\n{msg}\n"));
        assert_eq!(result["unique_diagnostics"], 1);
        assert_eq!(result["diagnostics"][0]["occurrences"], 2);
        assert_eq!(
            result["diagnostics"][0]["suggestions"][0]["edits"][0]["replacement"],
            ".into()"
        );
    }
}
