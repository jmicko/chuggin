//! Host-owned associations between persistent private computers and projects.
//! Discovery never starts/stops a computer or writes into a project. Destructive
//! operations require a specific managed identity and exclusive project locks.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Computer {
    pub name: String,
    pub project: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub running: bool,
    pub orphaned: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    name: String,
    project: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    identity: Option<String>,
    container_id: Option<String>,
    #[serde(default)]
    snapshots: Vec<String>,
    #[serde(default)]
    deleting: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Registry {
    computers: BTreeMap<String, Record>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    name: String,
    token: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Recovery {
    transaction: String,
    original: Record,
    source_id: String,
    backup_name: String,
    snapshot: Option<String>,
    new_project: PathBuf,
    new_state: PathBuf,
    config: PathBuf,
    config_before: Value,
    state_before: Option<Value>,
    ready_before: Option<Value>,
    phase: String,
}

fn directory() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_DIRECTORY.with(|v| v.borrow().clone()) {
        return Ok(path);
    }
    Ok(crate::setup::settings_path()?.with_file_name("computers"))
}
fn token() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot create computer identity"))?;
    Ok(bytes.iter().map(|v| format!("{v:02x}")).collect())
}
fn valid_name(name: &str) -> bool {
    name.strip_prefix("chuggin-")
        .is_some_and(|v| v.len() == 24 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn valid_image(image: &str) -> bool {
    let hash = image.strip_prefix("sha256:").unwrap_or(image);
    !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit())
}
fn file(path: &Path, create: bool) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = f.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
        "Computer records must be ordinary files owned by this user"
    );
    ensure!(
        metadata.len() <= 16 * 1024 * 1024,
        "Computer record is too large"
    );
    if create {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}
