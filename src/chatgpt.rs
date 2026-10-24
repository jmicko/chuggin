//! Official Sign in with ChatGPT for OSS clients. Credentials stay in protected
//! global storage; project configurations contain only a `chatgpt/` model name.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const CALLBACK_PATH: &str = "/auth/callback";
const STORE_LIMIT: u64 = 8 * 1024 * 1024;

/// Displayable registration details, never OAuth credentials or subject claims.
#[derive(Clone, Debug, Serialize)]
pub struct SafeAccount {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub active: bool,
    pub connected: bool,
    pub plan_enabled: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    host_id: String,
    active_account: Option<String>,
    accounts: Vec<Account>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Account {
    id: String,
    label: String,
    email: Option<String>,
    issuer: String,
    subject: String,
    client_id: String,
    credentials: Option<Credentials>,
    #[serde(default)]
    pending_credentials: Option<PendingCredentials>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingCredentials {
    credentials: Credentials,
    validate_id_token: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    scopes: Vec<String>,
    expires_at: i64,
    earliest_refresh_at: Option<i64>,
    saved_at: String,
}

struct LockedStore {
    directory: PathBuf,
    _lock: File,
    store: Store,
}

fn protected_directory(directory: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "ChatGPT credential directory must be an ordinary directory owned by this user"
    );
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    Ok(())
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
        "ChatGPT credential files must be ordinary files owned by this user"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

impl LockedStore {
    fn open(directory: PathBuf) -> Result<Self> {
        protected_directory(&directory)?;
        let lock = protected_file(&directory.join("accounts.lock"), true)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            ensure!(
                error
                    .raw_os_error()
                    .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN),
                "Cannot lock ChatGPT account storage"
            );
            ensure!(
                Instant::now() < deadline,
                "ChatGPT account storage is busy; try again"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let path = directory.join("accounts.json");
        let store = match protected_file(&path, false) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= STORE_LIMIT,
                    "ChatGPT account storage is too large"
                );
                serde_json::from_reader(file.take(STORE_LIMIT))
                    .map_err(|_| anyhow::anyhow!("ChatGPT account storage is invalid"))?
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Store::default()
            }
            Err(error) => {
                return Err(error.context("Cannot read protected ChatGPT account storage"));
            }
        };
        Ok(Self {
            directory,
            _lock: lock,
            store,
        })
    }

    fn global() -> Result<Self> {
        Self::open(crate::setup::settings_path()?.with_file_name("chatgpt"))
    }

    fn save(&self) -> Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temporary, &self.store)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.directory.join("accounts.json"))
            .map_err(|_| anyhow::anyhow!("Cannot atomically save ChatGPT credentials"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    fn active_index(&self) -> Result<usize> {
        self.store
            .accounts
            .iter()
            .position(|a| Some(&a.id) == self.store.active_account.as_ref())
            .context("Continue with ChatGPT in Global settings before choosing this provider")
    }
}

fn public_account(account: &Account, active: Option<&str>) -> SafeAccount {
    SafeAccount {
        id: account.id.clone(),
        label: account.label.clone(),
        email: account.email.clone(),
        active: active == Some(account.id.as_str()),
        connected: account.credentials.is_some(),
        plan_enabled: account
            .credentials
            .as_ref()
            .is_some_and(|c| c.scopes.iter().any(|s| s == PLAN_SCOPE)),
    }
}

pub fn model_id(name: &str) -> Option<&str> {
    name.strip_prefix("chatgpt/")
}

pub fn accounts() -> Result<Vec<SafeAccount>> {
    let locked = LockedStore::global()?;
    Ok(locked
        .store
        .accounts
        .iter()
        .map(|a| public_account(a, locked.store.active_account.as_deref()))
        .collect())
}

pub fn account_status() -> Result<String> {
    let locked = LockedStore::global()?;
    let Ok(index) = locked.active_index() else {
        return Ok("Not connected".into());
    };
    let account = public_account(
        &locked.store.accounts[index],
        locked.store.active_account.as_deref(),
    );
    let state = if locked.store.accounts[index].pending_credentials.is_some() {
        "renewal awaiting identity verification"
    } else if !account.connected {
        "signed out"
    } else if !account.plan_enabled {
        "ChatGPT plan use not enabled"
    } else {
        "ChatGPT plan connected"
    };
    Ok(format!("{} · {state}", account.label))
}

pub fn select_account(id: &str) -> Result<()> {
    let mut locked = LockedStore::global()?;
    let account = locked
        .store
        .accounts
        .iter()
        .find(|a| a.id == id)
        .context("Saved ChatGPT registration was not found")?;
    ensure!(
        account.credentials.is_some(),
        "This ChatGPT registration is signed out; continue with it first"
    );
    locked.store.active_account = Some(id.into());
    locked.save()?;
    clear_models();
    Ok(())
}

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow::anyhow!("Cannot initialize the ChatGPT connection"))
}

fn auth_endpoint(url: &str) -> Result<()> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| anyhow::anyhow!("OpenAI discovery returned an invalid endpoint"))?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.username().is_empty()
            && url.password().is_none(),
        "OpenAI discovery returned an untrusted endpoint"
    );
    Ok(())
}

