//! Public-web proxy and local-only desktop bridge. The model gets no host
//! network namespace or container-management socket. DNS results are validated
//! before connecting, then the approved address is used directly.
use crate::sandbox::Target;
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
static STARTED: LazyLock<Mutex<BTreeMap<PathBuf, Arc<AtomicBool>>>> = LazyLock::new(Mutex::default);
static DESKTOPS: LazyLock<Mutex<BTreeMap<String, String>>> = LazyLock::new(Mutex::default);
pub fn socket(t: &Target) -> PathBuf {
    t.state.join("computer/channel/public-web.sock")
}
pub fn start(t: &Target) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let path = socket(t);
        let mut started = STARTED.lock().unwrap();
        if let Some(policy) = started.get(&path) {
            policy.store(t.internet, Ordering::SeqCst);
            return Ok(());
        }
        let policy = Arc::new(AtomicBool::new(t.internet));
        let session_policy = policy.clone();
        fs::create_dir_all(path.parent().unwrap())?;
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let count = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut stream) = connection else { break };
                if count.fetch_add(1, Ordering::SeqCst) >= 32 {
                    count.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
                let count = count.clone();
                let policy = session_policy.clone();
                std::thread::spawn(move || {
                    let failure = stream.try_clone().ok();
                    let result = (|| -> Result<()> {
                        if !policy.load(Ordering::SeqCst) {
                            stream.write_all(
                                b"HTTP/1.1 403 Internet Disabled\r\nContent-Length: 0\r\n\r\n",
                            )?;
                            return Ok(());
                        }
                        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                        let header = header(&mut stream)?;
                        let (authority, first) = request(&header)?;
                        let addresses: Vec<_> = authority.to_socket_addrs()?.collect();
                        ensure!(
                            !addresses.is_empty()
                                && addresses.iter().all(|a| crate::web_tools::public(a.ip())),
                            "Only public Internet addresses are available"
                        );
                        let mut remote = addresses
                            .iter()
                            .find_map(|address| {
                                TcpStream::connect_timeout(address, Duration::from_secs(3)).ok()
                            })
                            .context("No approved public address accepted the connection")?;
                        remote.set_read_timeout(Some(Duration::from_secs(60)))?;
                        remote.set_write_timeout(Some(Duration::from_secs(30)))?;
                        if let Some(request) = first {
                            remote.write_all(&request)?;
                        } else {
                            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                        }
                        let mut upload = remote.try_clone()?;
                        let mut download = stream.try_clone()?;
                        let worker = std::thread::spawn(move || {
                            let _ = std::io::copy(&mut stream, &mut upload);
                            let _ = upload.shutdown(Shutdown::Write);
                        });
                        let _ = std::io::copy(&mut remote, &mut download);
                        let _ = download.shutdown(Shutdown::Write);
                        let _ = worker.join();
                        Ok(())
                    })();
                    if let Err(error) = result {
                        if let Some(mut failed) = failure {
                            let _ = failed.write_all(
                                b"HTTP/1.1 403 Public Internet Only\r\nContent-Length: 0\r\n\r\n",
                            );
                        }
                        crate::events::log(format!("Private computer web proxy: {error}"));
                    }
                    count.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        started.insert(path, policy);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = t;
        anyhow::bail!("Private computers currently require Linux")
    }
}
fn header(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while out.len() < 16_384 {
        ensure!(
            stream.read(&mut byte)? == 1,
            "Connection closed before request header"
        );
        out.push(byte[0]);
        if out.ends_with(b"\r\n\r\n") {
            return Ok(out);
        }
    }
    anyhow::bail!("Request header exceeds 16 KiB")
}
fn request(bytes: &[u8]) -> Result<(String, Option<Vec<u8>>)> {
    let text = std::str::from_utf8(bytes)?;
    let first = text.lines().next().context("Missing request line")?;
    let parts: Vec<_> = first.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[2].starts_with("HTTP/1."),
        "Invalid HTTP request"
    );
    let tunnel = parts[0] == "CONNECT";
    let url = reqwest::Url::parse(&if tunnel {
        format!("https://{}", parts[1])
    } else {
        parts[1].into()
    })?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Proxy target credentials are not accepted"
    );
    let port = url.port_or_known_default().context("Missing target port")?;
    ensure!(
        (tunnel && url.scheme() == "https" && port == 443 && url.path() == "/")
            || (!tunnel && url.scheme() == "http" && port == 80),
        "Only HTTP port 80 and HTTPS CONNECT port 443 are available"
    );
    let host = url.host_str().context("Missing target host")?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    if tunnel {
        return Ok((authority, None));
    }
    let path = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    let mut rewritten = format!("{} {} {}\r\n", parts[0], path, parts[2]);
    for line in text.split("\r\n").skip(1).filter(|l| !l.is_empty()) {
        if !line.to_ascii_lowercase().starts_with("proxy-") {
            rewritten.push_str(line);
            rewritten.push_str("\r\n");
        }
    }
    rewritten.push_str("\r\n");
    Ok((authority, Some(rewritten.into_bytes())))
}

