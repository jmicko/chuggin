//! Operator priorities live separately from worker state so UI writes cannot be lost.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const INSTRUCTION: &str = "A nudge is the user's temporary priority within the overall project goal. Prioritize it, preserving existing work; revise your current task if needed. It can span multiple tasks. When satisfied, call finish_nudge with its exact id, a summary and concrete evidence, then return to the overall goal. Completing a task does not complete a nudge. Never invent a nudge or treat finishing it as finishing the project.";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Nudge {
    pub id: u64,
    pub request: String,
    pub created_unix: u64,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub checkpoint: String,
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub revision: u64,
    pub serial: u64,
    pub active: Option<Nudge>,
    pub history: Vec<Nudge>,
}
pub fn read(dir: &Path) -> Result<Store> {
    match fs::read(dir.join("nudges.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Cannot read saved nudges"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.into()),
    }
}
fn update(dir: &Path, edit: impl FnOnce(&mut Store) -> Result<()>) -> Result<Store> {
    fs::create_dir_all(dir)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("nudges.lock"))?;
    lock.lock()?;
    let mut store = read(dir)?;
    edit(&mut store)?;
    store.revision = store
        .revision
        .checked_add(1)
        .context("Nudge revision exhausted")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&serde_json::to_vec_pretty(&store)?)?;
    file.as_file().sync_all()?;
    file.persist(dir.join("nudges.json"))?;
    Ok(store)
}
fn text(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 8000,
        "{label} must contain 1–8000 bytes"
    );
    Ok(value.to_owned())
}
fn activate(store: &mut Store, request: String) -> Result<()> {
    if let Some(mut old) = store.active.take() {
        old.status = "replaced".into();
        store.history.push(old);
    }
    store.serial = store.serial.checked_add(1).context("Nudge IDs exhausted")?;
    store.active = Some(Nudge {
        id: store.serial,
        request,
        created_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        status: "active".into(),
        summary: String::new(),
        evidence: String::new(),
        checkpoint: String::new(),
    });
    Ok(())
}
pub fn set(dir: &Path, request: &str, expected: Option<u64>) -> Result<Store> {
    let request = text(request, "Nudge")?;
    update(dir, |s| {
        anyhow::ensure!(
            s.active.as_ref().map(|n| n.id) == expected,
            "The active nudge changed; close and reopen the panel"
        );
        activate(s, request)
    })
}
pub fn cancel(dir: &Path, id: u64) -> Result<Store> {
    update(dir, |s| {
        anyhow::ensure!(
            s.active.as_ref().map(|n| n.id) == Some(id),
            "The active nudge changed; reopen the nudge panel"
        );
        let mut n = s.active.take().unwrap();
        n.status = "cancelled".into();
        s.history.push(n);
        Ok(())
    })
}
pub fn reopen(dir: &Path) -> Result<Store> {
    update(dir, |s| {
        anyhow::ensure!(
            s.active.is_none(),
            "Cancel the active nudge before reopening an earlier one"
        );
        let request = s
            .history
            .last()
            .context("No earlier nudge to reopen")?
            .request
            .clone();
        activate(s, request)
    })
}
pub fn finish(
    dir: &Path,
    id: u64,
    summary: &str,
    evidence: &str,
    checkpoint: &str,
) -> Result<Store> {
    let summary = text(summary, "Completion summary")?;
    let evidence = text(evidence, "Completion evidence")?;
    update(dir, |s| {
        anyhow::ensure!(
            s.active.as_ref().map(|n| n.id) == Some(id),
            "This nudge is no longer active; it may have been replaced, cancelled or completed. Read the next priority update."
        );
        let mut n = s.active.take().unwrap();
        n.status = "completed".into();
        n.summary = summary;
        n.evidence = evidence;
        n.checkpoint = checkpoint.into();
        s.history.push(n);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_rejects_stale_completion_and_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        set(p, "Make the UI usable", None).unwrap();
        set(p, "Compare other retailers", Some(1)).unwrap();
        assert!(finish(p, 1, "Done", "Evidence", "abc").is_err());
        assert!(cancel(p, 1).is_err());
        assert!(set(p, "Stale draft", Some(1)).is_err());
        assert!(finish(p, 2, "Done", " ", "abc").is_err());
        let s = finish(p, 2, "Compared", "Found three alternatives", "abc").unwrap();
        assert!(s.active.is_none());
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[0].status, "replaced");
        assert_eq!(s.history[1].evidence, "Found three alternatives");
        let s = reopen(p).unwrap();
        assert_eq!(s.active.unwrap().id, 3);
        cancel(p, 3).unwrap();
        assert_eq!(read(p).unwrap().history[2].status, "cancelled");
    }
    #[test]
    fn simultaneous_updates_do_not_lose_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let p = dir.path().to_owned();
                std::thread::spawn(move || set(&p, "Priority", None).is_ok())
            })
            .collect();
        assert_eq!(
            jobs.into_iter()
                .filter_map(|j| j.join().ok())
                .filter(|ok| *ok)
                .count(),
            1
        );
        assert_eq!(read(dir.path()).unwrap().revision, 1);
    }
}