fn discovery() -> Result<Discovery> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Discovery)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((saved, value)) = cache
        .as_ref()
        .filter(|(saved, _)| saved.elapsed() < Duration::from_secs(600))
    {
        let _ = saved;
        return Ok(value.clone());
    }
    let response = http_client()?
        .get(format!("{ISSUER}/.well-known/openid-configuration"))
        .send()
        .map_err(|_| anyhow::anyhow!("Cannot retrieve OpenAI sign-in configuration"))?;
    ensure!(
        response.status().is_success(),
        "OpenAI sign-in configuration returned HTTP {}",
        response.status()
    );
    let value: Discovery = response
        .json()
        .map_err(|_| anyhow::anyhow!("OpenAI sign-in configuration was invalid"))?;
    ensure!(
        value.issuer == ISSUER,
        "OpenAI sign-in issuer did not match"
    );
    for endpoint in [
        &value.authorization_endpoint,
        &value.token_endpoint,
        &value.jwks_uri,
    ] {
        auth_endpoint(endpoint)?;
    }
    if let Some(endpoint) = &value.revocation_endpoint {
        auth_endpoint(endpoint)?;
    }
    *cache = Some((Instant::now(), value.clone()));
    Ok(value)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot generate secure ChatGPT sign-in randomness"))?;
    Ok(bytes)
}

fn registration_id() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

struct Attempt {
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    client_id: Option<String>,
}

impl Attempt {
    fn new(port: u16, client_id: Option<String>) -> Result<Self> {
        Ok(Self {
            state: URL_SAFE_NO_PAD.encode(random_bytes::<32>()?),
            nonce: URL_SAFE_NO_PAD.encode(random_bytes::<32>()?),
            verifier: URL_SAFE_NO_PAD.encode(random_bytes::<32>()?),
            redirect: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
            client_id,
        })
    }

    fn url(
        &self,
        config: &Discovery,
        host: &str,
        existing: Option<&Account>,
    ) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&config.authorization_endpoint)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut params = url.query_pairs_mut();
        params.extend_pairs([
            (
                "client_id",
                self.client_id.as_deref().unwrap_or(DYNAMIC_CLIENT),
            ),
            ("ext_agent_host_id", host),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ]);
        if self.client_id.is_none() {
            params.append_pair("agent_name_hint", "Chuggin");
        }
        if let Some(account) = existing {
            if let Some(credentials) = &account.credentials {
                params.append_pair("id_token_hint", &credentials.id_token);
            }
            if let Some(email) = &account.email {
                params.append_pair("login_hint", email);
            }
        }
        drop(params);
        Ok(url)
    }
}

struct Callback {
    code: String,
    client_id: String,
}

fn callback(target: &str, attempt: &Attempt) -> Result<Callback> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| anyhow::anyhow!("Invalid ChatGPT callback"))?;
    ensure!(url.path() == CALLBACK_PATH, "Invalid ChatGPT callback path");
    let mut fields = HashMap::new();
    for (key, value) in url.query_pairs() {
        ensure!(
            fields
                .insert(key.into_owned(), value.into_owned())
                .is_none(),
            "Duplicate ChatGPT callback parameter"
        );
    }
    ensure!(
        fields.get("state") == Some(&attempt.state),
        "ChatGPT sign-in state did not match"
    );
    if fields.contains_key("error") {
        bail!("ChatGPT sign-in was declined or cancelled; no credentials were changed");
    }
    let client_id = match (&attempt.client_id, fields.get("client_id")) {
        (Some(expected), Some(actual)) => {
            ensure!(
                expected == actual,
                "ChatGPT callback changed this registration's client ID"
            );
            expected.clone()
        }
        (Some(expected), None) => expected.clone(),
        (None, Some(actual)) => actual.clone(),
        (None, None) => bail!("ChatGPT registration did not return an issued client ID"),
    };
    ensure!(
        !client_id.is_empty()
            && client_id != DYNAMIC_CLIENT
            && client_id.len() <= 512
            && !client_id.chars().any(char::is_control),
        "ChatGPT registration returned an invalid issued client ID"
    );
    let code = fields
        .remove("code")
        .filter(|code| {
            !code.is_empty() && code.len() <= 8192 && !code.chars().any(char::is_control)
        })
        .context("ChatGPT sign-in did not return an authorization code")?;
    Ok(Callback { code, client_id })
}

fn browser(url: &reqwest::Url) -> Result<()> {
    // Never log this URL: returning sign-in may contain the retained ID token.
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let child = Command::new(program).arg(url.as_str()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .map_err(|_| anyhow::anyhow!("Cannot open the system browser for ChatGPT sign-in; install a browser opener and try again"))?;
    thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

fn browser_response(stream: &mut TcpStream, valid: bool) {
    let body = if valid {
        "ChatGPT sign-in returned to Chuggin. You can close this tab and return to your terminal."
    } else {
        "This callback did not match a pending Chuggin sign-in."
    };
    let status = if valid { "200 OK" } else { "400 Bad Request" };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

fn wait_callback(listener: &TcpListener, attempt: &Attempt) -> Result<Callback> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        let (mut stream, peer) = match listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => bail!("ChatGPT callback listener failed"),
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut request = Vec::new();
        let mut byte = [0];
        let request_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        while request.len() < 16384 && Instant::now() < request_deadline {
            if stream.read(&mut byte).ok() != Some(1) {
                break;
            }
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        let mut line = request
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        let method = line.next();
        let target = line.next().unwrap_or_default();
        let expected_host = attempt
            .redirect
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default();
        let host_ok = request.lines().skip(1).any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("host") && value.trim() == expected_host
            })
        });
        let state_ok = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .ok()
            .is_some_and(|url| {
                url.path() == CALLBACK_PATH
                    && url
                        .query_pairs()
                        .any(|(name, value)| name == "state" && value == attempt.state)
            });
        if method != Some("GET") || !host_ok || !state_ok {
            browser_response(&mut stream, false);
            continue;
        }
        let result = callback(target, attempt);
        browser_response(&mut stream, result.is_ok());
        return result;
    }
    bail!("ChatGPT sign-in timed out; no credentials were changed")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
    earliest_refresh_at: Option<Value>,
}

