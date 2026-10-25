//! Durable mail between the human and the autonomous conversation. Message
//! bodies enter model context only through read_messages; receipts mean fetched,
//! never understood, agreed, or available. Nothing here starts inference.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const BODY_LIMIT: usize = 8_000;
const MODEL_PAGE_BYTES: usize = 24_000;
const REPLY_CONTEXT_BYTES: usize = 6_000;
const RECENT_MODEL_MESSAGES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sender {
    Human,
    Model,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender: Sender,
    pub body: String,
    pub created_unix: u64,
    pub reply_to: Option<u64>,
    pub thread_id: u64,
    pub read_unix: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub revision: u64,
    pub latest_id: u64,
    pub unread_to_model: usize,
    pub unread_to_human: usize,
    pub last_human_read_unix: Option<u64>,
    pub last_human_message_id: Option<u64>,
    pub last_model_message_id: Option<u64>,
    pub oldest_unread_to_human_unix: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    /// Chronological within this page, even when paging backwards.
    pub messages: Vec<Message>,
    pub older_before: Option<u64>,
    pub has_more: bool,
    pub status: Status,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendResult {
    pub status: String,
    pub changed: bool,
    pub message: Message,
    pub mailbox: Status,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    serial: u64,
    revision: u64,
    unread_to_model: BTreeMap<u64, u64>,
    unread_to_human: BTreeMap<u64, u64>,
    read_unix: BTreeMap<u64, u64>,
    recent_model: VecDeque<u64>,
    last_human_read_unix: Option<u64>,
    last_human_message_id: Option<u64>,
    last_model_message_id: Option<u64>,
}

impl Store {
    fn status(&self) -> Status {
        Status {
            revision: self.revision,
            latest_id: self.serial,
            unread_to_model: self.unread_to_model.len(),
            unread_to_human: self.unread_to_human.len(),
            last_human_read_unix: self.last_human_read_unix,
            last_human_message_id: self.last_human_message_id,
            last_model_message_id: self.last_model_message_id,
            oldest_unread_to_human_unix: self.unread_to_human.values().copied().min(),
        }
    }
    fn changed(&mut self) -> Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .context("Mailbox revision exhausted")?;
        Ok(())
    }
    fn accept(&mut self, message: &Message) -> Result<()> {
        ensure!(
            message.id == self.serial + 1,
            "Nonsequential saved mailbox message"
        );
        self.serial = message.id;
        match message.sender {
            Sender::Human => {
                self.unread_to_model
                    .insert(message.id, message.created_unix);
                self.last_human_message_id = Some(message.id);
            }
            Sender::Model => {
                self.unread_to_human
                    .insert(message.id, message.created_unix);
                self.last_model_message_id = Some(message.id);
                self.recent_model.push_back(message.id);
                while self.recent_model.len() > RECENT_MODEL_MESSAGES {
                    self.recent_model.pop_front();
                }
            }
        }
        self.changed()
    }
}

#[derive(Serialize, Deserialize)]
struct SavedMessage {
    #[serde(flatten)]
    message: Message,
    /// Recent human input scopes initiative duplicates without pretending that
    /// an unsolicited message is a reply to that input.
    human_context: Option<u64>,
    operation_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct OperationIndex {
    message_id: u64,
    request_digest: String,
}

#[derive(Serialize, Deserialize)]
struct ReadPlan {
    request_digest: String,
    ids: Vec<u64>,
    has_more: bool,
    older_before: Option<u64>,
    next_after_id: Option<u64>,
    remaining_message_ids: Vec<u64>,
    snapshot_revision: u64,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn home(state: &Path) -> PathBuf {
    state.join("messenger")
}
fn message_path(root: &Path, id: u64) -> PathBuf {
    root.join("messages").join(format!("{id:020}.json"))
}
fn atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Mailbox path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    // Persist the rename as well as the message bytes before acknowledging it.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_saved(root: &Path, id: u64) -> Result<SavedMessage> {
    ensure!(id > 0, "Message IDs start at 1");
    let path = message_path(root, id);
    let file = File::open(&path).with_context(|| format!("No message with ID {id}"))?;
    ensure!(
        file.metadata()?.len() <= 64 * 1024,
        "Saved message {id} exceeds its storage limit"
    );
    let saved: SavedMessage =
        serde_json::from_reader(file).context("Could not read saved mailbox message")?;
    ensure!(
        saved.message.id == id,
        "Saved message ID does not match its filename"
    );
    Ok(saved)
}
fn message(root: &Path, store: &Store, id: u64) -> Result<Message> {
    ensure!(id <= store.serial, "No committed message with ID {id}");
    let mut message = read_saved(root, id)?.message;
    message.read_unix = store.read_unix.get(&id).copied();
    Ok(message)
}
fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn exact_key(body: &str, context: Option<u64>) -> String {
    digest(&format!(
        "{}\n{}",
        context.map(|v| v.to_string()).unwrap_or_default(),
        normalized(body)
    ))
}
fn index_path(root: &Path, key: &str) -> PathBuf {
    root.join("dedupe").join(format!("{key}.json"))
}
fn index_message(root: &Path, saved: &SavedMessage) -> Result<()> {
    if saved.message.sender == Sender::Model {
        atomic(
            &index_path(root, &exact_key(&saved.message.body, saved.human_context)),
            &saved.message.id,
        )?;
    }
    if let Some(key) = &saved.operation_hash {
        atomic(
            &root.join("operations").join(format!("{key}.json")),
            &OperationIndex {
                message_id: saved.message.id,
                request_digest: request_digest(
                    saved.message.sender,
                    &saved.message.body,
                    saved.message.reply_to,
                ),
            },
        )?;
    }
    Ok(())
}

fn with_store<T>(
    state: &Path,
    edit: impl FnOnce(&Path, &mut Store) -> Result<(T, bool)>,
) -> Result<T> {
    let root = home(state);
    fs::create_dir_all(&root)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(root.join("mailbox.lock"))?;
    lock.lock()?;
    let path = root.join("state.json");
    let mut store: Store = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Cannot read mailbox state")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(error) => return Err(error.into()),
    };
    let mut recovered = false;
    // The immutable message is committed first. If a process dies before the
    // metadata rename, recover that message instead of overwriting or losing it.
    while let Some(id) = store
        .serial
        .checked_add(1)
        .filter(|id| message_path(&root, *id).exists())
    {
        let saved = read_saved(&root, id)?;
        index_message(&root, &saved)?;
        store.accept(&saved.message)?;
        recovered = true;
    }
    if recovered {
        atomic(&path, &store)?;
    }
    let (result, changed) = edit(&root, &mut store)?;
    if changed {
        atomic(&path, &store)?;
    }
    Ok(result)
}

