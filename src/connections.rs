//! Saved inference endpoints. Stable numeric identities keep projects attached to
//! the same connection after a rename; credentials never enter the registry.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const LIMIT: u64 = 8 * 1024 * 1024;
const PREFIX: &str = "connection/";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Ollama,
    #[serde(rename = "openai_compatible")]
    OpenAICompatible,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::OpenAICompatible => "OpenAI compatible",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Connection {
    pub id: u64,
    pub name: String,
    pub kind: Kind,
    pub base_url: String,
}
impl Connection {
    pub fn endpoint(&self) -> String {
        format!(
            "{}{}",
            self.base_url,
            match self.kind {
                Kind::Ollama => "/api/chat",
                Kind::OpenAICompatible => "/chat/completions",
            }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub connection: Connection,
    pub model_id: String,
}

#[derive(Clone, Debug)]
pub struct CatalogModel {
    /// Raw provider model identity, without Chuggin's connection prefix.
    pub id: String,
    pub name: String,
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Registry {
    next_id: u64,
    connections: Vec<Connection>,
}
struct Store {
    directory: PathBuf,
    _lock: File,
    registry: Registry,
}

fn directory() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(directory) = TEST_DIRECTORY.with(|value| value.borrow().clone()) {
        return Ok(directory);
    }
    Ok(crate::setup::settings_path()?.with_file_name("connections"))
}

fn protected_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
        "Saved connection storage must use ordinary files owned by this user"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
impl Store {
    fn open() -> Result<Self> {
        let directory = directory()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
            "Saved connection storage must be an ordinary directory owned by this user"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let lock = protected_file(&directory.join("registry.lock"), true)?;
        lock.lock()
            .context("Cannot lock saved connection storage")?;
        let registry: Registry = match protected_file(&directory.join("registry.json"), false) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= LIMIT,
                    "Saved connection registry is too large"
                );
                serde_json::from_reader(file.take(LIMIT))
                    .map_err(|_| anyhow::anyhow!("Saved connection registry is invalid"))?
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Registry::default()
            }
            Err(error) => return Err(error.context("Cannot read saved connection registry")),
        };
        let mut seen = std::collections::HashSet::new();
        for connection in &registry.connections {
            ensure!(
                connection.id > 0 && seen.insert(connection.id),
                "Saved connection registry has invalid identities"
            );
            validate_name(&connection.name)?;
            ensure!(
                normalize_url(&connection.base_url, connection.kind)? == connection.base_url,
                "Saved connection URL needs to be updated in Global settings"
            );
        }
        Ok(Self {
            directory,
            _lock: lock,
            registry,
        })
    }
    fn save(&self) -> Result<()> {
        atomic_write(
            &self.directory,
            "registry.json",
            &serde_json::to_vec_pretty(&self.registry)?,
        )
    }
    fn get(&self, id: u64) -> Result<&Connection> {
        self.registry.connections.iter().find(|connection| connection.id == id)
            .with_context(|| format!("Saved connection {id} is missing or was deleted. Choose an available provider in settings."))
    }
    fn key(&self, id: u64) -> Result<Option<String>> {
        self.get(id)?;
        match protected_file(&self.directory.join(format!("{id}.key")), false) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= 16_384,
                    "Saved connection credential is too large"
                );
                let mut key = String::new();
                file.take(16_384)
                    .read_to_string(&mut key)
                    .map_err(|_| anyhow::anyhow!("Cannot read saved connection credential"))?;
                let key = key.trim();
                ensure!(
                    !key.chars().any(char::is_control),
                    "Saved connection credential is invalid"
                );
                Ok((!key.is_empty()).then(|| key.to_owned()))
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(_) => anyhow::bail!("Cannot read protected saved connection credential"),
        }
    }
}
fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join(name))
        .map_err(|_| anyhow::anyhow!("Cannot atomically save connection storage"))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.trim() == name
            && !name.chars().any(char::is_control),
        "Connection name must contain 1–128 characters without surrounding whitespace or control characters"
    );
    Ok(())
}
pub fn normalize_url(input: &str, kind: Kind) -> Result<String> {
    let mut url = reqwest::Url::parse(input.trim())
        .map_err(|_| anyhow::anyhow!("Enter a valid HTTP(S) base URL for this connection"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Connection URL must use HTTP(S), without embedded credentials, query parameters, or a fragment"
    );
    let mut path = url.path().trim_end_matches('/').to_owned();
    for suffix in match kind {
        Kind::Ollama => &["/api/chat", "/api/tags", "/api/show"][..],
        Kind::OpenAICompatible => &["/chat/completions", "/models"][..],
    } {
        if let Some(base) = path.strip_suffix(suffix) {
            path = base.to_owned();
            break;
        }
    }
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn list() -> Result<Vec<Connection>> {
    Ok(Store::open()?.registry.connections)
}
pub fn get(id: u64) -> Result<Connection> {
    Ok(Store::open()?.get(id)?.clone())
}
pub fn has_key(id: u64) -> Result<bool> {
    Ok(Store::open()?.key(id)?.is_some())
}
/// `None` preserves an existing key; an empty provided key removes it.
pub fn save(
    id: Option<u64>,
    name: &str,
    kind: Kind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Connection> {
    let name = name.trim();
    validate_name(name)?;
    let base_url = normalize_url(base_url, kind)?;
    if let Some(key) = api_key {
        ensure!(
            key.len() <= 16_384 && !key.trim().chars().any(char::is_control),
            "Connection API key must be a single line of at most 16384 bytes"
        );
    }
    let mut store = Store::open()?;
    let id = if let Some(id) = id {
        store.get(id)?;
        id
    } else {
        let highest = store
            .registry
            .connections
            .iter()
            .map(|connection| connection.id)
            .max()
            .unwrap_or(0);
        let previous = store.registry.next_id.max(highest);
        let previous = if previous == 0 {
            let mut seed = [0; 8];
            getrandom::fill(&mut seed)
                .map_err(|_| anyhow::anyhow!("Cannot generate a saved connection identity"))?;
            // Keep numeric identities exactly representable by common JSON readers.
            (u64::from_ne_bytes(seed) & ((1u64 << 52) - 1)).max(1)
        } else {
            previous
        };
        store.registry.next_id = previous
            .checked_add(1)
            .context("Saved connection identities are exhausted")?;
        store.registry.next_id
    };
    ensure!(
        !store
            .registry
            .connections
            .iter()
            .any(|connection| connection.id != id && connection.name.eq_ignore_ascii_case(name)),
        "Another saved connection already uses this name"
    );
    let connection = Connection {
        id,
        name: name.to_owned(),
        kind,
        base_url,
    };
    if let Some(key) = api_key {
        if key.trim().is_empty() {
            match fs::remove_file(store.directory.join(format!("{id}.key"))) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => anyhow::bail!("Cannot remove saved connection credential"),
            }
        } else {
            atomic_write(
                &store.directory,
                &format!("{id}.key"),
                key.trim().as_bytes(),
            )?;
        }
    }
    if let Some(existing) = store
        .registry
        .connections
        .iter_mut()
        .find(|connection| connection.id == id)
    {
        *existing = connection.clone();
    } else {
        store.registry.connections.push(connection.clone());
    }
    store.save()?;
    Ok(connection)
}
pub fn delete(id: u64) -> Result<()> {
    let mut store = Store::open()?;
    store.get(id)?;
    store
        .registry
        .connections
        .retain(|connection| connection.id != id);
    store.save()?;
    match fs::remove_file(store.directory.join(format!("{id}.key"))) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            anyhow::bail!("Connection removed, but its protected credential could not be removed")
        }
    }
    Ok(())
}
pub fn route(id: u64, model: &str) -> String {
    format!("{PREFIX}{id}/{model}")
}
/// Parse without touching storage. Raw model IDs may themselves contain slashes.
pub fn route_parts(model: &str) -> Result<Option<(u64, &str)>> {
    let Some(rest) = model.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let (id, raw) = rest
        .split_once('/')
        .context("Saved connection model route is invalid")?;
    let id: u64 = id
        .parse()
        .context("Saved connection model identity is invalid")?;
    ensure!(
        id > 0
            && !raw.trim().is_empty()
            && raw.trim() == raw
            && !raw.chars().any(char::is_whitespace)
            && !raw.chars().any(char::is_control),
        "Saved connection model identity is invalid"
    );
    Ok(Some((id, raw)))
}
pub fn resolve(model: &str) -> Result<Option<Resolved>> {
    route_parts(model)?
        .map(|(id, model)| {
            Ok(Resolved {
                connection: get(id)?,
                model_id: model.to_owned(),
            })
        })
        .transpose()
}
pub fn label(model: &str) -> Result<Option<String>> {
    Ok(resolve(model)?.map(|resolved| resolved.connection.name))
}
pub(crate) fn key(id: u64) -> Result<Option<String>> {
    Store::open()?.key(id)
}