#[derive(Debug)]
struct InvalidGrant;
impl std::fmt::Display for InvalidGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "ChatGPT authorization expired or was revoked; continue with this account again",
        )
    }
}
impl std::error::Error for InvalidGrant {}

#[derive(Debug)]
struct VerificationUnavailable;
impl std::fmt::Display for VerificationUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "OpenAI sign-in verification keys are unavailable; retry to finish token renewal",
        )
    }
}
impl std::error::Error for VerificationUnavailable {}

fn token_request(endpoint: &str, fields: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http_client()?
        .post(endpoint)
        .form(fields)
        .send()
        .map_err(|_| anyhow::anyhow!("ChatGPT token request could not reach OpenAI"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .map_err(|_| anyhow::anyhow!("OpenAI returned an invalid ChatGPT token response"))?;
    if !status.is_success() {
        if body["error"].as_str() == Some("invalid_grant") {
            return Err(InvalidGrant.into());
        }
        if body["error"].as_str() == Some("invalid_client") {
            bail!("ChatGPT registration is no longer accepted; continue with this account again");
        }
        bail!(
            "OpenAI could not authorize the ChatGPT connection (HTTP {status}); try again or review the app in ChatGPT Settings"
        );
    }
    let token: TokenResponse = serde_json::from_value(body)
        .map_err(|_| anyhow::anyhow!("OpenAI returned incomplete ChatGPT credentials"))?;
    ensure!(
        !token.access_token.is_empty()
            && token.token_type.eq_ignore_ascii_case("bearer")
            && token.expires_in > 0
            && token.expires_in <= 86400,
        "OpenAI returned invalid ChatGPT credential fields"
    );
    Ok(token)
}

#[derive(Deserialize)]
struct Identity {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
    iat: i64,
}

fn validate_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    jwks: &Value,
) -> Result<Identity> {
    let header =
        decode_header(token).map_err(|_| anyhow::anyhow!("ChatGPT ID token header was invalid"))?;
    ensure!(
        matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::ES256
                | Algorithm::ES384
        ),
        "ChatGPT ID token used an unsupported signing algorithm"
    );
    let kid = header
        .kid
        .as_deref()
        .context("ChatGPT ID token did not identify its signing key")?;
    let raw = jwks["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|key| key["kid"].as_str() == Some(kid)))
        .context("ChatGPT ID token signing key was not published by OpenAI")?;
    let algorithm = serde_json::to_value(header.alg)?;
    ensure!(
        raw["alg"].is_null() || raw["alg"] == algorithm,
        "ChatGPT signing key algorithm did not match"
    );
    ensure!(
        raw["use"].is_null() || raw["use"].as_str() == Some("sig"),
        "ChatGPT signing key is not authorized for signatures"
    );
    ensure!(
        raw["key_ops"].is_null()
            || raw["key_ops"]
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("verify"))),
        "ChatGPT signing key is not authorized for verification"
    );
    let keys: JwkSet = serde_json::from_value(jwks.clone())
        .map_err(|_| anyhow::anyhow!("OpenAI signing keys were invalid"))?;
    let key = DecodingKey::from_jwk(
        keys.find(kid)
            .context("ChatGPT signing key was unavailable")?,
    )
    .map_err(|_| anyhow::anyhow!("OpenAI signing key could not be decoded"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["sub", "iss", "aud", "exp", "iat"]);
    validation.leeway = 5;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| {
            anyhow::anyhow!("ChatGPT ID token signature or identity claims failed validation")
        })?
        .claims;
    ensure!(
        !identity.sub.is_empty(),
        "ChatGPT ID token did not contain an account identity"
    );
    ensure!(
        identity.iat <= chrono::Utc::now().timestamp() + 5,
        "ChatGPT ID token was issued in the future"
    );
    if let Some(expected) = nonce {
        ensure!(
            identity.nonce.as_deref() == Some(expected),
            "ChatGPT ID token nonce did not match"
        );
    }
    Ok(identity)
}

fn identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    config: &Discovery,
) -> Result<Identity> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    let kid = decode_header(token).ok().and_then(|header| header.kid);
    let available = cache.as_ref().is_some_and(|(saved, keys)| {
        saved.elapsed() < Duration::from_secs(600)
            && keys["keys"]
                .as_array()
                .is_some_and(|keys| keys.iter().any(|key| key["kid"].as_str() == kid.as_deref()))
    });
    if !available {
        let response = http_client()?
            .get(&config.jwks_uri)
            .send()
            .map_err(|_| anyhow::Error::from(VerificationUnavailable))?;
        if !response.status().is_success() {
            return Err(VerificationUnavailable.into());
        }
        let keys = response
            .json()
            .map_err(|_| anyhow::Error::from(VerificationUnavailable))?;
        *cache = Some((Instant::now(), keys));
    }
    validate_identity(token, client_id, nonce, &cache.as_ref().unwrap().1)
}