fn read_registry() -> Result<Registry> {
    match file(&directory()?.join("registry.json"), false) {
        Ok(f) => {
            let registry: Registry =
                serde_json::from_reader(f).context("Cannot read private-computer registry")?;
            for (name, record) in &registry.computers {
                ensure!(
                    valid_name(name) && record.name == *name,
                    "Private-computer registry has invalid identities"
                );
                ensure!(
                    record.project.as_ref().is_none_or(|p| p.is_absolute())
                        && record.state_dir.as_ref().is_none_or(|p| p.is_absolute())
                        && record.config.as_ref().is_none_or(|p| p.is_absolute()),
                    "Private-computer registry paths must be absolute"
                );
                ensure!(
                    record
                        .identity
                        .as_deref()
                        .is_none_or(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit())),
                    "Private-computer registry has an invalid session token"
                );
            }
            Ok(registry)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(Registry::default())
        }
        Err(error) => Err(error),
    }
}
fn make_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() },
        "Private-computer records need a user-owned directory"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("Missing computer record directory")?;
    if !parent.is_dir() {
        make_directory(parent)?;
    }
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut staged, value)?;
    staged.write_all(b"\n")?;
    staged.as_file().sync_all()?;
    staged
        .persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot save private-computer record"))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn update_registry(f: impl FnOnce(&mut Registry) -> Result<()>) -> Result<()> {
    let dir = directory()?;
    make_directory(&dir)?;
    let lock = file(&dir.join("registry.lock"), true)?;
    lock.lock()?;
    let mut registry = read_registry()?;
    f(&mut registry)?;
    atomic(&dir.join("registry.json"), &registry)
}
fn record(name: &str) -> Result<Record> {
    ensure!(valid_name(name), "Invalid private-computer identity");
    read_registry()?
        .computers
        .remove(name)
        .context("Private computer is not registered; refresh the list first")
}
fn save_record(record: &Record) -> Result<()> {
    update_registry(|registry| {
        registry
            .computers
            .insert(record.name.clone(), record.clone());
        Ok(())
    })
}
fn marker(state: &Path) -> Option<Identity> {
    let bytes = fs::read(state.join("computer/identity.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}
fn ready_name(state: &Path) -> Option<String> {
    let bytes = fs::read(state.join("computer/ready.json")).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["name"].as_str().map(str::to_owned)
}
fn belongs(record: &Record) -> bool {
    let Some(root) = record.project.as_ref() else {
        return false;
    };
    let Some(state) = record.state_dir.as_ref() else {
        return false;
    };
    if !root.is_dir() || !state.is_dir() || record.config.as_ref().is_some_and(|p| !p.is_file()) {
        return false;
    }
    match record.identity.as_deref() {
        Some(identity) => {
            marker(state).is_some_and(|m| m.name == record.name && m.token == identity)
        }
        None => ready_name(state).is_some_and(|name| name == record.name),
    }
}
fn legacy_name(root: &Path) -> String {
    let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    format!("chuggin-{}", &hash[..24])
}

/// A copied/moved identity is never automatically adopted at a new path.
pub fn name_for(c: &crate::runner::Config) -> Result<String> {
    let root = fs::canonicalize(&c.repo)?;
    let registry = read_registry()?;
    if let Some(identity) = marker(&c.state_dir) {
        ensure!(
            valid_name(&identity.name),
            "Saved private-computer identity is invalid"
        );
        let r=registry.computers.get(&identity.name).context("Private computer association is missing. Open Global settings → Private computers to reconnect it.")?;
        ensure!(
            !r.deleting && recovery(&identity.name)?.is_none(),
            "This private computer has unfinished deletion or reconnect work. Finish it in Global settings → Private computers before resuming."
        );
        ensure!(
            r.project.as_ref() == Some(&root)
                && r.state_dir.as_ref() == Some(&c.state_dir)
                && r.identity.as_deref() == Some(identity.token.as_str()),
            "This private computer belongs to another project location. Open Global settings → Private computers and reconnect the moved project."
        );
        return Ok(identity.name);
    }
    let name = legacy_name(&root);
    if let Some(saved) = ready_name(&c.state_dir) {
        ensure!(
            saved == name,
            "This project was moved or copied. Reconnect its private computer from Global settings → Private computers."
        );
    }
    if let Some(r) = registry.computers.get(&name) {
        ensure!(
            !r.deleting && recovery(&name)?.is_none(),
            "This private computer has unfinished deletion or reconnect work. Finish it in Global settings → Private computers before resuming."
        );
        ensure!(
            r.identity.is_none()
                && r.project.as_ref() == Some(&root)
                && r.state_dir.as_ref() == Some(&c.state_dir)
                && belongs(r),
            "A previous private computer remains at this path. Reconnect or delete it in Global settings → Private computers before creating a new one."
        );
    }
    Ok(name)
}

pub(crate) fn authorize_existing(c: &crate::runner::Config, name: &str) -> Result<()> {
    ensure!(
        ready_name(&c.state_dir).as_deref() == Some(name)
            || marker(&c.state_dir).is_some_and(|m| m.name == name),
        "A private computer already exists for this path, but this project does not own its saved identity. Reconnect or delete it in Global settings → Private computers."
    );
    ensure!(name_for(c)? == name, "Private computer identity changed");
    Ok(())
}
pub(crate) fn authorize_missing(name: &str) -> Result<()> {
    ensure!(
        read_registry()?
            .computers
            .get(name)
            .is_none_or(|r| r.container_id.is_none()),
        "This project's private computer was removed outside Chuggin. Remove its saved association in Global settings → Private computers before preparing a fresh filesystem."
    );
    Ok(())
}
pub(crate) fn verify_instance(name: &str, instance: &Value) -> Result<()> {
    let registry = read_registry()?;
    if let Some(expected) = registry
        .computers
        .get(name)
        .and_then(|r| r.container_id.as_deref())
    {
        ensure!(
            id(instance)? == expected,
            "A different container now uses this saved private computer's name. Inspect it in Global settings → Private computers before resuming."
        );
    }
    Ok(())
}
pub(crate) fn remember(
    c: &crate::runner::Config,
    name: &str,
    instance: Option<&Value>,
) -> Result<()> {
    if let Some(instance) = instance {
        verify_instance(name, instance)?;
    }
    let root = fs::canonicalize(&c.repo)?;
    let existing = read_registry()?.computers.remove(name);
    let identity = marker(&c.state_dir)
        .filter(|m| m.name == name)
        .map(|m| m.token)
        .or_else(|| existing.as_ref().and_then(|r| r.identity.clone()))
        .map(Ok)
        .unwrap_or_else(token)?;
    let config = root.join("chuggin.json");
    let r = Record {
        name: name.into(),
        project: Some(root),
        state_dir: Some(c.state_dir.clone()),
        config: config.is_file().then_some(config),
        identity: Some(identity.clone()),
        container_id: instance
            .and_then(|v| v["Id"].as_str().or(v["ID"].as_str()))
            .map(str::to_owned)
            .or_else(|| existing.as_ref().and_then(|r| r.container_id.clone())),
        snapshots: existing.map(|r| r.snapshots).unwrap_or_default(),
        deleting: false,
    };
    save_record(&r)?;
    atomic(
        &c.state_dir.join("computer/identity.json"),
        &Identity {
            name: name.into(),
            token: identity,
        },
    )
}

fn podman(args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    #[cfg(test)]
    if let Some(result) = TEST_PODMAN.with(|mock| mock.borrow_mut().as_mut().map(|f| f(args))) {
        return result;
    }
    let mut command = Command::new("podman");
    command
        .arg("--remote=false")
        .args(args)
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION");
    crate::sandbox::bounded(command, None, timeout, 32 * 1024 * 1024)
}
fn inspect(name: &str) -> Result<Value> {
    let data = podman(&["inspect", name], Duration::from_secs(10))?;
    serde_json::from_slice::<Vec<Value>>(&data)?
        .into_iter()
        .next()
        .context("Private computer was not found")
}
fn inspect_optional(name: &str) -> Result<Option<Value>> {
    match inspect(name) {
        Ok(instance) => Ok(Some(instance)),
        Err(error) => {
            // An inspect error can be a transient Podman failure. A successful
            // inventory must establish absence before dropping recovery links.
            let list: Vec<Value> = serde_json::from_slice(&podman(
                &["ps", "-a", "--no-trunc", "--format=json"],
                Duration::from_secs(8),
            )?)?;
            let exists = list.iter().any(|v| {
                v["Id"] == name
                    || v["ID"] == name
                    || v["Names"] == name
                    || v["Name"] == name
                    || v["Names"]
                        .as_array()
                        .is_some_and(|names| names.iter().any(|n| n == name))
            });
            if exists {
                Err(error
                    .context("Cannot verify this private computer; recovery records were retained"))
            } else {
                Ok(None)
            }
        }
    }
}
fn owned(value: &Value, name: &str) -> Result<()> {
    ensure!(
        valid_name(name)
            && value["Config"]["Labels"]["io.chuggin.managed"] == "true"
            && value["Config"]["Labels"]["io.chuggin.session"] == name,
        "This container is not a Chuggin-managed private computer for this session"
    );
    Ok(())
}
fn instance_name(value: &Value) -> Option<&str> {
    value["Name"].as_str().map(|n| n.trim_start_matches('/'))
}
fn id(value: &Value) -> Result<&str> {
    value["Id"]
        .as_str()
        .or(value["ID"].as_str())
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("Podman returned an invalid computer ID")
}
fn inferred_record(value: &Value, name: &str) -> Record {
    let state = value["Mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|m| m["Destination"] == "/opt/session-runtime")
        })
        .and_then(|m| m["Source"].as_str())
        .map(PathBuf::from)
        .and_then(|p| {
            if p.file_name().is_some_and(|n| n == "tool-runtime")
                && p.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "computer")
            {
                p.parent().and_then(Path::parent).map(Path::to_path_buf)
            } else {
                None
            }
        });
    let project = state
        .as_ref()
        .filter(|p| p.file_name().is_some_and(|v| v == ".chuggin"))
        .and_then(|p| p.parent())
        .map(Path::to_path_buf);
    let config = project
        .as_ref()
        .map(|p| p.join("chuggin.json"))
        .filter(|p| p.is_file());
    let identity = state
        .as_deref()
        .and_then(marker)
        .filter(|m| {
            m.name == name && m.token.len() == 32 && m.token.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(|m| m.token);
    Record {
        name: name.into(),
        project,
        state_dir: state,
        config,
        identity,
        container_id: id(value).ok().map(str::to_owned),
        snapshots: vec![],
        deleting: false,
    }
}
fn recovery_path(name: &str) -> Result<PathBuf> {
    Ok(directory()?.join("recoveries").join(format!("{name}.json")))
}
fn recovery(name: &str) -> Result<Option<Recovery>> {
    let path = recovery_path(name)?;
    match file(&path, false) {
        Ok(f) => Ok(Some(serde_json::from_reader(f)?)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
fn save_recovery(name: &str, recovery: &Recovery) -> Result<()> {
    atomic(&recovery_path(name)?, recovery)
}

/// Read-only Podman discovery. Legacy associations are imported into the global
/// registry only; no project marker or container is changed during discovery.
pub fn scan() -> Result<Vec<Computer>> {
    #[cfg(test)]
    let installed = TEST_PODMAN.with(|mock| mock.borrow().is_some()) || crate::sandbox::installed();
    #[cfg(not(test))]
    let installed = crate::sandbox::installed();
    if !installed {
        return Ok(vec![]);
    }
    let list: Vec<Value> = serde_json::from_slice(&podman(
        &[
            "ps",
            "-a",
            "--filter",
            "label=io.chuggin.managed=true",
            "--format=json",
        ],
        Duration::from_secs(8),
    )?)?;
    ensure!(
        list.len() <= 1024,
        "Too many managed computers to inspect at once"
    );
    let names = list
        .iter()
        .filter_map(|v| {
            v["Names"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .or(v["Names"].as_str())
                .or(v["Name"].as_str())
        })
        .filter(|n| valid_name(n))
        .collect::<Vec<_>>();
    let mut found = BTreeMap::new();
    if !names.is_empty() {
        let mut args = vec!["inspect"];
        args.extend(names);
        let inspected: Vec<Value> =
            serde_json::from_slice(&podman(&args, Duration::from_secs(12))?)?;
        for value in inspected {
            if let Some(name) = instance_name(&value).filter(|n| valid_name(n))
                && owned(&value, name).is_ok()
            {
                found.insert(name.to_owned(), value);
            }
        }
    }
    let mut registry = read_registry()?;
    let mut additions = vec![];
    for (name, value) in &found {
        if !registry.computers.contains_key(name) {
            let r = inferred_record(value, name);
            registry.computers.insert(name.clone(), r.clone());
            additions.push(r);
        }
    }
    if !additions.is_empty() {
        update_registry(|r| {
            for entry in additions {
                r.computers.entry(entry.name.clone()).or_insert(entry);
            }
            Ok(())
        })?;
    }
    let mut results = vec![];
    for (name, r) in registry.computers {
        let pending = recovery(&name)?.is_some();
        let instance = found.get(&name);
        if instance.is_none()
            && !pending
            && r.snapshots.is_empty()
            && !r.deleting
            && r.container_id.is_none()
        {
            continue;
        }
        let same_id = instance.is_none_or(|v| {
            r.container_id
                .as_deref()
                .is_none_or(|expected| id(v).ok() == Some(expected))
        });
        let linked = belongs(&r) && same_id && instance.is_some() && !pending && !r.deleting;
        let reason = if r.deleting {
            "Deletion did not finish; delete again to finish cleanup of this computer."
        } else if pending {
            "A previous reconnect was interrupted; reconnect again to recover safely."
        } else if instance.is_none() && !r.snapshots.is_empty() {
            "The container was removed; private snapshot cleanup is still needed."
        } else if instance.is_none() {
            "The private computer is missing. Remove its saved association to start fresh."
        } else if !same_id {
            "A different container now uses this saved name."
        } else if r.project.as_ref().is_none_or(|p| !p.is_dir()) {
            "The project folder is missing or its location is unknown."
        } else if !linked {
            "The project history is missing or belongs to another session."
        } else {
            "Project and private computer are connected."
        };
        results.push(Computer {
            name,
            project: r.project,
            state_dir: r.state_dir,
            running: instance.is_some_and(|v| v["State"]["Running"] == true),
            orphaned: !linked,
            reason: reason.into(),
        });
    }
    Ok(results)
}

fn exclusive(path: &Path) -> Result<File> {
    let f = file(path, true)?;
    use std::os::fd::AsRawFd;
    ensure!(
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another Chuggin controller or computer operation is using this project. Stop it and close its controller first."
    );
    Ok(f)
}
fn operation_lock(name: &str) -> Result<File> {
    ensure!(valid_name(name), "Invalid private-computer identity");
    let dir = directory()?.join("operations");
    make_directory(&dir)?;
    exclusive(&dir.join(format!("{name}.lock")))
}
fn old_locks(r: &Record, new_root: Option<&Path>, new_state: Option<&Path>) -> Result<Vec<File>> {
    let mut locks = vec![];
    if let Some(root) = r.project.as_deref().filter(|p| Some(*p) != new_root) {
        let dir = locks_directory()?;
        make_directory(&dir)?;
        locks.push(exclusive(&dir.join(format!(
            "workspace-{:x}.lock",
            Sha256::digest(root.as_os_str().as_encoded_bytes())
        )))?);
    }
    if let Some(state) = r.state_dir.as_deref().filter(|p| Some(*p) != new_state) {
        let path = state.join("run.lock");
        if path.is_file() {
            locks.push(exclusive(&path)?);
        }
    }
    Ok(locks)
}
fn locks_directory() -> Result<PathBuf> {
    #[cfg(test)]
    if TEST_PODMAN.with(|mock| mock.borrow().is_some()) {
        return Ok(directory()?.join("runtime"));
    }
    Ok(crate::setup::settings_path()?
        .parent()
        .unwrap()
        .join("runtime"))
}
fn new_locks(c: &crate::runner::Config) -> Result<Vec<File>> {
    #[cfg(test)]
    if TEST_PODMAN.with(|mock| mock.borrow().is_some()) {
        fs::create_dir_all(&c.state_dir)?;
        let r = Record {
            name: String::new(),
            project: Some(c.repo.clone()),
            state_dir: Some(c.state_dir.clone()),
            config: None,
            identity: None,
            container_id: None,
            snapshots: vec![],
            deleting: false,
        };
        return old_locks(&r, None, None);
    }
    crate::runner::lock_project(c)
}

/// Called only after the UI confirms deletion of this exact computer.
pub fn delete(name: &str) -> Result<()> {
    let _operation = operation_lock(name)?;
    let mut r = record(name)?;
    let _ownership = old_locks(&r, None, None)?;
    let pending = recovery(name)?;
    if r.identity.is_none() && belongs(&r) {
        // Legacy cleanup also needs an ownership proof that outlives ready.json.
        // This is an explicitly confirmed operation, never discovery.
        let identity = token()?;
        atomic(
            &r.state_dir.as_ref().unwrap().join("computer/identity.json"),
            &Identity {
                name: name.into(),
                token: identity.clone(),
            },
        )?;
        r.identity = Some(identity);
        save_record(&r)?;
    }
    let mut snapshots = r.snapshots.clone();
    let current = inspect_optional(name)?;
    if let Some(instance) = current {
        owned(&instance, name)?;
        ensure!(
            r.container_id
                .as_deref()
                .is_none_or(|expected| id(&instance).ok() == Some(expected))
                || pending.as_ref().is_some_and(
                    |journal| instance["Config"]["Labels"]["io.chuggin.reconnect"]
                        == journal.transaction
                ),
            "Saved computer identity changed; refusing to remove another container"
        );
        r.deleting = true;
        save_record(&r)?;
        podman(&["rm", "--force", id(&instance)?], Duration::from_secs(30))?;
    } else {
        // A list round-trip distinguishes a removed container from a broken
        // Podman daemon; cleanup is never inferred from an inspect failure.
        let listed: Vec<Value> = serde_json::from_slice(&podman(
            &[
                "ps",
                "-a",
                "--filter",
                "label=io.chuggin.managed=true",
                "--format=json",
            ],
            Duration::from_secs(8),
        )?)?;
        ensure!(
            !listed.iter().any(|v| v["Names"]
                .as_array()
                .is_some_and(|a| a.iter().any(|n| n == name))
                || v["Names"] == name
                || v["Name"] == name),
            "Cannot verify the selected private computer; nothing was removed"
        );
    }
    if let Some(journal) = pending {
        if let Some(original) = inspect_optional(&journal.source_id)? {
            owned(&original, name)?;
            podman(&["rm", "--force", id(&original)?], Duration::from_secs(30))?;
        }
        if let Some(image) = journal.snapshot
            && !snapshots.contains(&image)
        {
            snapshots.push(image);
        }
    }
    r.deleting = true;
    r.snapshots = snapshots.clone();
    save_record(&r)?;
    // Newer private snapshots can depend on older ones after repeated moves.
    for image in snapshots.into_iter().rev() {
        ensure!(
            valid_image(&image),
            "Invalid saved private snapshot identity"
        );
        podman(&["image","rm",&image],Duration::from_secs(30)).with_context(||format!("Computer removed, but its private recovery snapshot {image} could not be removed. It may still be used by another container; no shared images were forced or pruned."))?;
        r.snapshots.retain(|saved| saved != &image);
        save_record(&r)?;
        if let Some(mut pending) = recovery(name)?
            && pending.snapshot.as_deref() == Some(image.as_str())
        {
            pending.snapshot = None;
            save_recovery(name, &pending)?;
        }
    }
    if belongs(&r)
        && let Some(state) = &r.state_dir
    {
        for path in ["computer/ready.json", "computer/desktop-apps.json"] {
            match fs::remove_file(state.join(path)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        atomic(
            &state.join("computer/reset-notice.json"),
            &json!({"created_at":chrono::Utc::now(),"reason":"The user deleted this private computer. Its filesystem was removed; conversation and self-profile records remain. A newly prepared computer starts with a fresh filesystem."}),
        )?;
        match fs::remove_file(state.join("computer/identity.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match fs::remove_file(recovery_path(name)?) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    update_registry(|registry| {
        registry.computers.remove(name);
        Ok(())
    })?;
    Ok(())
}

fn rebase(value: &mut Value, key: &str, old: Option<&Path>, new: &Path) {
    if let Some(path) = value[key]
        .as_str()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        && let Some(suffix) = old.and_then(|old| path.strip_prefix(old).ok())
    {
        value[key] = json!(if suffix.as_os_str().is_empty() {
            new.to_path_buf()
        } else {
            new.join(suffix)
        });
    }
}
fn read_optional(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn config_for(path: &Path, original: &Record) -> Result<(PathBuf, Value, crate::runner::Config)> {
    let path = fs::canonicalize(if path.is_dir() {
        path.join("chuggin.json")
    } else {
        path.to_path_buf()
    })
    .context("Choose the moved project's folder or chuggin.json file")?;
    let parent = path.parent().context("Missing project folder")?;
    let before: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let mut value = before.clone();
    rebase(&mut value, "repo", original.project.as_deref(), parent);
    rebase(&mut value, "state_dir", original.project.as_deref(), parent);
    let mut merged = serde_json::to_value(crate::setup::settings()?)?;
    for (key, value) in value
        .as_object()
        .context("Project configuration is invalid")?
    {
        merged[key] = value.clone();
    }
    let mut c: crate::runner::Config = serde_json::from_value(merged)?;
    if c.repo.is_relative() {
        c.repo = parent.join(&c.repo);
    }
    c.repo = fs::canonicalize(&c.repo)?;
    if c.state_dir.is_relative() {
        c.state_dir = parent.join(&c.state_dir);
    }
    ensure!(
        c.execution.is_private() && c.activity_mode.is_chaos(),
        "Choose an unassigned project configured for a private computer"
    );
    c.execution.validate()?;
    Ok((path, value, c))
}

pub fn reconnect(name: &str, project_path: &Path) -> Result<()> {
    let _operation = operation_lock(name)?;
    let entry = scan()?
        .into_iter()
        .find(|c| c.name == name)
        .context("Private computer is no longer listed; refresh and try again")?;
    ensure!(
        entry.orphaned,
        "This computer is still connected to an existing project. Move the original folder rather than sharing one computer between project copies."
    );
    let pending = recovery(name)?;
    let original = pending
        .as_ref()
        .map(|journal| journal.original.clone())
        .unwrap_or(record(name)?);
    ensure!(
        !record(name)?.deleting,
        "This computer is being deleted; finish its deletion before creating another one"
    );
    let (config_path, proposed, c) = config_for(project_path, &original)?;
    let old_name = ready_name(&c.state_dir);
    let matching = match original.identity.as_deref() {
        Some(expected) => {
            marker(&c.state_dir).is_some_and(|m| m.name == name && m.token == expected)
        }
        None => old_name.as_deref() == Some(name),
    };
    ensure!(
        matching,
        "The selected folder does not contain this computer's saved identity. Choose the original moved project, including its .chuggin history."
    );
    let _old = old_locks(&original, Some(&c.repo), Some(&c.state_dir))?;
    let _new = new_locks(&c)?;
    let mut journal = if let Some(journal) = pending {
        ensure!(
            journal.new_project == c.repo
                && journal.new_state == c.state_dir
                && journal.config == config_path,
            "Reconnect recovery is already prepared for another folder"
        );
        journal
    } else {
        let source = inspect(name)?;
        owned(&source, name)?;
        ensure!(
            original
                .container_id
                .as_deref()
                .is_none_or(|expected| id(&source).ok() == Some(expected)),
            "Private computer identity changed"
        );
        let transaction = token()?;
        let journal = Recovery {
            transaction: transaction.clone(),
            original: original.clone(),
            source_id: id(&source)?.into(),
            backup_name: format!("{name}-previous-{}", &transaction[..8]),
            snapshot: None,
            new_project: c.repo.clone(),
            new_state: c.state_dir.clone(),
            config: config_path.clone(),
            config_before: serde_json::from_slice(&fs::read(&config_path)?)?,
            state_before: read_optional(&c.state_dir.join("state.json"))?,
            ready_before: read_optional(&c.state_dir.join("computer/ready.json"))?,
            phase: "snapshot".into(),
        };
        save_recovery(name, &journal)?;
        journal
    };
    if journal.phase == "connected" {
        let candidate = inspect(name)?;
        owned(&candidate, name)?;
        ensure!(
            candidate["Config"]["Labels"]["io.chuggin.reconnect"] == journal.transaction,
            "Reconnected computer identity changed"
        );
        if let Some(source) = inspect_optional(&journal.source_id)? {
            owned(&source, name)?;
            podman(&["rm", &journal.source_id], Duration::from_secs(30))?;
        }
        fs::remove_file(recovery_path(name)?)?;
        return Ok(());
    }
    let result = (|| -> Result<()> {
        let source = inspect(&journal.source_id)?;
        owned(&source, name)?;
        if source["State"]["Running"] == true {
            podman(
                &["stop", "--time=10", &journal.source_id],
                Duration::from_secs(25),
            )?;
        }
        if journal.snapshot.is_none() {
            let tag = format!("localhost/chuggin-private-snapshot:{}", journal.transaction);
            let bytes = podman(
                &[
                    "commit",
                    "--include-volumes=false",
                    "--pause=true",
                    &journal.source_id,
                    &tag,
                ],
                Duration::from_secs(600),
            )?;
            let image = String::from_utf8(bytes)?.trim().to_owned();
            ensure!(
                valid_image(&image),
                "Podman returned an invalid recovery snapshot identity"
            );
            journal.snapshot = Some(image.clone());
            let mut r = original.clone();
            if !r.snapshots.contains(&image) {
                r.snapshots.push(image);
            }
            save_record(&r)?;
            save_recovery(name, &journal)?;
        }
        if instance_name(&source) == Some(name) {
            podman(
                &["rename", &journal.source_id, &journal.backup_name],
                Duration::from_secs(20),
            )?;
        }
        if let Some(candidate) = inspect_optional(name)? {
            owned(&candidate, name)?;
            ensure!(
                candidate["Config"]["Labels"]["io.chuggin.reconnect"] == journal.transaction,
                "Another container now occupies this private computer's name"
            );
            podman(&["rm", "--force", id(&candidate)?], Duration::from_secs(30))?;
        }
        journal.phase = "recreate".into();
        save_recovery(name, &journal)?;
        let instance = recreate(
            &c,
            name,
            journal.snapshot.as_deref().unwrap(),
            &journal.transaction,
        )?;
        if let Some(mut state) = journal.state_before.clone() {
            rebase(&mut state, "repo", original.project.as_deref(), &c.repo);
            rebase(
                &mut state,
                "working_workspace",
                original.project.as_deref(),
                &c.repo,
            );
            atomic(&c.state_dir.join("state.json"), &state)?;
        }
        atomic(&config_path, &proposed)?;
        let identity = original.identity.clone().map(Ok).unwrap_or_else(token)?;
        let mut linked = record(name)?;
        linked.project = Some(c.repo.clone());
        linked.state_dir = Some(c.state_dir.clone());
        linked.config = Some(config_path.clone());
        linked.identity = Some(identity.clone());
        linked.container_id = Some(id(&instance)?.into());
        save_record(&linked)?;
        atomic(
            &c.state_dir.join("computer/identity.json"),
            &Identity {
                name: name.into(),
                token: identity,
            },
        )?;
        journal.phase = "connected".into();
        save_recovery(name, &journal)?;
        podman(&["rm", &journal.source_id], Duration::from_secs(30))?;
        fs::remove_file(recovery_path(name)?)?;
        Ok(())
    })();
    result.with_context(||format!("Reconnect did not finish. The original installation and private snapshot are retained. Recovery details: {}. Retry reconnecting this same folder; no unrelated computer or image was removed.",recovery_path(name).map(|p|p.display().to_string()).unwrap_or_default()))
}

fn recreate(
    c: &crate::runner::Config,
    name: &str,
    image: &str,
    transaction: &str,
) -> Result<Value> {
    #[cfg(test)]
    if let Some(result) = TEST_RECREATE.with(|mock| {
        mock.borrow_mut()
            .as_mut()
            .map(|f| f(c, name, image, transaction))
    }) {
        return result;
    }
    crate::sandbox::reconnect_prepare(c, name, image, transaction)
}

#[cfg(test)]
thread_local! {static TEST_DIRECTORY:std::cell::RefCell<Option<PathBuf>>=const{std::cell::RefCell::new(None)};}
#[cfg(test)]
type PodmanMock = Box<dyn FnMut(&[&str]) -> Result<Vec<u8>>>;
#[cfg(test)]
type RecreateMock = Box<dyn FnMut(&crate::runner::Config, &str, &str, &str) -> Result<Value>>;
#[cfg(test)]
thread_local! {
    static TEST_PODMAN:std::cell::RefCell<Option<PodmanMock>>=const{std::cell::RefCell::new(None)};
    static TEST_RECREATE:std::cell::RefCell<Option<RecreateMock>>=const{std::cell::RefCell::new(None)};
}
#[cfg(test)]
pub(crate) struct TestDirectory(Option<PathBuf>);
#[cfg(test)]
impl Drop for TestDirectory {
    fn drop(&mut self) {
        TEST_DIRECTORY.with(|v| *v.borrow_mut() = self.0.take());
    }
}
#[cfg(test)]
pub(crate) fn test_directory(path: &Path) -> TestDirectory {
    TestDirectory(TEST_DIRECTORY.with(|v| v.borrow_mut().replace(path.into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Default)]
    struct Fake {
        containers: BTreeMap<String, Value>,
        images: BTreeMap<String, Value>,
        calls: Vec<Vec<String>>,
        fail_create: bool,
        fail_original_cleanup: bool,
        fail_image_cleanup: bool,
    }
    struct Mock;
    impl Drop for Mock {
        fn drop(&mut self) {
            TEST_PODMAN.with(|mock| *mock.borrow_mut() = None);
            TEST_RECREATE.with(|mock| *mock.borrow_mut() = None);
        }
    }
    fn mocks(fake: Rc<RefCell<Fake>>) -> Mock {
        let commands = fake.clone();
        TEST_PODMAN.with(|mock| {
            *mock.borrow_mut() = Some(Box::new(move |args| {
                let mut fake = commands.borrow_mut();
                fake.calls
                    .push(args.iter().map(|v| v.to_string()).collect());
                let find = |fake: &Fake, key: &str| {
                    fake.containers
                        .iter()
                        .find(|(name, v)| name.as_str() == key || v["Id"] == key)
                        .map(|(name, v)| (name.clone(), v.clone()))
                };
                let value = match args[0] {
                    "ps" => json!(
                        fake.containers
                            .iter()
                            .map(|(name, value)| json!({"Names":[name],"Id":value["Id"]}))
                            .collect::<Vec<_>>()
                    ),
                    "inspect" => json!(
                        args[1..]
                            .iter()
                            .map(|name| find(&fake, name)
                                .map(|(_, v)| v)
                                .context("Container absent"))
                            .collect::<Result<Vec<_>>>()?
                    ),
                    "stop" => {
                        let (name, mut value) = find(&fake, args[2]).context("Container absent")?;
                        value["State"]["Running"] = json!(false);
                        fake.containers.insert(name, value);
                        json!({})
                    }
                    "commit" => {
                        let (_, source) = find(&fake, args[3]).context("Container absent")?;
                        let image = "c".repeat(64);
                        fake.images.insert(image.clone(), source);
                        return Ok(image.into_bytes());
                    }
                    "rename" => {
                        let (name, mut value) = find(&fake, args[1]).context("Container absent")?;
                        fake.containers.remove(&name);
                        value["Name"] = json!(args[2]);
                        fake.containers.insert(args[2].into(), value);
                        json!({})
                    }
                    "rm" => {
                        let key = args.last().unwrap();
                        if let Some((name, value)) = find(&fake, key) {
                            if fake.fail_original_cleanup
                                && value["Id"] == "a".repeat(64)
                                && args.len() == 2
                            {
                                fake.fail_original_cleanup = false;
                                anyhow::bail!("Simulated original cleanup failure");
                            }
                            fake.containers.remove(&name);
                        }
                        json!({})
                    }
                    "image" => {
                        assert_eq!(args[1], "rm");
                        if fake.fail_image_cleanup {
                            fake.fail_image_cleanup = false;
                            anyhow::bail!("Image busy");
                        }
                        fake.images.remove(args[2]);
                        json!({})
                    }
                    other => anyhow::bail!("Unexpected mock operation {other}"),
                };
                Ok(serde_json::to_vec(&value)?)
            }))
        });
        TEST_RECREATE.with(|mock|*mock.borrow_mut()=Some(Box::new(move|c,name,image,transaction|{
            let mut fake=fake.borrow_mut();fake.calls.push(vec!["recreate".into(),name.into(),image.into()]);
            if fake.fail_create{anyhow::bail!("Simulated replacement failure");}
            let mut value=fake.images.get(image).cloned().context("Image absent")?;
            value["Name"]=json!(name);value["Id"]=json!("b".repeat(64));value["Config"]["Labels"]["io.chuggin.reconnect"]=json!(transaction);
            value["Mounts"]=json!([{"Destination":"/opt/session-runtime","Source":c.state_dir.join("computer/tool-runtime"),"RW":false},{"Destination":"/run/public-web","Source":c.state_dir.join("computer/channel"),"RW":false}]);
            fake.containers.insert(name.into(),value.clone());
            atomic(&c.state_dir.join("computer/ready.json"),&json!({"name":name,"image":image}))?;
            Ok(value)
        })));
        Mock
    }
    fn moved_fixture(dir: &Path) -> (String, PathBuf, Rc<RefCell<Fake>>) {
        let old = dir.join("original");
        fs::create_dir(&old).unwrap();
        let c = fixture(&old);
        fs::create_dir_all(c.state_dir.join("computer")).unwrap();
        let name = name_for(&c).unwrap();
        atomic(&old.join("chuggin.json"), &c).unwrap();
        atomic(
            &c.state_dir.join("computer/ready.json"),
            &json!({"name":name}),
        )
        .unwrap();
        atomic(&c.state_dir.join("state.json"),&json!({"repo":old,"working_workspace":old,"cycle":88,"run_id":"keep-id","current_task":{"title":"Keep this activity"},"goal":""})).unwrap();
        let source = json!({"Id":"a".repeat(64),"Name":name,"Config":{"Labels":{"io.chuggin.managed":"true","io.chuggin.session":name}},"State":{"Running":true},"Filesystem":{"personal-document":"Retained files"},"Mounts":[{"Destination":"/opt/session-runtime","Source":c.state_dir.join("computer/tool-runtime"),"RW":false},{"Destination":"/run/public-web","Source":c.state_dir.join("computer/channel"),"RW":false}]});
        remember(&c, &name, Some(&source)).unwrap();
        let moved = dir.join("moved");
        fs::rename(old, &moved).unwrap();
        let fake = Rc::new(RefCell::new(Fake::default()));
        fake.borrow_mut().containers.insert(name.clone(), source);
        (name, moved, fake)
    }
    fn fixture(root: &Path) -> crate::runner::Config {
        serde_json::from_value(json!({"repo":root,"state_dir":root.join(".chuggin"),"activity_mode":"type2","execution":{"kind":"podman"},"model":"zen/unused","context_tokens":8192,"output_tokens":1024,"implementation_calls":4,"retry_seconds":0})).unwrap()
    }
    #[test]
    fn identity_survives_only_an_explicit_reconnect_and_recreated_paths_cannot_adopt_it() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_directory(&dir.path().join("global"));
        let root = dir.path().join("first");
        fs::create_dir(&root).unwrap();
        let c = fixture(&root);
        let name = name_for(&c).unwrap();
        remember(&c, &name, None).unwrap();
        assert_eq!(name_for(&c).unwrap(), name);
        let moved = dir.path().join("moved");
        fs::rename(&root, &moved).unwrap();
        let copy = fixture(&moved);
        assert!(name_for(&copy).is_err());
        fs::create_dir(&root).unwrap();
        assert!(name_for(&fixture(&root)).is_err());
    }
    #[test]
    fn legacy_discovery_requires_its_saved_history_and_writes_no_project_marker() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_directory(&dir.path().join("global"));
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        let c = fixture(&root);
        fs::create_dir_all(c.state_dir.join("computer")).unwrap();
        let name = name_for(&c).unwrap();
        atomic(
            &c.state_dir.join("computer/ready.json"),
            &json!({"name":name}),
        )
        .unwrap();
        let value = json!({"Id":"abcdef","Mounts":[{"Destination":"/opt/session-runtime","Source":c.state_dir.join("computer/tool-runtime")}]});
        let r = inferred_record(&value, &name);
        assert!(belongs(&r));
        save_record(&r).unwrap();
        assert!(!c.state_dir.join("computer/identity.json").exists());
        assert_eq!(name_for(&c).unwrap(), name);
        fs::remove_file(c.state_dir.join("computer/ready.json")).unwrap();
        assert!(!belongs(&r));
        assert!(name_for(&c).is_err());
    }
    #[test]
    fn registry_is_atomic_private_and_malformed_data_is_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_directory(dir.path());
        let r = Record {
            name: "chuggin-0123456789abcdef01234567".into(),
            project: None,
            state_dir: None,
            config: None,
            identity: None,
            container_id: None,
            snapshots: vec![],
            deleting: false,
        };
        save_record(&r).unwrap();
        assert_eq!(
            file(&dir.path().join("registry.json"), false)
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::write(dir.path().join("registry.json"), "broken").unwrap();
        assert!(read_registry().is_err());
        assert!(save_record(&r).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("registry.json")).unwrap(),
            "broken"
        );
    }
    #[test]
    fn rebase_keeps_unrelated_paths_and_preserves_state_details() {
        let mut state = json!({"repo":"/old/project","working_workspace":"/old/project","cycle":42,"current_task":{"title":"Keep this"},"other":"/old/project/history"});
        for key in ["repo", "working_workspace"] {
            rebase(
                &mut state,
                key,
                Some(Path::new("/old/project")),
                Path::new("/new/project"),
            );
        }
        assert_eq!(state["repo"], "/new/project");
        assert_eq!(state["working_workspace"], "/new/project");
        assert_eq!(state["cycle"], 42);
        assert_eq!(state["other"], "/old/project/history");
    }
    #[test]
    fn reconnect_preserves_installation_rebases_history_and_delete_cleans_only_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        let _mock = mocks(fake.clone());
        let unrelated = "chuggin-111111111111111111111111".to_string();
        fake.borrow_mut().containers.insert(unrelated.clone(),json!({"Id":"d".repeat(64),"Name":unrelated,"Config":{"Labels":{"io.chuggin.managed":"true","io.chuggin.session":unrelated}}}));
        assert!(
            scan()
                .unwrap()
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .orphaned
        );
        reconnect(&name, &moved).unwrap();
        assert_eq!(
            fake.borrow().containers[&name]["Filesystem"]["personal-document"],
            "Retained files"
        );
        assert_eq!(fake.borrow().containers[&name]["Id"], "b".repeat(64));
        let c = crate::runner::load(&moved.join("chuggin.json")).unwrap();
        assert_eq!(name_for(&c).unwrap(), name);
        assert!(!recovery_path(&name).unwrap().exists());
        let saved = read_optional(&c.state_dir.join("state.json"))
            .unwrap()
            .unwrap();
        assert_eq!(saved["repo"], json!(moved));
        assert_eq!(saved["working_workspace"], json!(moved));
        assert_eq!(saved["cycle"], 88);
        assert_eq!(saved["run_id"], "keep-id");
        let calls = fake.borrow().calls.clone();
        let created = calls.iter().position(|args| args[0] == "recreate").unwrap();
        let removed = calls
            .iter()
            .position(|args| args[0] == "rm" && args.last() == Some(&"a".repeat(64)))
            .unwrap();
        assert!(created < removed);
        delete(&name).unwrap();
        assert!(!fake.borrow().containers.contains_key(&name));
        assert!(fake.borrow().containers.contains_key(&unrelated));
        assert!(fake.borrow().images.is_empty());
        assert!(!c.state_dir.join("computer/ready.json").exists());
        assert!(c.state_dir.join("computer/reset-notice.json").exists());
        assert_eq!(
            read_optional(&c.state_dir.join("state.json"))
                .unwrap()
                .unwrap(),
            saved
        );
    }
    #[test]
    fn failed_replacement_retains_original_and_snapshot_then_retries_without_repeating_commit() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        fake.borrow_mut().fail_create = true;
        let _mock = mocks(fake.clone());
        assert!(reconnect(&name, &moved).is_err());
        let journal = recovery(&name).unwrap().unwrap();
        assert!(fake.borrow().containers.contains_key(&journal.backup_name));
        assert_eq!(
            fake.borrow().containers[&journal.backup_name]["Filesystem"]["personal-document"],
            "Retained files"
        );
        assert_eq!(fake.borrow().images.len(), 1);
        let old_state = read_optional(&moved.join(".chuggin/state.json"))
            .unwrap()
            .unwrap();
        assert_eq!(old_state["cycle"], 88);
        assert!(old_state["repo"].as_str().unwrap().ends_with("original"));
        fake.borrow_mut().fail_create = false;
        reconnect(&name, &moved).unwrap();
        assert_eq!(
            fake.borrow()
                .calls
                .iter()
                .filter(|args| args[0] == "commit")
                .count(),
            1
        );
        assert_eq!(
            fake.borrow().containers[&name]["Filesystem"]["personal-document"],
            "Retained files"
        );
    }
    #[test]
    fn connected_phase_retry_never_replaces_the_valid_new_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        fake.borrow_mut().fail_original_cleanup = true;
        let _mock = mocks(fake.clone());
        assert!(reconnect(&name, &moved).is_err());
        assert_eq!(recovery(&name).unwrap().unwrap().phase, "connected");
        fake.borrow_mut().containers.get_mut(&name).unwrap()["Filesystem"]["after-reconnect"] =
            json!("Latest changes");
        reconnect(&name, &moved).unwrap();
        assert_eq!(
            fake.borrow()
                .calls
                .iter()
                .filter(|args| args[0] == "recreate")
                .count(),
            1
        );
        assert_eq!(
            fake.borrow().containers[&name]["Filesystem"]["after-reconnect"],
            "Latest changes"
        );
        assert!(recovery(&name).unwrap().is_none());
    }
    #[test]
    fn old_controller_lock_blocks_deletion_and_reconnect_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        let _mock = mocks(fake.clone());
        let r = record(&name).unwrap();
        let _owner = old_locks(&r, None, None).unwrap();
        assert!(delete(&name).is_err());
        assert!(reconnect(&name, &moved).is_err());
        assert!(
            fake.borrow()
                .calls
                .iter()
                .all(|args| matches!(args[0].as_str(), "ps" | "inspect"))
        );
        assert!(fake.borrow().containers.contains_key(&name));
    }
    #[test]
    fn partial_snapshot_cleanup_blocks_resume_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        let _mock = mocks(fake.clone());
        reconnect(&name, &moved).unwrap();
        fake.borrow_mut().fail_image_cleanup = true;
        assert!(delete(&name).is_err());
        let c = crate::runner::load(&moved.join("chuggin.json")).unwrap();
        assert!(name_for(&c).is_err());
        assert!(
            scan()
                .unwrap()
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .orphaned
        );
        delete(&name).unwrap();
        assert!(fake.borrow().images.is_empty());
        assert!(!read_registry().unwrap().computers.contains_key(&name));
    }
    #[test]
    fn discovery_restores_a_lost_registry_from_existing_marker_without_writing_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let old = dir.path().join("project");
        fs::create_dir(&old).unwrap();
        let c = fixture(&old);
        fs::create_dir_all(c.state_dir.join("computer")).unwrap();
        let name = name_for(&c).unwrap();
        atomic(&old.join("chuggin.json"), &c).unwrap();
        atomic(
            &c.state_dir.join("computer/ready.json"),
            &json!({"name":name}),
        )
        .unwrap();
        let instance = json!({"Id":"a".repeat(64),"Name":name,"Config":{"Labels":{"io.chuggin.managed":"true","io.chuggin.session":name}},"State":{"Running":false},"Mounts":[{"Destination":"/opt/session-runtime","Source":c.state_dir.join("computer/tool-runtime")} ]});
        remember(&c, &name, Some(&instance)).unwrap();
        let before = fs::read(c.state_dir.join("computer/identity.json")).unwrap();
        fs::remove_file(directory().unwrap().join("registry.json")).unwrap();
        let fake = Rc::new(RefCell::new(Fake::default()));
        fake.borrow_mut().containers.insert(name.clone(), instance);
        let _mock = mocks(fake);
        assert!(!scan().unwrap()[0].orphaned);
        assert_eq!(name_for(&c).unwrap(), name);
        assert_eq!(
            fs::read(c.state_dir.join("computer/identity.json")).unwrap(),
            before
        );
    }
    #[test]
    fn replacement_id_is_never_accepted_by_normal_resume_or_overwritten_in_registry() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, _, fake) = moved_fixture(dir.path());
        let expected = record(&name).unwrap().container_id.unwrap();
        let mut changed = fake.borrow().containers[&name].clone();
        changed["Id"] = json!("e".repeat(64));
        assert!(verify_instance(&name, &changed).is_err());
        assert!(authorize_missing(&name).is_err());
        assert_eq!(record(&name).unwrap().container_id.unwrap(), expected);
    }
    #[test]
    fn interrupted_host_cleanup_keeps_ownership_until_notice_and_readiness_are_finished() {
        let dir = tempfile::tempdir().unwrap();
        let _directory = test_directory(&dir.path().join("global"));
        let (name, moved, fake) = moved_fixture(dir.path());
        let _mock = mocks(fake.clone());
        reconnect(&name, &moved).unwrap();
        let mut r = record(&name).unwrap();
        r.deleting = true;
        save_record(&r).unwrap();
        let state = r.state_dir.clone().unwrap();
        // Simulate a kill after ready.json is removed, while the ownership
        // marker is retained. A retry must still finish its model heads-up.
        fs::remove_file(state.join("computer/ready.json")).unwrap();
        assert!(belongs(&r));
        delete(&name).unwrap();
        assert!(state.join("computer/reset-notice.json").exists());
        assert!(!state.join("computer/identity.json").exists());
        assert!(state.join("state.json").exists());
    }
}
