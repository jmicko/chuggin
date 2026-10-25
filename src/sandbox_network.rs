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
                    let request = format!(
                        "{} /{} {}\r\n{}",
                        parts[0],
                        &parts[1][prefix.len()..],
                        parts[2],
                        lines.next().unwrap_or("")
                    );
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
                        let _ = std::io::copy(&mut from_browser, &mut input);
                    });
                    let _ = std::io::copy(&mut output, &mut stream);
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
    Ok(format!(
        "http://127.0.0.1:{port}/{token}/vnc.html?autoconnect=1&resize=scale&path={token}/websockify"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
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