fn token_credentials(token: TokenResponse, previous: Option<&Credentials>) -> Result<Credentials> {
    ensure!(
        previous.and_then(|c| c.refresh_token.as_ref()).is_none()
            || token
                .refresh_token
                .as_ref()
                .is_some_and(|value| !value.is_empty()),
        "OpenAI did not return the replacement ChatGPT refresh token; continue with this account again"
    );
    let scopes = token
        .scope
        .as_deref()
        .map(|scope| scope.split_whitespace().map(String::from).collect())
        .or_else(|| previous.map(|c| c.scopes.clone()))
        .context("OpenAI did not return the granted ChatGPT permissions")?;
    let refresh_token = token
        .refresh_token
        .filter(|value| !value.is_empty())
        .or_else(|| previous.and_then(|c| c.refresh_token.clone()));
    let id_token = token
        .id_token
        .filter(|value| !value.is_empty())
        .or_else(|| previous.map(|c| c.id_token.clone()))
        .context("OpenAI did not return a ChatGPT ID token")?;
    let now = chrono::Utc::now();
    let earliest_refresh_at = token.earliest_refresh_at.as_ref().and_then(|value| {
        value.as_i64().or_else(|| {
            value.as_str().and_then(|value| {
                value.parse().ok().or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .ok()
                        .map(|time| time.timestamp())
                })
            })
        })
    });
    Ok(Credentials {
        access_token: token.access_token,
        refresh_token,
        id_token,
        scopes,
        expires_at: now.timestamp() + token.expires_in as i64,
        earliest_refresh_at,
        saved_at: now.to_rfc3339(),
    })
}

pub fn sign_in(existing_id: Option<&str>) -> Result<SafeAccount> {
    let (host_id, existing) = {
        let mut locked = LockedStore::global()?;
        if locked.store.host_id.is_empty() {
            locked.store.host_id = format!("urn:uuid:{}", registration_id()?);
            locked.save()?;
        }
        let existing = existing_id
            .map(|id| {
                locked
                    .store
                    .accounts
                    .iter()
                    .find(|a| a.id == id)
                    .cloned()
                    .context("Saved ChatGPT registration was not found")
            })
            .transpose()?;
        (locked.store.host_id.clone(), existing)
    };
    let config = discovery()?;
    let mut pending_client = existing.as_ref().map(|a| a.client_id.clone());
    for retry in 0..2 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("Cannot listen for the ChatGPT sign-in callback")?;
        let attempt = Attempt::new(listener.local_addr()?.port(), pending_client.clone())?;
        browser(&attempt.url(&config, &host_id, existing.as_ref())?)?;
        let callback = wait_callback(&listener, &attempt)?;
        pending_client = Some(callback.client_id.clone());
        let token = match token_request(
            &config.token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &callback.client_id),
                ("code", &callback.code),
                ("code_verifier", &attempt.verifier),
                ("redirect_uri", &attempt.redirect),
                ("resource", RESOURCE),
            ],
        ) {
            Err(error) if retry == 0 && error.is::<InvalidGrant>() => continue,
            result => result?,
        };
        let id_token = token
            .id_token
            .as_deref()
            .context("OpenAI did not return a ChatGPT ID token")?;
        let verified = identity(id_token, &callback.client_id, Some(&attempt.nonce), &config)?;
        if let Some(existing) = &existing {
            ensure!(
                existing.subject == verified.sub
                    && existing.issuer == ISSUER
                    && existing.client_id == callback.client_id,
                "ChatGPT sign-in did not match the selected registration; existing credentials were kept"
            );
        }
        let credentials = token_credentials(token, None)?;
        let mut locked = LockedStore::global()?;
        let id = if let Some(existing) = &existing {
            existing.id.clone()
        } else if let Some(account) = locked
            .store
            .accounts
            .iter()
            .find(|a| a.client_id == callback.client_id)
        {
            ensure!(
                account.subject == verified.sub && account.issuer == ISSUER,
                "ChatGPT client ID was already associated with a different identity"
            );
            account.id.clone()
        } else {
            registration_id()?
        };
        let email = verified
            .email
            .filter(|email| email.len() <= 254 && !email.chars().any(char::is_control));
        let label = existing
            .as_ref()
            .map(|a| a.label.clone())
            .unwrap_or_else(|| {
                format!(
                    "{} · {}",
                    email.as_deref().unwrap_or("ChatGPT account"),
                    &id[..8]
                )
            });
        let account = Account {
            id: id.clone(),
            label,
            email,
            issuer: ISSUER.into(),
            subject: verified.sub,
            client_id: callback.client_id,
            credentials: Some(credentials),
            pending_credentials: None,
        };
        if let Some(index) = locked.store.accounts.iter().position(|a| a.id == id) {
            locked.store.accounts[index] = account;
        } else {
            locked.store.accounts.push(account);
        }
        locked.store.active_account = Some(id.clone());
        locked.save()?;
        clear_models();
        return Ok(public_account(
            locked.store.accounts.iter().find(|a| a.id == id).unwrap(),
            Some(&id),
        ));
    }
    unreachable!()
}