fn body(text: &str) -> Result<String> {
    let text = text.trim();
    ensure!(
        !text.is_empty() && text.len() <= BODY_LIMIT,
        "Message must contain 1–{BODY_LIMIT} bytes"
    );
    ensure!(
        !text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t')),
        "Message contains control characters"
    );
    Ok(text.into())
}
fn optional_id(args: &Value, field: &str) -> Result<Option<u64>> {
    match args.get(field).filter(|v| !v.is_null()) {
        Some(value) => {
            Ok(Some(value.as_u64().filter(|v| *v > 0).with_context(
                || format!("{field} must be a positive message ID"),
            )?))
        }
        None => Ok(None),
    }
}
fn page_limit(args: &Value, default: usize, max: usize) -> Result<usize> {
    args.get("limit")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_u64()
                .filter(|v| *v > 0 && *v <= max as u64)
                .map(|v| v as usize)
                .with_context(|| format!("limit must be 1–{max}"))
        })
        .unwrap_or(Ok(default))
}

/// Remove only obvious greeting/update wrappers. We deliberately do not use a
/// broad semantic/word-overlap rule: changed facts, numbers, names, negations,
/// questions, and uncertainty must remain deliverable.
fn near_identity(text: &str) -> String {
    let normalized = normalized(text);
    let mut text = normalized.as_str();
    for prefix in [
        "just a quick update: ",
        "quick update: ",
        "update: ",
        "fyi: ",
        "hello, ",
        "hi, ",
        "hey, ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }
    let trimmed = text.trim_end_matches(['.', '!']).trim();
    if trimmed.chars().last().is_some_and(char::is_alphabetic) {
        trimmed.to_owned()
    } else {
        text.to_owned()
    }
}
fn read_index(path: &Path) -> Result<Option<u64>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("Cannot read mailbox delivery index")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn request_digest(sender: Sender, body: &str, reply_to: Option<u64>) -> String {
    digest(&format!("{sender:?}\n{reply_to:?}\n{body}"))
}

fn send(
    state: &Path,
    sender: Sender,
    value: &str,
    reply_to: Option<u64>,
    operation: Option<&str>,
) -> Result<SendResult> {
    let body = body(value)?;
    let operation_hash = operation
        .map(|op| {
            ensure!(
                !op.is_empty() && op.len() <= 1000,
                "Delivery operation ID must contain 1–1000 bytes"
            );
            Ok::<_, anyhow::Error>(digest(&format!("{sender:?}\n{op}")))
        })
        .transpose()?;
    with_store(state, |root, store| {
        let parent = reply_to.map(|id| message(root, store, id)).transpose()?;
        let human_context = parent
            .as_ref()
            .filter(|m| m.sender == Sender::Human)
            .map(|m| m.id)
            .or(store.last_human_message_id);
        if let Some(key) = &operation_hash {
            let path = root.join("operations").join(format!("{key}.json"));
            if path.exists() {
                let index: OperationIndex = serde_json::from_slice(&fs::read(path)?)
                    .context("Cannot read mailbox delivery operation")?;
                ensure!(
                    index.request_digest == request_digest(sender, &body, reply_to),
                    "Delivery operation ID was already used for a different message"
                );
                let prior = message(root, store, index.message_id)?;
                return Ok((
                    SendResult {
                        status: "already_sent".into(),
                        changed: false,
                        message: prior,
                        mailbox: store.status(),
                    },
                    false,
                ));
            }
        }
        if sender == Sender::Model {
            let exact = read_index(&index_path(root, &exact_key(&body, human_context)))?;
            let mut duplicate = exact;
            if duplicate.is_none() {
                let core = near_identity(&body);
                if !core.is_empty() {
                    for id in store.recent_model.iter().rev() {
                        let saved = read_saved(root, *id)?;
                        if saved.human_context == human_context
                            && near_identity(&saved.message.body) == core
                        {
                            duplicate = Some(*id);
                            break;
                        }
                    }
                }
            }
            if let Some(id) = duplicate {
                let prior = message(root, store, id)?;
                if let Some(key) = &operation_hash {
                    atomic(
                        &root.join("operations").join(format!("{key}.json")),
                        &OperationIndex {
                            message_id: id,
                            request_digest: request_digest(sender, &body, reply_to),
                        },
                    )?;
                }
                return Ok((
                    SendResult {
                        status: "already_sent".into(),
                        changed: false,
                        message: prior,
                        mailbox: store.status(),
                    },
                    false,
                ));
            }
        }
        let id = store
            .serial
            .checked_add(1)
            .context("Mailbox message IDs exhausted")?;
        let message = Message {
            id,
            sender,
            body,
            created_unix: now()?,
            reply_to,
            thread_id: parent.map(|m| m.thread_id).unwrap_or(id),
            read_unix: None,
        };
        let saved = SavedMessage {
            message: message.clone(),
            human_context,
            operation_hash,
        };
        atomic(&message_path(root, id), &saved)?;
        index_message(root, &saved)?;
        store.accept(&message)?;
        Ok((
            SendResult {
                status: "sent".into(),
                changed: true,
                message,
                mailbox: store.status(),
            },
            true,
        ))
    })
}

pub fn status(state: &Path) -> Result<Status> {
    if !home(state).exists() {
        return Ok(Status::default());
    }
    with_store(state, |_, store| Ok((store.status(), false)))
}

#[cfg(test)]
pub fn human_send(state: &Path, body: &str, reply_to: Option<u64>) -> Result<SendResult> {
    send(state, Sender::Human, body, reply_to, None)
}

/// For a UI/RPC retry, retain the same caller-generated operation ID.
pub fn human_send_with_id(
    state: &Path,
    body: &str,
    reply_to: Option<u64>,
    operation: &str,
) -> Result<SendResult> {
    send(state, Sender::Human, body, reply_to, Some(operation))
}

pub fn model_send(state: &Path, args: &Value) -> Result<Value> {
    ensure!(args.is_object(), "send_message arguments must be an object");
    let body = args["body"]
        .as_str()
        .context("send_message.body must be a string")?;
    let reply_to = optional_id(args, "reply_to")?;
    let operation = args.get("_operation_id").and_then(Value::as_str);
    let result = send(state, Sender::Model, body, reply_to, operation)?;
    Ok(
        json!({"status":result.status,"changed":result.changed,"message_id":result.message.id,"created_unix":result.message.created_unix,"reply_to":result.message.reply_to,"thread_id":result.message.thread_id,"human_read_unix":result.message.read_unix,"mailbox":result.mailbox,"existing_body":if result.changed {None} else {Some(crate::project::excerpt(&result.message.body,600))}}),
    )
}

/// Reconstruct a tool result after a controller interruption only when the
/// mailbox already contains that operation's delivery or pinned fetch. Missing
/// operations are never executed here. A pinned fetch may finish its read
/// receipts, but keeps exactly its original batch and validates its arguments.
pub fn recover_model_tool(
    state: &Path,
    name: &str,
    args: &Value,
    operation: &str,
) -> Result<Option<Value>> {
    ensure!(
        matches!(name, "read_messages" | "send_message"),
        "Unknown messenger tool"
    );
    ensure!(args.is_object(), "Messenger arguments must be an object");
    ensure!(
        !operation.is_empty() && operation.len() <= 1000,
        "Messenger operation ID must contain 1–1000 bytes"
    );
    if !home(state).exists() {
        return Ok(None);
    }
    // Taking the store lock also reconciles a message committed just before its
    // operation index or metadata, so those successful sends are recoverable.
    let exists = with_store(state, |root, _| {
        let path = if name == "send_message" {
            root.join("operations").join(format!(
                "{}.json",
                digest(&format!("{:?}\n{operation}", Sender::Model))
            ))
        } else {
            root.join("reads")
                .join(format!("{}.json", digest(operation)))
        };
        let exists = match fs::metadata(path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        Ok((exists, false))
    })?;
    if !exists {
        return Ok(None);
    }
    // Indexes and read plans are append-only. Release the lock before entering
    // the normal idempotent dispatcher, which validates the saved request digest.
    let mut saved_args = args.clone();
    saved_args["_operation_id"] = json!(operation);
    match name {
        "send_message" => model_send(state, &saved_args).map(Some),
        "read_messages" => model_read(state, &saved_args).map(Some),
        _ => unreachable!(),
    }
}

pub fn human_page(state: &Path, before_id: Option<u64>, limit: usize) -> Result<Page> {
    ensure!(
        (1..=100).contains(&limit),
        "Message page size must be 1–100"
    );
    if !home(state).exists() {
        return Ok(Page {
            messages: Vec::new(),
            older_before: None,
            has_more: false,
            status: Status::default(),
        });
    }
    with_store(state, |root, store| {
        let top = before_id
            .unwrap_or(store.serial.saturating_add(1))
            .min(store.serial.saturating_add(1));
        ensure!(top > 0, "before_id must be a positive message ID");
        let first = top.saturating_sub(limit as u64).max(1);
        let mut messages = Vec::new();
        for id in first..top {
            messages.push(message(root, store, id)?);
        }
        let has_more = first > 1;
        let older_before = has_more.then(|| messages.first().map(|m| m.id)).flatten();
        Ok((
            Page {
                messages,
                older_before,
                has_more,
                status: store.status(),
            },
            false,
        ))
    })
}

fn mark_read(store: &mut Store, ids: &[u64], recipient: Sender) -> Result<Vec<u64>> {
    let at = now()?;
    let mut changed = Vec::new();
    for id in ids {
        let removed = match recipient {
            Sender::Human => store.unread_to_human.remove(id),
            Sender::Model => store.unread_to_model.remove(id),
        };
        if removed.is_some() {
            store.read_unix.insert(*id, at);
            changed.push(*id);
        }
    }
    if !changed.is_empty() {
        if recipient == Sender::Human {
            store.last_human_read_unix = Some(at);
        }
        store.changed()?;
    }
    Ok(changed)
}

pub fn mark_human_read(state: &Path, ids: &[u64]) -> Result<Status> {
    ensure!(
        ids.len() <= 100,
        "At most 100 visible messages may be marked read at once"
    );
    with_store(state, |root, store| {
        let mut incoming = Vec::new();
        for id in ids {
            if message(root, store, *id)?.sender == Sender::Model {
                incoming.push(*id);
            }
        }
        let changed = !mark_read(store, &incoming, Sender::Human)?.is_empty();
        Ok((store.status(), changed))
    })
}

pub fn model_read(state: &Path, args: &Value) -> Result<Value> {
    ensure!(
        args.is_object(),
        "read_messages arguments must be an object"
    );
    let limit = page_limit(args, 10, 50)?;
    let after = optional_id(args, "after_id")?;
    let before = optional_id(args, "before_id")?;
    let thread = optional_id(args, "thread_id")?;
    let exact = match args.get("message_ids").filter(|v| !v.is_null()) {
        Some(value) => {
            let values = value
                .as_array()
                .context("message_ids must be an array of message IDs")?;
            ensure!(
                !values.is_empty() && values.len() <= 50,
                "Supply 1–50 message IDs"
            );
            let mut ids = values
                .iter()
                .map(|v| {
                    v.as_u64()
                        .filter(|id| *id > 0)
                        .context("message_ids must contain positive IDs")
                })
                .collect::<Result<Vec<_>>>()?;
            ids.sort_unstable();
            ids.dedup();
            Some(ids)
        }
        None => None,
    };
    ensure!(
        exact.is_none() || (after.is_none() && before.is_none() && thread.is_none()),
        "message_ids cannot be combined with page/thread cursors"
    );
    ensure!(
        after.zip(before).is_none_or(|(a, b)| a < b),
        "after_id must precede before_id"
    );
    let operation = args.get("_operation_id").and_then(Value::as_str);
    if let Some(operation) = operation {
        ensure!(
            !operation.is_empty() && operation.len() <= 1000,
            "Read operation ID must contain 1–1000 bytes"
        );
    }
    let mut request = args.clone();
    if let Some(object) = request.as_object_mut() {
        object.remove("_operation_id");
    }
    let request_digest = digest(&request.to_string());
    with_store(state, |root, store| {
        let operation_path = operation.map(|operation| {
            root.join("reads")
                .join(format!("{}.json", digest(operation)))
        });
        if let Some(path) = operation_path.as_ref().filter(|path| path.exists()) {
            let plan: ReadPlan = serde_json::from_slice(&fs::read(path)?)
                .context("Cannot read saved mailbox fetch")?;
            ensure!(
                plan.request_digest == request_digest,
                "Read operation ID was already used for different arguments"
            );
            return deliver_read(root, store, plan, true);
        }
        if let Some(id) = thread {
            ensure!(id <= store.serial, "No message thread with ID {id}");
        }
        let unread = exact.is_none() && after.is_none() && before.is_none() && thread.is_none();
        let explicit_ids = exact.clone();
        let descending = !unread && exact.is_none() && after.is_none();
        let candidates: Box<dyn Iterator<Item = u64>> = if let Some(ids) = exact {
            Box::new(ids.into_iter())
        } else if unread {
            Box::new(
                store
                    .unread_to_model
                    .keys()
                    .copied()
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        } else {
            let start = after.unwrap_or(0).saturating_add(1);
            let end = before
                .unwrap_or(store.serial.saturating_add(1))
                .min(store.serial.saturating_add(1));
            if descending {
                Box::new((start..end).rev())
            } else {
                Box::new(start..end)
            }
        };
        let mut messages = Vec::new();
        let mut bytes = 0;
        let mut has_more = false;
        for id in candidates {
            let candidate = message(root, store, id)?;
            if thread.is_some_and(|thread| candidate.thread_id != thread) {
                continue;
            }
            if messages.len() >= limit
                || (!messages.is_empty()
                    && bytes + candidate.body.len() > MODEL_PAGE_BYTES - REPLY_CONTEXT_BYTES)
            {
                has_more = true;
                break;
            }
            bytes += candidate.body.len();
            messages.push(candidate);
        }
        if descending {
            messages.reverse();
        }
        let first = messages.first().map(|m| m.id);
        let last = messages.last().map(|m| m.id);
        let remaining_ids: Vec<_> = explicit_ids
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !messages.iter().any(|message| message.id == *id))
            .collect();
        let plan = ReadPlan {
            request_digest,
            ids: messages.iter().map(|message| message.id).collect(),
            has_more,
            older_before: if descending && has_more { first } else { None },
            next_after_id: if !descending && has_more { last } else { None },
            remaining_message_ids: remaining_ids,
            snapshot_revision: store.revision,
        };
        // Pin the fetch before marking it read. A retried interrupted tool call
        // can retrieve the same batch even when its read receipt was persisted.
        if let Some(path) = operation_path {
            atomic(&path, &plan)?;
        }
        deliver_read(root, store, plan, false)
    })
}

fn deliver_read(
    root: &Path,
    store: &mut Store,
    plan: ReadPlan,
    replayed: bool,
) -> Result<(Value, bool)> {
    let mut messages = plan
        .ids
        .iter()
        .map(|id| message(root, store, *id))
        .collect::<Result<Vec<_>>>()?;
    let mut context = Vec::new();
    let mut context_ids = Vec::new();
    let mut missing_context = Vec::new();
    let mut budget = REPLY_CONTEXT_BYTES;
    for incoming in messages
        .iter()
        .filter(|message| message.sender == Sender::Human)
    {
        let Some(parent_id) = incoming.reply_to else {
            continue;
        };
        if plan.ids.contains(&parent_id) || context_ids.contains(&parent_id) {
            continue;
        }
        if budget < 32 {
            break;
        }
        let Ok(mut parent) = message(root, store, parent_id) else {
            missing_context.push(parent_id);
            continue;
        };
        if parent.sender != Sender::Model {
            continue;
        }
        let limit = budget.min(2000);
        let truncated = parent.body.len() > limit;
        if truncated {
            parent.body = crate::project::excerpt(&parent.body, limit.saturating_sub(12));
        }
        budget = budget.saturating_sub(parent.body.len());
        context_ids.push(parent_id);
        let mut value = serde_json::to_value(parent)?;
        value["context_only"] = json!(true);
        value["body_truncated"] = json!(truncated);
        context.push(value);
    }
    let ids: Vec<_> = messages
        .iter()
        .filter(|message| message.sender == Sender::Human)
        .map(|message| message.id)
        .collect();
    let newly_read = mark_read(store, &ids, Sender::Model)?;
    for message in &mut messages {
        message.read_unix = store.read_unix.get(&message.id).copied();
    }
    Ok((
        json!({"status":if messages.is_empty(){"no_new_messages"}else{"messages"},"changed":!newly_read.is_empty(),"messages":messages,"reply_context":context,"missing_reply_context_ids":missing_context,"newly_read_ids":newly_read,"has_more":plan.has_more,"older_before":plan.older_before,"next_after_id":plan.next_after_id,"remaining_message_ids":plan.remaining_message_ids,"snapshot_revision":plan.snapshot_revision,"replayed_fetch":replayed,"receipt":"fetched","mailbox":store.status()}),
        !newly_read.is_empty(),
    ))
}

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read_messages","description":"Fetch messages from the human mailbox. With no arguments, returns only new unread human messages and marks those fetched. Read receipts mean retrieved, not agreement. Retrieve older context explicitly by message_ids, thread_id, after_id or before_id; follow page cursors when has_more. Mail is asynchronous; checking or sending does not pause the conversation.","parameters":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":50,"default":10},"after_id":{"type":"integer","minimum":1},"before_id":{"type":"integer","minimum":1},"thread_id":{"type":"integer","minimum":1},"message_ids":{"type":"array","items":{"type":"integer","minimum":1},"minItems":1,"maxItems":50}}}}}),
        json!({"type":"function","function":{"name":"send_message","description":"Send a message to the human's local inbox. You may initiate contact or reply using reply_to. Returns message_id, whether it was delivered or already_sent, human read receipt, and pending-mail counts. Duplicate messages are not sent again. New human input or changed facts can justify a new message. Sending is nonblocking; it never waits for a reply or stops the loop.","parameters":{"type":"object","properties":{"body":{"type":"string","maxLength":8000},"reply_to":{"type":"integer","minimum":1}},"required":["body"]}}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mail_survives_restart_receipts_are_separate_and_old_messages_are_retrievable() {
        let root = tempfile::tempdir().unwrap();
        let first = human_send(root.path(), "How is it going?", None).unwrap();
        let second =
            human_send(root.path(), "Anything interesting?", Some(first.message.id)).unwrap();
        assert_eq!(status(root.path()).unwrap().unread_to_model, 2);
        let page = model_read(root.path(), &json!({"limit":1})).unwrap();
        assert_eq!(page["messages"][0]["id"], first.message.id);
        assert_eq!(page["has_more"], true);
        assert_eq!(status(root.path()).unwrap().unread_to_model, 1);
        let sent = model_send(
            root.path(),
            &json!({"body":"I found an interesting pattern.","reply_to":first.message.id}),
        )
        .unwrap();
        let id = sent["message_id"].as_u64().unwrap();
        let human = human_page(root.path(), None, 10).unwrap();
        assert!(
            human
                .messages
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .read_unix
                .is_none()
        );
        assert_eq!(
            status(root.path()).unwrap().unread_to_human,
            1,
            "Merely listing must not mark a human receipt"
        );
        mark_human_read(root.path(), &[id]).unwrap();
        assert_eq!(status(root.path()).unwrap().unread_to_human, 0);
        let old = model_read(root.path(), &json!({"message_ids":[first.message.id]})).unwrap();
        assert_eq!(old["messages"][0]["body"], "How is it going?");
        assert_eq!(old["changed"], false);
        assert_eq!(status(root.path()).unwrap().unread_to_model, 1);
        let remaining = model_read(root.path(), &json!({})).unwrap();
        assert_eq!(remaining["messages"][0]["id"], second.message.id);
        assert!(
            model_read(root.path(), &json!({})).unwrap()["messages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn repeated_outbound_messages_do_not_create_notifications_but_new_facts_and_replies_do() {
        let root = tempfile::tempdir().unwrap();
        let first = model_send(root.path(), &json!({"body":"The result is 10 items."})).unwrap();
        let before = status(root.path()).unwrap();
        let duplicate =
            model_send(root.path(), &json!({"body":"  THE result  is 10 items. "})).unwrap();
        assert_eq!(duplicate["status"], "already_sent");
        assert_eq!(duplicate["message_id"], first["message_id"]);
        assert_eq!(status(root.path()).unwrap().revision, before.revision);
        assert_eq!(
            model_send(
                root.path(),
                &json!({"body":"Quick update: The result is 10 items!"})
            )
            .unwrap()["status"],
            "already_sent"
        );
        assert_eq!(
            model_send(root.path(), &json!({"body":"The result is 11 items."})).unwrap()["status"],
            "sent"
        );
        assert_eq!(
            model_send(root.path(), &json!({"body":"The result is not 10 items."})).unwrap()["status"],
            "sent"
        );
        assert_eq!(
            model_send(root.path(), &json!({"body":"The result is 10 items?"})).unwrap()["status"],
            "sent"
        );
        let human = human_send(root.path(), "Please repeat the first result.", None).unwrap();
        assert_eq!(
            model_send(
                root.path(),
                &json!({"body":"The result is 10 items.","reply_to":human.message.id})
            )
            .unwrap()["status"],
            "sent"
        );
    }
    #[test]
    fn concurrent_sends_have_stable_ids_and_duplicate_retries_are_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let path = root.path().to_owned();
                std::thread::spawn(move || {
                    model_send(&path, &json!({"body":"A single message"})).unwrap()
                })
            })
            .collect();
        let sent: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
        assert_eq!(sent.iter().filter(|r| r["changed"] == true).count(), 1);
        assert_eq!(status(root.path()).unwrap().latest_id, 1);
        let a = human_send_with_id(root.path(), "Hello", None, "request-1").unwrap();
        human_send(root.path(), "Another question", None).unwrap();
        let b = human_send_with_id(root.path(), "Hello", None, "request-1").unwrap();
        assert_eq!(a.message.id, b.message.id);
        assert!(!b.changed);
        assert!(human_send_with_id(root.path(), "A different body", None, "request-1").is_err());
    }
    #[test]
    fn orphan_message_recovery_never_overwrites_delivered_content() {
        let root = tempfile::tempdir().unwrap();
        human_send(root.path(), "First", None).unwrap();
        let orphan = SavedMessage {
            message: Message {
                id: 2,
                sender: Sender::Model,
                body: "Survived interruption".into(),
                created_unix: now().unwrap(),
                reply_to: None,
                thread_id: 2,
                read_unix: None,
            },
            human_context: Some(1),
            operation_hash: Some(digest("Model\norphan-send")),
        };
        atomic(&message_path(&home(root.path()), 2), &orphan).unwrap();
        let recovered = recover_model_tool(
            root.path(),
            "send_message",
            &json!({"body":"Survived interruption"}),
            "orphan-send",
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered["message_id"], 2);
        assert_eq!(recovered["changed"], false);
        assert_eq!(status(root.path()).unwrap().latest_id, 2);
        let duplicate = model_send(root.path(), &json!({"body":"Survived interruption"})).unwrap();
        assert_eq!(duplicate["status"], "already_sent");
        assert_eq!(duplicate["message_id"], 2);
        assert_eq!(
            human_send(root.path(), "Third", None).unwrap().message.id,
            3
        );
    }
    #[test]
    fn backwards_pages_are_chronological_and_mark_only_visible_incoming_mail() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..8 {
            model_send(root.path(), &json!({"body":format!("Result number {n}")})).unwrap();
        }
        let page = human_page(root.path(), None, 3).unwrap();
        assert_eq!(
            page.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            [6, 7, 8]
        );
        assert_eq!(page.older_before, Some(6));
        mark_human_read(root.path(), &[6, 7, 8]).unwrap();
        assert_eq!(status(root.path()).unwrap().unread_to_human, 5);
        let old = human_page(root.path(), page.older_before, 3).unwrap();
        assert_eq!(
            old.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert!(old.messages.iter().all(|m| m.read_unix.is_none()));
    }

    #[test]
    fn reply_context_and_pinned_fetches_survive_retries_without_consuming_new_mail() {
        let root = tempfile::tempdir().unwrap();
        let question = model_send(
            root.path(),
            &json!({"body":"Would you like to see the image?"}),
        )
        .unwrap()["message_id"]
            .as_u64()
            .unwrap();
        let answer = human_send(root.path(), "Yes", Some(question)).unwrap();
        let fetched = model_read(root.path(), &json!({"_operation_id":"read-1"})).unwrap();
        assert_eq!(fetched["messages"][0]["body"], "Yes");
        assert_eq!(fetched["reply_context"][0]["id"], question);
        assert_eq!(
            fetched["reply_context"][0]["body"],
            "Would you like to see the image?"
        );
        assert_eq!(
            status(root.path()).unwrap().unread_to_human,
            1,
            "A reply is not a fabricated human read receipt"
        );
        human_send(root.path(), "A separate question", None).unwrap();
        let retried = model_read(root.path(), &json!({"_operation_id":"read-1"})).unwrap();
        assert_eq!(retried["messages"][0]["id"], answer.message.id);
        assert_eq!(retried["replayed_fetch"], true);
        assert_eq!(retried["changed"], false);
        assert_eq!(status(root.path()).unwrap().unread_to_model, 1);
        assert!(model_read(root.path(), &json!({"_operation_id":"read-1","limit":1})).is_err());
        assert_eq!(
            model_read(root.path(), &json!({"_operation_id":"read-2"})).unwrap()["messages"][0]["body"],
            "A separate question"
        );
    }

    #[test]
    fn near_duplicate_delivery_operation_is_still_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let first = model_send(root.path(), &json!({"body":"The result is 10 items."})).unwrap();
        let args =
            json!({"body":"Quick update: The result is 10 items!","_operation_id":"model-send-1"});
        let duplicate = model_send(root.path(), &args).unwrap();
        assert_eq!(duplicate["message_id"], first["message_id"]);
        human_send(root.path(), "New input after the interrupted send", None).unwrap();
        let retry = model_send(root.path(), &args).unwrap();
        assert_eq!(retry["message_id"], first["message_id"]);
        assert_eq!(retry["changed"], false);
        assert!(
            model_send(
                root.path(),
                &json!({"body":"A different message","_operation_id":"model-send-1"})
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_does_not_execute_missing_operations_or_consume_mail() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            recover_model_tool(root.path(), "read_messages", &json!({}), "absent-read")
                .unwrap()
                .is_none()
        );
        assert!(
            recover_model_tool(
                root.path(),
                "send_message",
                &json!({"body":"Do not send"}),
                "absent-send"
            )
            .unwrap()
            .is_none()
        );
        assert!(!home(root.path()).exists());
        human_send(root.path(), "Keep this unread", None).unwrap();
        assert!(
            recover_model_tool(root.path(), "read_messages", &json!({}), "absent-read")
                .unwrap()
                .is_none()
        );
        assert!(
            recover_model_tool(
                root.path(),
                "send_message",
                &json!({"body":"Do not send"}),
                "absent-send"
            )
            .unwrap()
            .is_none()
        );
        let status = status(root.path()).unwrap();
        assert_eq!(status.latest_id, 1);
        assert_eq!(status.unread_to_model, 1);
        assert_eq!(status.unread_to_human, 0);
    }

    #[test]
    fn recovery_replays_saved_fetch_and_delivery_and_validates_original_arguments() {
        let root = tempfile::tempdir().unwrap();
        let first = human_send(root.path(), "First question", None).unwrap();
        model_read(
            root.path(),
            &json!({"_operation_id":"saved-read","limit":1}),
        )
        .unwrap();
        human_send(root.path(), "Later question", None).unwrap();
        let recovered = recover_model_tool(
            root.path(),
            "read_messages",
            &json!({"limit":1}),
            "saved-read",
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered["messages"][0]["id"], first.message.id);
        assert_eq!(recovered["replayed_fetch"], true);
        assert_eq!(status(root.path()).unwrap().unread_to_model, 1);
        assert!(
            recover_model_tool(
                root.path(),
                "read_messages",
                &json!({"limit":2}),
                "saved-read"
            )
            .is_err()
        );
        let sent = model_send(
            root.path(),
            &json!({"body":"A saved answer","_operation_id":"saved-send"}),
        )
        .unwrap();
        human_send(root.path(), "New context after delivery", None).unwrap();
        let recovered = recover_model_tool(
            root.path(),
            "send_message",
            &json!({"body":"A saved answer"}),
            "saved-send",
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered["message_id"], sent["message_id"]);
        assert_eq!(recovered["changed"], false);
        assert!(
            recover_model_tool(
                root.path(),
                "send_message",
                &json!({"body":"A changed answer"}),
                "saved-send"
            )
            .is_err()
        );
        assert_eq!(status(root.path()).unwrap().unread_to_human, 1);
    }

    #[test]
    fn recovery_finishes_pinned_fetch_receipts_without_adding_later_messages() {
        let root = tempfile::tempdir().unwrap();
        let pinned = human_send(root.path(), "Pinned before interruption", None).unwrap();
        // Simulate interruption after persisting the read plan but before the
        // fetch's receipt/state save. No model_read result has been returned.
        with_store(root.path(), |dir, store| {
            atomic(
                &dir.join("reads")
                    .join(format!("{}.json", digest("interrupted-read"))),
                &ReadPlan {
                    request_digest: digest(&json!({}).to_string()),
                    ids: vec![pinned.message.id],
                    has_more: false,
                    older_before: None,
                    next_after_id: None,
                    remaining_message_ids: Vec::new(),
                    snapshot_revision: store.revision,
                },
            )?;
            Ok(((), false))
        })
        .unwrap();
        let later = human_send(root.path(), "Arrived after the saved plan", None).unwrap();
        let recovered =
            recover_model_tool(root.path(), "read_messages", &json!({}), "interrupted-read")
                .unwrap()
                .unwrap();
        assert_eq!(recovered["messages"].as_array().unwrap().len(), 1);
        assert_eq!(recovered["messages"][0]["id"], pinned.message.id);
        assert_eq!(recovered["changed"], true);
        assert_eq!(recovered["replayed_fetch"], true);
        assert_eq!(status(root.path()).unwrap().unread_to_model, 1);
        assert_eq!(
            model_read(root.path(), &json!({})).unwrap()["messages"][0]["id"],
            later.message.id
        );
    }

    #[test]
    fn fetched_text_is_bounded_and_only_displayed_model_ids_get_human_receipts() {
        let root = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for n in 0..5 {
            ids.push(
                human_send(root.path(), &format!("{n}{}", "x".repeat(7900)), None)
                    .unwrap()
                    .message
                    .id,
            );
        }
        let page = model_read(root.path(), &json!({"message_ids":ids})).unwrap();
        let text_bytes: usize = page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap().len())
            .sum();
        assert!(text_bytes <= MODEL_PAGE_BYTES - REPLY_CONTEXT_BYTES);
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
        assert_eq!(page["remaining_message_ids"].as_array().unwrap().len(), 3);
        let sent = model_send(root.path(), &json!({"body":"Reply"})).unwrap()["message_id"]
            .as_u64()
            .unwrap();
        mark_human_read(root.path(), &[ids[0], sent]).unwrap();
        assert_eq!(status(root.path()).unwrap().unread_to_model, 3);
        assert_eq!(status(root.path()).unwrap().unread_to_human, 0);
    }
}