fn fetch(connection: &Connection, suffix: &str, body: Option<Value>) -> Result<Value> {
    let client = crate::groq::client()?;
    let url = format!("{}{suffix}", connection.base_url);
    let request = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    let request = if let Some(key) = key(connection.id)? {
        request.bearer_auth(key)
    } else {
        request
    };
    let response = request
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(|_| anyhow::anyhow!("Saved connection metadata endpoint could not be reached"))?;
    ensure!(
        response.status().is_success(),
        "Saved connection metadata returned HTTP {}",
        response.status()
    );
    let mut bytes = Vec::new();
    response
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Saved connection metadata response was interrupted"))?;
    ensure!(
        bytes.len() as u64 <= LIMIT,
        "Saved connection metadata response is too large"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Saved connection metadata is invalid"))
}
fn numeric(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}
pub fn catalog(id: u64) -> Result<Vec<CatalogModel>> {
    let connection = get(id)?;
    let native = connection.kind == Kind::Ollama;
    let value = fetch(
        &connection,
        if native { "/api/tags" } else { "/models" },
        None,
    )?;
    let entries = value[if native { "models" } else { "data" }]
        .as_array()
        .context("Saved connection model catalog has no model list")?;
    let mut models: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let id = entry[if native { "name" } else { "id" }].as_str()?;
            if id.trim().is_empty()
                || id.chars().any(char::is_whitespace)
                || id.chars().any(char::is_control)
            {
                return None;
            }
            Some(CatalogModel {
                id: id.into(),
                name: entry["name"].as_str().unwrap_or(id).into(),
                context_tokens: numeric(&entry["context_length"])
                    .or_else(|| numeric(&entry["context_window"])),
                output_tokens: numeric(&entry["max_output_tokens"]),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}
/// Return advertised model metadata, using the connection's native protocol.
pub fn metadata(name: &str) -> Result<Option<Value>> {
    let Some(resolved) = resolve(name)? else {
        return Ok(None);
    };
    let value = if resolved.connection.kind == Kind::Ollama {
        fetch(
            &resolved.connection,
            "/api/show",
            Some(json!({"model":resolved.model_id})),
        )?
    } else {
        let value = fetch(&resolved.connection, "/models", None)?;
        value["data"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["id"] == resolved.model_id)
            })
            .cloned()
            .context("Selected model is missing from this connection's metadata")?
    };
    Ok(Some(value))
}

#[cfg(test)]
thread_local! { static TEST_DIRECTORY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) struct TestDirectory(Option<PathBuf>);
#[cfg(test)]
impl Drop for TestDirectory {
    fn drop(&mut self) {
        TEST_DIRECTORY.with(|value| *value.borrow_mut() = self.0.take());
    }
}
#[cfg(test)]
pub(crate) fn test_directory(path: &Path) -> TestDirectory {
    TestDirectory(TEST_DIRECTORY.with(|value| value.borrow_mut().replace(path.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_survive_renames_and_deletions_never_reuse_them() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = test_directory(temp.path());
        let a = save(
            None,
            "First",
            Kind::Ollama,
            "http://localhost:11434/",
            Some("fixture-private-key"),
        )
        .unwrap();
        let model = route(a.id, "org/model:latest");
        assert_eq!(
            resolve(&model).unwrap().unwrap().model_id,
            "org/model:latest"
        );
        let renamed = save(Some(a.id), "Renamed", a.kind, &a.base_url, None).unwrap();
        assert_eq!(resolve(&model).unwrap().unwrap().connection, renamed);
        assert!(has_key(a.id).unwrap());
        let registry = fs::read_to_string(temp.path().join("registry.json")).unwrap();
        assert!(!registry.contains("fixture-private-key"));
        assert!(!format!("{a:?}").contains("fixture-private-key"));
        assert_eq!(
            fs::metadata(temp.path().join(format!("{}.key", a.id)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        delete(a.id).unwrap();
        assert!(
            resolve(&model)
                .unwrap_err()
                .to_string()
                .contains("missing or was deleted")
        );
        assert!(!temp.path().join(format!("{}.key", a.id)).exists());
        let b = save(
            None,
            "Next",
            Kind::OpenAICompatible,
            "http://localhost:9999/v1/chat/completions",
            None,
        )
        .unwrap();
        assert!(b.id > a.id);
        assert_eq!(b.base_url, "http://localhost:9999/v1");
        assert_eq!(b.endpoint(), "http://localhost:9999/v1/chat/completions");
    }
    #[test]
    fn urls_reject_secrets_and_unsupported_schemes_and_preserve_base_paths() {
        for url in [
            "file:///tmp/model",
            "http://user:secret@example.com/v1",
            "https://example.com/?key=secret",
            "https://example.com/#key",
        ] {
            let error = normalize_url(url, Kind::OpenAICompatible)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
        }
        assert_eq!(
            normalize_url("https://example.com/proxy/v1/", Kind::OpenAICompatible).unwrap(),
            "https://example.com/proxy/v1"
        );
        assert_eq!(
            normalize_url("http://localhost:11434/proxy/api/chat", Kind::Ollama).unwrap(),
            "http://localhost:11434/proxy"
        );
        assert!(route_parts("connection/nope/a").is_err());
        assert!(route_parts("connection/1/").is_err());
        assert!(resolve("legacy-model:latest").unwrap().is_none());
    }
    #[test]
    fn malformed_registry_never_becomes_an_empty_registry() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = test_directory(temp.path());
        fs::write(temp.path().join("registry.json"), b"broken").unwrap();
        assert!(list().unwrap_err().to_string().contains("invalid"));
        assert!(save(None, "New", Kind::Ollama, "http://localhost:11434", None).is_err());
    }
    #[test]
    fn concurrent_saves_are_atomic_and_preserve_each_connection_and_key() {
        let temp = tempfile::tempdir().unwrap();
        let mut workers = Vec::new();
        for index in 0..12 {
            let path = temp.path().to_owned();
            workers.push(std::thread::spawn(move || {
                let _guard = test_directory(&path);
                save(
                    None,
                    &format!("Concurrent {index}"),
                    Kind::Ollama,
                    "http://localhost:11434",
                    Some(&format!("private-key-{index}")),
                )
                .unwrap()
            }));
        }
        let saved: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let _guard = test_directory(temp.path());
        let all = list().unwrap();
        assert_eq!(all.len(), saved.len());
        let identities: std::collections::HashSet<_> =
            all.iter().map(|connection| connection.id).collect();
        assert_eq!(identities.len(), saved.len());
        for connection in &saved {
            assert_eq!(&get(connection.id).unwrap(), connection);
            assert!(has_key(connection.id).unwrap());
        }
        let bytes = fs::read_to_string(temp.path().join("registry.json")).unwrap();
        assert!(!bytes.contains("private-key"));
    }
    #[test]
    fn losing_the_registry_is_an_explicit_missing_connection() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = test_directory(temp.path());
        let first = save(None, "First", Kind::Ollama, "http://localhost:11434", None).unwrap();
        let model = route(first.id, "model");
        fs::remove_file(temp.path().join("registry.json")).unwrap();
        assert!(
            resolve(&model)
                .unwrap_err()
                .to_string()
                .contains("missing or was deleted")
        );
        let next = save(
            None,
            "New registry",
            Kind::Ollama,
            "http://localhost:9999",
            None,
        )
        .unwrap();
        assert_ne!(next.id, first.id);
        assert!(resolve(&model).is_err());
    }
    #[test]
    fn compatible_metadata_uses_models_and_only_advertised_portable_overrides() {
        use std::io::{BufRead, BufReader, Write};
        let temp = tempfile::tempdir().unwrap();
        let _guard = test_directory(temp.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let connection = save(
            None,
            "Metadata",
            Kind::OpenAICompatible,
            &format!("http://{}/proxy/v1", listener.local_addr().unwrap()),
            None,
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with("GET /proxy/v1/models "));
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                }
                let body=json!({"data":[{"id":"org/model","context_length":32768,"max_output_tokens":4096,"supported_parameters":["temperature","top_p","top_k"],"supported_reasoning_efforts":["low","high"]}]}).to_string();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        assert!(!has_key(connection.id).unwrap());
        let entries = catalog(connection.id).unwrap();
        assert_eq!(entries[0].context_tokens, Some(32768));
        assert_eq!(entries[0].output_tokens, Some(4096));
        let name = route(connection.id, "org/model");
        let reasoning = crate::reasoning::discover("http://legacy-unused", &name).unwrap();
        let sampling = crate::sampling::discover("http://legacy-unused", &name).unwrap();
        assert!(reasoning.supports(&crate::reasoning::Setting::Effort("high".into())));
        assert!(!reasoning.supports(&crate::reasoning::Setting::On));
        assert!(!sampling.supports_parameter("top_k"));
        let mut body = json!({"model":"org/model"});
        crate::reasoning::apply(
            &mut body,
            &name,
            &crate::reasoning::Setting::Effort("high".into()),
            Some(&reasoning),
        )
        .unwrap();
        crate::sampling::apply(
            &mut body,
            &name,
            &crate::sampling::Settings::Focused,
            Some(&sampling),
        )
        .unwrap();
        assert_eq!(
            body,
            json!({"model":"org/model","reasoning_effort":"high","temperature":0.4})
        );
        let unknown = crate::reasoning::compatible_capabilities(&json!({}));
        assert!(unknown.supports(&crate::reasoning::Setting::Default));
        assert!(!unknown.supports(&crate::reasoning::Setting::On));
        let unknown = crate::sampling::compatible_capabilities(&json!({}));
        assert!(unknown.supports(&crate::sampling::Settings::ProviderDefault));
        assert!(!unknown.supports(&crate::sampling::Settings::Focused));
        server.join().unwrap();
    }
}