fn get_token(rejected: Option<&str>) -> Result<String> {
    let mut locked = LockedStore::global()?;
    let index = locked.active_index()?;
    let now = chrono::Utc::now().timestamp();
    if let Some(pending) = locked.store.accounts[index].pending_credentials.as_ref() {
        if !pending.validate_id_token {
            settle_pending_with(&mut locked, index, |_, _| unreachable!())?;
        } else if pending.credentials.expires_at > now {
            return settle_pending(&mut locked, index, &discovery()?);
        }
    }
    let account = &locked.store.accounts[index];
    // A renewal interrupted for longer than its access-token lifetime resumes
    // with the latest rotating refresh token, never the obsolete committed one.
    let pending = account.pending_credentials.as_ref();
    let credentials = pending
        .map(|pending| &pending.credentials)
        .or(account.credentials.as_ref())
        .context("This ChatGPT account is signed out; continue with it in Global settings")?;
    ensure!(
        credentials.scopes.iter().any(|scope| scope == PLAN_SCOPE),
        "ChatGPT plan usage is not enabled for this registration; continue with it and enable plan access in ChatGPT Settings"
    );
    if pending.is_none()
        && (rejected
            .is_some_and(|token| token != credentials.access_token && now < credentials.expires_at)
            || (rejected.is_none() && now + 120 < credentials.expires_at))
    {
        return Ok(credentials.access_token.clone());
    }
    if credentials
        .earliest_refresh_at
        .is_some_and(|earliest| now < earliest)
    {
        ensure!(
            rejected.is_none() && now < credentials.expires_at,
            "ChatGPT token renewal is not available yet; try again shortly"
        );
        return Ok(credentials.access_token.clone());
    }
    let refresh = credentials
        .refresh_token
        .as_deref()
        .context("ChatGPT renewable session is unavailable; continue with this account again")?;
    let config = discovery()?;
    let token = match token_request(
        &config.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &account.client_id),
            ("refresh_token", refresh),
            ("resource", RESOURCE),
        ],
    ) {
        Err(error) if error.is::<InvalidGrant>() => {
            locked.store.accounts[index].credentials = None;
            locked.store.accounts[index].pending_credentials = None;
            locked.save()?;
            clear_models();
            return Err(error);
        }
        result => result?,
    };
    let validate_id_token =
        token.id_token.is_some() || pending.is_some_and(|pending| pending.validate_id_token);
    let replacement = token_credentials(token, Some(credentials));
    let replacement = match replacement {
        Ok(credentials) => credentials,
        Err(error) => {
            // The server may already have rotated the old refresh token. Never
            // keep retrying that obsolete credential after a failed validation.
            locked.store.accounts[index].credentials = None;
            locked.store.accounts[index].pending_credentials = None;
            locked.save()?;
            clear_models();
            return Err(error.context("Renewed ChatGPT credentials could not be validated; continue with this account again"));
        }
    };
    // Save the rotating token before any additional network verification. It
    // remains unusable until the registered identity has been checked.
    locked.store.accounts[index].pending_credentials = Some(PendingCredentials {
        credentials: replacement,
        validate_id_token,
    });
    locked.save()?;
    settle_pending(&mut locked, index, &config)
}

fn settle_pending(locked: &mut LockedStore, index: usize, config: &Discovery) -> Result<String> {
    settle_pending_with(locked, index, |token, client| {
        identity(token, client, None, config)
    })
}

fn settle_pending_with(
    locked: &mut LockedStore,
    index: usize,
    verify: impl FnOnce(&str, &str) -> Result<Identity>,
) -> Result<String> {
    let account = &locked.store.accounts[index];
    let pending = account
        .pending_credentials
        .as_ref()
        .context("ChatGPT renewal was not pending")?;
    if pending.validate_id_token {
        let verified = verify(&pending.credentials.id_token, &account.client_id);
        let verification = verified.and_then(|identity| {
            ensure!(
                identity.sub == account.subject && account.issuer == ISSUER,
                "Renewed ChatGPT identity did not match this registration"
            );
            Ok(())
        });
        if let Err(error) = verification {
            if !error.is::<VerificationUnavailable>() {
                locked.store.accounts[index].credentials = None;
                locked.store.accounts[index].pending_credentials = None;
                locked.save()?;
                clear_models();
            }
            return Err(error);
        }
    }
    let replacement = locked.store.accounts[index]
        .pending_credentials
        .take()
        .unwrap()
        .credentials;
    let plan_enabled = replacement.scopes.iter().any(|scope| scope == PLAN_SCOPE);
    let access = replacement.access_token.clone();
    locked.store.accounts[index].credentials = Some(replacement);
    locked.save()?;
    if !plan_enabled {
        clear_models();
        bail!("ChatGPT plan permission is no longer enabled; continue with this account again");
    }
    Ok(access)
}

pub fn access_token() -> Result<String> {
    get_token(None)
}
pub fn refresh_access_token(rejected: &str) -> Result<String> {
    get_token(Some(rejected))
}

pub fn disconnect_active() -> Result<String> {
    let mut locked = LockedStore::global()?;
    let index = locked.active_index()?;
    let account = &locked.store.accounts[index];
    let credentials = account
        .pending_credentials
        .as_ref()
        .map(|pending| &pending.credentials)
        .or(account.credentials.as_ref());
    let mut confirmed = credentials
        .as_ref()
        .and_then(|c| c.refresh_token.as_ref())
        .is_none();
    if let Some(refresh) = credentials.and_then(|c| c.refresh_token.as_deref())
        && let Ok(config) = discovery()
        && let Some(endpoint) = config.revocation_endpoint
        && let Ok(client) = http_client()
    {
        for delay in [0, 250, 1000] {
            if delay > 0 {
                thread::sleep(Duration::from_millis(delay));
            }
            match client
                .post(&endpoint)
                .form(&[
                    ("token", refresh),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", account.client_id.as_str()),
                ])
                .send()
            {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    confirmed = true;
                    break;
                }
                Ok(response) if !response.status().is_server_error() => break,
                _ => {}
            }
        }
    }
    locked.store.accounts[index].credentials = None;
    locked.store.accounts[index].pending_credentials = None;
    locked.save()?;
    clear_models();
    Ok(if confirmed { "Signed out of this ChatGPT registration." } else { "Signed out locally. Remote revocation was not confirmed; disconnect Chuggin in ChatGPT Settings to end that session." }.into())
}