pub fn desktop(t: &Target) -> Result<String> {
    let mut viewers = DESKTOPS.lock().unwrap();
    if let Some(url) = viewers.get(&t.name) {
        return Ok(url.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|e| anyhow::anyhow!("Cannot create desktop access token: {e}"))?;
    let token = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let prefix = format!("/{token}/");
    let target = t.clone();
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            if count.fetch_add(1, Ordering::SeqCst) >= 32 {
                count.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let prefix = prefix.clone();
            let target = target.clone();
            let count = count.clone();
            std::thread::spawn(move || {
                let _ = (|| -> Result<()> {
                    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                    let bytes = header(&mut stream)?;
                    let text = std::str::from_utf8(&bytes)?;
                    let mut lines = text.splitn(2, "\r\n");
                    let first = lines.next().unwrap();
                    let parts: Vec<_> = first.split_whitespace().collect();
                    if parts.len() != 3 || !parts[1].starts_with(&prefix) {
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
                        return Ok(());
                    }
                    let (request, websocket) = desktop_request(&bytes, &prefix)?;
                    if websocket {
                        // An idle desktop is a valid long-lived connection.
                        stream.set_read_timeout(None)?;
                    }
                    let mut command = Command::new("podman");
                    command.args([
                        "--remote=false",
                        "exec",
                        "--interactive",
                        &target.name,
                        "socat",
                        "STDIO",
                        "TCP:127.0.0.1:6080",
                    ]);
                    let mut child = command
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()?;
                    let mut input = child.stdin.take().unwrap();
                    input.write_all(request.as_bytes())?;
                    let mut output = child.stdout.take().unwrap();
                    let mut from_browser = stream.try_clone()?;
                    let upload = std::thread::spawn(move || {
                        let _ = relay_interactive(&mut from_browser, &mut input);
                    });
                    let _ = relay_interactive(&mut output, &mut stream);
                    let _ = stream.shutdown(Shutdown::Both);
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = upload.join();
                    Ok(())
                })();
                count.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    let url = format!(
        "http://127.0.0.1:{port}/{token}/vnc.html?autoconnect=1&resize=scale&path=/{token}/websockify"
    );
    viewers.insert(t.name.clone(), url.clone());
    Ok(url)
}

// std::io::copy can select a socket-to-pipe splice while holding the pipe's
// write side, preventing Podman from reading the header already queued there.
// Explicit reads/writes preserve full-duplex progress before either side closes.
fn relay_interactive(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<u64> {
    let mut buffer = [0u8; 8192];
    let mut total = 0;
    loop {
        let size = match from.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if size == 0 {
            return Ok(total);
        }
        to.write_all(&buffer[..size])?;
        total += size as u64;
    }
}

fn desktop_request(bytes: &[u8], prefix: &str) -> Result<(String, bool)> {
    let text = std::str::from_utf8(bytes)?;
    let mut lines = text.split("\r\n");
    let first = lines.next().context("Missing desktop request")?;
    let parts: Vec<_> = first.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[1].starts_with(prefix),
        "Desktop access token missing"
    );
    let headers: Vec<_> = lines.filter(|line| !line.is_empty()).collect();
    let websocket = headers.iter().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("upgrade") && value.trim().eq_ignore_ascii_case("websocket")
        })
    });
    let mut request = format!(
        "{} /{} {}\r\n",
        parts[0],
        &parts[1][prefix.len()..],
        parts[2]
    );
    for line in headers {
        if line.split_once(':').is_some_and(|(name, _)| {
            ["cookie", "authorization", "proxy-authorization"]
                .iter()
                .any(|header| name.eq_ignore_ascii_case(header))
        }) {
            continue;
        }
        if !websocket
            && line.split_once(':').is_some_and(|(name, _)| {
                name.eq_ignore_ascii_case("connection")
                    || name.eq_ignore_ascii_case("proxy-connection")
            })
        {
            continue;
        }
        request.push_str(line);
        request.push_str("\r\n");
    }
    // Each HTTP request gets a newly authenticated/re-written bridge connection.
    // A persistent raw tunnel would forward later token-prefixed paths unchanged.
    if !websocket {
        request.push_str("Connection: close\r\n");
    }
    request.push_str("\r\n");
    Ok((request, websocket))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_http_connections_close_but_websocket_upgrades_stay_open() {
        let (request, websocket) = desktop_request(
            b"GET /secret/app/ui.js HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nCookie: other-local-app-secret\r\nAuthorization: private\r\nProxy-Authorization: private\r\n\r\n",
            "/secret/",
        )
        .unwrap();
        assert!(!websocket);
        assert!(request.starts_with("GET /app/ui.js HTTP/1.1\r\n"));
        assert!(request.ends_with("Connection: close\r\n\r\n"));
        assert!(!request.contains("keep-alive"));
        assert!(!request.contains("other-local-app-secret"));
        assert!(!request.contains("private\r\n"));
        let (request,websocket)=desktop_request(b"GET /secret/websockify HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n", "/secret/").unwrap();
        assert!(websocket);
        assert!(request.starts_with("GET /websockify HTTP/1.1\r\n"));
        assert!(request.contains("Connection: Upgrade"));
        assert!(!request.contains("Connection: close"));
        assert!(desktop_request(b"GET /websockify HTTP/1.1\r\n\r\n", "/secret/").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn desktop_relay_delivers_data_before_either_direction_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut browser = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut upload = server.try_clone().unwrap();
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"first\n").unwrap();
        let mut output = child.stdout.take().unwrap();
        let up = std::thread::spawn(move || relay_interactive(&mut upload, &mut input));
        let down = std::thread::spawn(move || relay_interactive(&mut output, &mut server));
        let result = (|| -> std::io::Result<()> {
            let mut first = [0; 6];
            browser.read_exact(&mut first)?;
            assert_eq!(&first, b"first\n");
            browser.write_all(b"second\n")?;
            let mut second = [0; 7];
            browser.read_exact(&mut second)?;
            assert_eq!(&second, b"second\n");
            Ok(())
        })();
        let _ = browser.shutdown(Shutdown::Both);
        let _ = child.kill();
        let _ = child.wait();
        let _ = up.join();
        let _ = down.join();
        result.expect(
            "Both the initial queued header and later messages must pass without waiting for EOF",
        );
    }

    #[test]
    fn proxy_validates_targets_and_strips_proxy_credentials() {
        let (host,request)=request(b"GET http://example.com/path?q=x HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: secret\r\n\r\n").unwrap();
        assert_eq!(host, "example.com:80");
        let r = String::from_utf8(request.unwrap()).unwrap();
        assert!(r.starts_with("GET /path?q=x HTTP/1.1"));
        assert!(!r.contains("secret"));
        assert!(request_fn(b"CONNECT example.com:22 HTTP/1.1\r\n\r\n").is_err());
        assert!(request_fn(b"GET file:///etc/passwd HTTP/1.1\r\n\r\n").is_err());
        assert!(!crate::web_tools::public("127.0.0.1".parse().unwrap()));
        assert!(!crate::web_tools::public("192.168.1.164".parse().unwrap()));
    }
    fn request_fn(bytes: &[u8]) -> Result<(String, Option<Vec<u8>>)> {
        request(bytes)
    }
}
