//! Local suggestions for missing harness capabilities. Never installs or submits anything.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub capability: String,
    pub use_case: String,
    pub reason: String,
    pub suggested_interface: String,
    pub source: String,
    pub created: String,
    pub status: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    next_id: u64,
    requests: Vec<Request>,
}

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"request_tool","description":"Record a missing harness capability for the human to consider. Explain a concrete use case and why existing tools cannot meet it. This only saves a local suggestion: it does not install tools, submit an issue, or pause work. Continue with available tools; do not repeat an already recorded request.","parameters":{"type":"object","properties":{"capability":{"type":"string"},"use_case":{"type":"string"},"reason":{"type":"string","description":"Why the currently available tools are insufficient; include attempted alternatives if relevant."},"suggested_interface":{"type":"string","description":"Optional example tool name and arguments."}},"required":["capability","use_case","reason"]}}}),
    ]
}

fn with_store<T>(state: &Path, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    fs::create_dir_all(state)?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join("tool-requests.lock"))?;
    lock.lock()?;
    let path = state.join("tool-requests.json");
    let mut store: Store = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?).context("Could not read saved tool requests")?
    } else {
        Store::default()
    };
    let result = f(&mut store)?;
    crate::setup::save(&path, &store)?;
    Ok(result)
}

fn field(args: &Value, key: &str, limit: usize) -> Result<String> {
    let value = args[key]
        .as_str()
        .with_context(|| format!("Missing {key}"))?
        .trim();
    ensure!(
        !value.is_empty() && value.len() <= limit,
        "{key} must contain 1–{limit} bytes"
    );
    ensure!(
        !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
        "{key} contains control characters"
    );
    Ok(value.into())
}
fn identity(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
pub fn record(state: &Path, args: &Value, source: &str) -> Result<Value> {
    let capability = field(args, "capability", 200)?;
    let use_case = field(args, "use_case", 4000)?;
    let reason = field(args, "reason", 4000)?;
    let suggested_interface = if args.get("suggested_interface").is_some() {
        field(args, "suggested_interface", 2000)?
    } else {
        String::new()
    };
    with_store(state, |store| {
        let duplicate = store
            .requests
            .iter()
            .find(|r| identity(&r.capability) == identity(&capability));
        let (id, duplicate) = if let Some(r) = duplicate {
            (r.id, true)
        } else {
            ensure!(
                store.requests.len() < 1000,
                "Tool request inbox is full; continue with available tools"
            );
            store.next_id += 1;
            let id = store.next_id;
            store.requests.push(Request {
                id,
                capability: capability.clone(),
                use_case,
                reason,
                suggested_interface,
                source: source.into(),
                created: chrono::Utc::now().to_rfc3339(),
                status: "new".into(),
            });
            (id, false)
        };
        Ok(
            json!({"recorded":true,"request_id":id,"duplicate":duplicate,"capability":capability,
            "instruction":"Saved locally in Settings → Tool requests for human review. No tool was installed, no issue was submitted, and work remains active. Continue with available tools; this capability request is already recorded."}),
        )
    })
}
pub fn list(state: &Path) -> Result<Vec<Request>> {
    if !state.join("tool-requests.json").exists() {
        return Ok(Vec::new());
    }
    let store: Store = serde_json::from_slice(&fs::read(state.join("tool-requests.json"))?)?;
    Ok(store.requests)
}
fn status(state: &Path, id: u64, value: &str) -> Result<()> {
    ensure!(
        ["new", "reviewed", "dismissed"].contains(&value),
        "Invalid tool request status"
    );
    with_store(state, |store| {
        let request = store
            .requests
            .iter_mut()
            .find(|r| r.id == id)
            .context("Tool request no longer exists")?;
        request.status = value.into();
        Ok(())
    })
}
fn issue_text(r: &Request) -> String {
    format!(
        "Requested capability: {}\n\nUse case\n{}\n\nWhy existing tools are insufficient\n{}\n\nSuggested interface\n{}\n\nRequested by: {}\nRecorded: {}\nStatus: {}\n\nThis is a model suggestion, not proof that the feature is needed. Review the text for private information before sharing it. Nothing is submitted automatically.",
        r.capability,
        r.use_case,
        r.reason,
        if r.suggested_interface.is_empty() {
            "Not specified"
        } else {
            &r.suggested_interface
        },
        r.source,
        r.created,
        r.status
    )
}
pub fn menu(path: &Path) -> Result<()> {
    let state = crate::runner::load(path)?.state_dir;
    loop {
        let requests = list(&state)?;
        if requests.is_empty() {
            return crate::ui::show(
                "Tool requests",
                "No missing capabilities have been requested.\n\nModels can use request_tool to save a suggestion here. Requests never install anything, pause work, or submit a GitHub issue.",
            );
        }
        let mut items: Vec<String> = requests
            .iter()
            .rev()
            .map(|r| format!("#{} · {} · {}", r.id, r.status, r.capability))
            .collect();
        items.push("Back".into());
        let Some(index) =
            crate::menu::select("Tool requests · saved locally for your review", &items, 0)?
        else {
            return Ok(());
        };
        let Some(request) = requests.iter().rev().nth(index) else {
            return Ok(());
        };
        loop {
            crate::ui::clear_notes();
            crate::ui::notice(issue_text(request));
            let choice = crate::menu::select(
                "Review tool request",
                &[
                    "View full request / issue draft".into(),
                    "Mark reviewed".into(),
                    "Dismiss".into(),
                    "Reopen".into(),
                    "Back".into(),
                ],
                0,
            )?;
            crate::ui::clear_notes();
            match choice {
                Some(0) => {
                    crate::ui::show("Tool request · review before sharing", &issue_text(request))?
                }
                Some(1) => {
                    status(&state, request.id, "reviewed")?;
                    break;
                }
                Some(2) => {
                    status(&state, request.id, "dismissed")?;
                    break;
                }
                Some(3) => {
                    status(&state, request.id, "new")?;
                    break;
                }
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(capability: &str) -> Value {
        json!({"capability":capability,"use_case":"Inspect a rendered report", "reason":"Current tools only read text"})
    }
    #[test]
    fn requests_persist_deduplicate_and_preserve_human_review() {
        let dir = tempfile::tempdir().unwrap();
        let first = record(dir.path(), &args("Image inspection"), "model").unwrap();
        status(
            dir.path(),
            first["request_id"].as_u64().unwrap(),
            "dismissed",
        )
        .unwrap();
        let again = record(dir.path(), &args(" IMAGE   INSPECTION "), "model").unwrap();
        assert_eq!(first["request_id"], again["request_id"]);
        assert_eq!(again["duplicate"], true);
        let requests = list(dir.path()).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].status, "dismissed");
        assert_eq!(requests[0].use_case, "Inspect a rendered report");
        assert!(issue_text(&requests[0]).contains("Nothing is submitted automatically"));
    }
    #[test]
    fn invalid_requests_and_corrupt_stores_do_not_destroy_saved_requests() {
        let dir = tempfile::tempdir().unwrap();
        assert!(record(dir.path(), &args(""), "model").is_err());
        fs::write(dir.path().join("tool-requests.json"), "broken").unwrap();
        assert!(record(dir.path(), &args("image"), "model").is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("tool-requests.json")).unwrap(),
            "broken"
        );
    }
    #[test]
    fn simultaneous_requests_do_not_lose_or_duplicate_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for i in 0..12 {
                let root = dir.path();
                scope.spawn(move || {
                    record(root, &args(&format!("capability {}", i % 4)), "model").unwrap();
                });
            }
        });
        let requests = list(dir.path()).unwrap();
        assert_eq!(requests.len(), 4);
        let ids: std::collections::BTreeSet<_> = requests.iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), 4);
    }
}