type ModelCache = Option<(String, Instant, Value)>;
fn model_cache() -> &'static Mutex<ModelCache> {
    static CACHE: OnceLock<Mutex<ModelCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}
fn clear_models() {
    *model_cache().lock().unwrap() = None;
}

fn active_id() -> Result<String> {
    let locked = LockedStore::global()?;
    Ok(locked.store.accounts[locked.active_index()?].id.clone())
}

pub fn models() -> Result<Value> {
    let id = active_id()?;
    let client = http_client()?;
    let mut token = access_token()?;
    let mut response = client
        .get(format!("{RESOURCE}/models"))
        .bearer_auth(&token)
        .send()
        .map_err(|_| anyhow::anyhow!("Cannot retrieve this ChatGPT account's model catalog"))?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        token = refresh_access_token(&token)?;
        response = client
            .get(format!("{RESOURCE}/models"))
            .bearer_auth(token)
            .send()
            .map_err(|_| anyhow::anyhow!("Cannot retrieve this ChatGPT account's model catalog"))?;
    }
    ensure!(
        response.status().is_success(),
        "ChatGPT account model catalog returned HTTP {}; review this registration's plan access in ChatGPT Settings",
        response.status()
    );
    let catalog: Value = response
        .json()
        .map_err(|_| anyhow::anyhow!("ChatGPT account model catalog was invalid"))?;
    ensure!(
        catalog["models"].is_array(),
        "ChatGPT account model catalog did not contain a models array"
    );
    ensure!(
        active_id()? == id,
        "The active ChatGPT account changed; refresh the model catalog"
    );
    *model_cache().lock().unwrap() = Some((id, Instant::now(), catalog.clone()));
    Ok(catalog)
}

pub fn model_info(name: &str) -> Result<Value> {
    let slug = model_id(name).context("Expected a chatgpt/ model")?;
    let id = active_id()?;
    let cached = model_cache()
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(account, saved, _)| *account == id && saved.elapsed() < Duration::from_secs(60))
        .map(|(_, _, catalog)| catalog.clone());
    let catalog = match cached {
        Some(catalog) => catalog,
        None => models()?,
    };
    catalog["models"].as_array().unwrap().iter().find(|model| model["slug"].as_str() == Some(slug)).cloned()
        .context("This model is unavailable to the active ChatGPT account; choose from its current catalog")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    // An isolated, disposable test key; it never authenticates to a provider.
    const TEST_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgqHycpw8U5XXno5mcAQNPMoy0zWrHKuIPigrB-IqSXxihRANCAASZEY5rn1CXQFNIcp9E7QsAapvQyXF95HEiDvHFZ6xXX3RQJiF2e_1uhLYJnLcm71xrCY3qYVZl5bz6PH3MT_8b";

    fn test_jwks() -> Value {
        json!({"keys":[{"kty":"EC", "crv":"P-256", "kid":"test", "alg":"ES256", "use":"sig",
            "x":"mRGOa59Ql0BTSHKfRO0LAGqb0MlxfeRxIg7xxWesV18",
            "y":"dFAmIXZ7_W6EtgmctybvXGsJjephVmXlvPo8fcxP_xs"}]})
    }

    fn claims() -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({"iss":ISSUER,"aud":"issued-client","sub":"account-identity","email":"test@example.test",
            "iat":now,"exp":now+300,"nonce":"pending-nonce"})
    }

    fn signed(claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("test".into());
        encode(
            &header,
            claims,
            &EncodingKey::from_ec_der(&URL_SAFE_NO_PAD.decode(TEST_KEY).unwrap()),
        )
        .unwrap()
    }

    fn test_discovery() -> Discovery {
        Discovery {
            issuer: ISSUER.into(),
            authorization_endpoint: format!("{ISSUER}/api/accounts/authorize"),
            token_endpoint: format!("{ISSUER}/api/accounts/oauth/token"),
            jwks_uri: format!("{ISSUER}/.well-known/jwks.json"),
            revocation_endpoint: None,
        }
    }

    fn saved_account(id: &str, client: &str) -> Account {
        Account {
            id: id.into(),
            label: format!("test@example.test · {id}"),
            email: Some("test@example.test".into()),
            issuer: ISSUER.into(),
            subject: "private-subject".into(),
            client_id: client.into(),
            credentials: Some(Credentials {
                access_token: format!("private-access-{id}"),
                refresh_token: Some(format!("private-refresh-{id}")),
                id_token: format!("private-id-{id}"),
                scopes: vec![PLAN_SCOPE.into()],
                expires_at: 0,
                earliest_refresh_at: None,
                saved_at: String::new(),
            }),
            pending_credentials: None,
        }
    }

    #[test]
    fn verifies_signature_issuer_audience_expiry_nonce_and_issued_at() {
        let claims = claims();
        let identity = validate_identity(
            &signed(&claims),
            "issued-client",
            Some("pending-nonce"),
            &test_jwks(),
        )
        .unwrap();
        assert_eq!(identity.sub, "account-identity");
        assert!(
            validate_identity(
                &signed(&claims),
                "other-client",
                Some("pending-nonce"),
                &test_jwks()
            )
            .is_err()
        );
        assert!(
            validate_identity(
                &signed(&claims),
                "issued-client",
                Some("other-nonce"),
                &test_jwks()
            )
            .is_err()
        );
        for (field, invalid) in [
            ("iss", json!("https://untrusted.example.test")),
            ("exp", json!(chrono::Utc::now().timestamp() - 60)),
            ("iat", json!(chrono::Utc::now().timestamp() + 60)),
            ("sub", json!("")),
        ] {
            let mut changed = claims.clone();
            changed[field] = invalid;
            assert!(
                validate_identity(
                    &signed(&changed),
                    "issued-client",
                    Some("pending-nonce"),
                    &test_jwks()
                )
                .is_err(),
                "Accepted invalid {field}"
            );
        }
        let mut missing_iat = claims.clone();
        missing_iat.as_object_mut().unwrap().remove("iat");
        assert!(
            validate_identity(
                &signed(&missing_iat),
                "issued-client",
                Some("pending-nonce"),
                &test_jwks()
            )
            .is_err()
        );
        let mut forged: Vec<String> = signed(&claims).split('.').map(String::from).collect();
        let mut changed = claims.clone();
        changed["sub"] = json!("forged-identity");
        forged[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&changed).unwrap());
        assert!(
            validate_identity(
                &forged.join("."),
                "issued-client",
                Some("pending-nonce"),
                &test_jwks()
            )
            .is_err()
        );
        let mut wrong_key = test_jwks();
        wrong_key["keys"][0]["use"] = json!("enc");
        assert!(
            validate_identity(
                &signed(&claims),
                "issued-client",
                Some("pending-nonce"),
                &wrong_key
            )
            .is_err()
        );
    }

    #[test]
    fn authorization_uses_pkce_stable_host_and_registration_specific_hints() {
        let mut attempt = Attempt::new(54321, None).unwrap();
        attempt.verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into();
        let url = attempt
            .url(&test_discovery(), "urn:uuid:test-host", None)
            .unwrap();
        let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            fields["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(fields["client_id"], DYNAMIC_CLIENT);
        assert_eq!(fields["agent_name_hint"], "Chuggin");
        assert_eq!(fields["ext_agent_host_id"], "urn:uuid:test-host");
        assert_eq!(
            fields["redirect_uri"],
            "http://127.0.0.1:54321/auth/callback"
        );
        assert!(!fields.contains_key("id_token_hint"));
        let account = saved_account("one", "issued-one");
        attempt.client_id = Some(account.client_id.clone());
        let url = attempt
            .url(&test_discovery(), "urn:uuid:test-host", Some(&account))
            .unwrap();
        let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(fields["client_id"], "issued-one");
        assert_eq!(fields["id_token_hint"], "private-id-one");
        assert!(!fields.contains_key("agent_name_hint"));
    }

    #[test]
    fn callback_rejects_state_confusion_and_rebinding_returning_clients() {
        let mut attempt = Attempt::new(54321, None).unwrap();
        let valid = format!(
            "{CALLBACK_PATH}?state={}&code=test-code&client_id=issued-one",
            attempt.state
        );
        assert_eq!(callback(&valid, &attempt).unwrap().client_id, "issued-one");
        assert!(callback(&valid.replace(&attempt.state, "wrong-state"), &attempt).is_err());
        assert!(callback(&format!("{valid}&state={}", attempt.state), &attempt).is_err());
        assert!(callback(&valid.replace("issued-one", DYNAMIC_CLIENT), &attempt).is_err());
        assert!(
            callback(
                &format!("{CALLBACK_PATH}?state={}&code=test-code", attempt.state),
                &attempt
            )
            .is_err()
        );
        assert!(
            callback(
                &format!(
                    "{CALLBACK_PATH}?state={}&error=access_denied",
                    attempt.state
                ),
                &attempt
            )
            .is_err()
        );
        attempt.client_id = Some("issued-one".into());
        assert_eq!(
            callback(
                &format!("{CALLBACK_PATH}?state={}&code=test-code", attempt.state),
                &attempt
            )
            .unwrap()
            .client_id,
            "issued-one"
        );
        assert!(callback(&valid.replace("issued-one", "issued-two"), &attempt).is_err());
    }

    #[test]
    fn protected_records_keep_same_email_registrations_and_refresh_tokens_separate() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join("credentials");
        let mut locked = LockedStore::open(directory.clone()).unwrap();
        locked.store.host_id = "urn:uuid:stable-host".into();
        locked.store.accounts = vec![
            saved_account("one", "issued-one"),
            saved_account("two", "issued-two"),
        ];
        locked.store.active_account = Some("one".into());
        locked.save().unwrap();
        let safe =
            serde_json::to_string(&public_account(&locked.store.accounts[0], Some("one"))).unwrap();
        assert!(!safe.contains("private-") && !safe.contains("issued-one"));
        let next = token_credentials(
            TokenResponse {
                access_token: "rotated-access".into(),
                refresh_token: Some("rotated-refresh".into()),
                id_token: None,
                token_type: "Bearer".into(),
                expires_in: 3600,
                scope: None,
                earliest_refresh_at: Some(json!(123)),
            },
            locked.store.accounts[0].credentials.as_ref(),
        )
        .unwrap();
        assert_eq!(next.id_token, "private-id-one");
        assert!(next.scopes.iter().any(|scope| scope == PLAN_SCOPE));
        locked.store.accounts[0].credentials = Some(next);
        locked.save().unwrap();
        drop(locked);
        let mut resumed = LockedStore::open(directory.clone()).unwrap();
        assert_eq!(resumed.store.host_id, "urn:uuid:stable-host");
        assert_eq!(
            resumed.store.accounts[0]
                .credentials
                .as_ref()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
        assert_eq!(
            resumed.store.accounts[1]
                .credentials
                .as_ref()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("private-refresh-two")
        );
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(directory.join("accounts.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        resumed.store.accounts[0].credentials = None;
        resumed.save().unwrap();
        drop(resumed);
        let resumed = LockedStore::open(directory).unwrap();
        assert!(resumed.store.accounts[0].credentials.is_none());
        assert_eq!(resumed.store.accounts[0].client_id, "issued-one");
        assert!(resumed.store.accounts[1].credentials.is_some());
    }

    #[test]
    fn refresh_storage_lock_serializes_read_modify_write_and_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join("credentials");
        let mut first = LockedStore::open(directory.clone()).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let next_directory = directory.clone();
        let next = thread::spawn(move || {
            let second = LockedStore::open(next_directory).unwrap();
            sent.send(second.store.host_id).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        first.store.host_id = "latest-host".into();
        first.save().unwrap();
        drop(first);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            "latest-host"
        );
        next.join().unwrap();
        let sentinel = dir.path().join("unrelated.json");
        fs::write(&sentinel, b"keep").unwrap();
        fs::remove_file(directory.join("accounts.json")).unwrap();
        std::os::unix::fs::symlink(&sentinel, directory.join("accounts.json")).unwrap();
        assert!(LockedStore::open(directory).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    }

    #[test]
    fn discovery_refuses_endpoints_that_could_receive_tokens_off_origin() {
        auth_endpoint("https://auth.openai.com/api/accounts/oauth/token").unwrap();
        for endpoint in [
            "http://auth.openai.com/token",
            "https://untrusted.example.test/token",
            "https://name@auth.openai.com/token",
        ] {
            assert!(auth_endpoint(endpoint).is_err());
        }
    }

    #[test]
    fn interrupted_refresh_keeps_rotation_pending_until_identity_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let mut locked = LockedStore::open(dir.path().join("credentials")).unwrap();
        let mut account = saved_account("one", "issued-one");
        let mut replacement = account.credentials.as_ref().unwrap().clone();
        replacement.access_token = "pending-access".into();
        replacement.refresh_token = Some("pending-refresh".into());
        replacement.id_token = "pending-id".into();
        account.pending_credentials = Some(PendingCredentials {
            credentials: replacement,
            validate_id_token: true,
        });
        locked.store.accounts.push(account);
        locked.store.active_account = Some("one".into());
        locked.save().unwrap();
        let error = settle_pending_with(&mut locked, 0, |_, _| Err(VerificationUnavailable.into()))
            .unwrap_err();
        assert!(error.is::<VerificationUnavailable>());
        drop(locked);
        let mut resumed = LockedStore::open(dir.path().join("credentials")).unwrap();
        assert_eq!(
            resumed.store.accounts[0]
                .pending_credentials
                .as_ref()
                .unwrap()
                .credentials
                .refresh_token
                .as_deref(),
            Some("pending-refresh")
        );
        let access = settle_pending_with(&mut resumed, 0, |token, client| {
            assert_eq!(token, "pending-id");
            assert_eq!(client, "issued-one");
            Ok(Identity {
                sub: "private-subject".into(),
                email: None,
                nonce: None,
                iat: 0,
            })
        })
        .unwrap();
        assert_eq!(access, "pending-access");
        assert!(resumed.store.accounts[0].pending_credentials.is_none());
        assert_eq!(
            resumed.store.accounts[0]
                .credentials
                .as_ref()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("pending-refresh")
        );
    }

    #[test]
    fn pending_refresh_rejects_changed_identity_and_saves_revoked_plan_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let mut locked = LockedStore::open(dir.path().join("credentials")).unwrap();
        let mut account = saved_account("one", "issued-one");
        account.pending_credentials = Some(PendingCredentials {
            credentials: account.credentials.as_ref().unwrap().clone(),
            validate_id_token: true,
        });
        locked.store.accounts.push(account);
        locked.store.active_account = Some("one".into());
        locked.save().unwrap();
        assert!(
            settle_pending_with(&mut locked, 0, |_, _| Ok(Identity {
                sub: "different-account".into(),
                email: None,
                nonce: None,
                iat: 0
            }))
            .is_err()
        );
        assert!(locked.store.accounts[0].credentials.is_none());
        assert!(locked.store.accounts[0].pending_credentials.is_none());
        assert_eq!(locked.store.accounts[0].client_id, "issued-one");
        let mut replacement = saved_account("one", "issued-one").credentials.unwrap();
        replacement.scopes.clear();
        replacement.refresh_token = Some("latest-revoked-plan-refresh".into());
        locked.store.accounts[0].pending_credentials = Some(PendingCredentials {
            credentials: replacement,
            validate_id_token: false,
        });
        locked.save().unwrap();
        assert!(
            settle_pending_with(&mut locked, 0, |_, _| panic!(
                "No ID token verification should be needed"
            ))
            .is_err()
        );
        drop(locked);
        let resumed = LockedStore::open(dir.path().join("credentials")).unwrap();
        let credentials = resumed.store.accounts[0].credentials.as_ref().unwrap();
        assert!(credentials.scopes.is_empty());
        assert_eq!(
            credentials.refresh_token.as_deref(),
            Some("latest-revoked-plan-refresh")
        );
    }
}
