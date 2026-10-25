//! Advisory human presence from the host login session. This module never reads
//! keys, window titles, clipboard contents, model activity, or the guest desktop.
//! A single background sampler keeps OS calls off inference/UI threads.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    sync::{LazyLock, Mutex, Once},
    time::{Duration, Instant},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Automatic,
    Active,
    Away,
    Busy,
}
impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Active => "Active · set manually",
            Self::Away => "Away · set manually",
            Self::Busy => "Busy · set manually",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub away_after_minutes: u32,
    pub mode: Mode,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            away_after_minutes: 5,
            mode: Mode::Automatic,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1440).contains(&self.away_after_minutes),
            "Automatic away time must be 1–1440 minutes"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Away,
    Busy,
    Unknown,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Away => "Away",
            Self::Busy => "Busy",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub status: Status,
    pub automatic: bool,
    pub idle_seconds: Option<u64>,
    pub locked: Option<bool>,
    pub source: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
struct Observation {
    idle_seconds: Option<u64>,
    locked: Option<bool>,
    screen_inactive: bool,
    source: &'static str,
}
impl Observation {
    fn unavailable() -> Self {
        Self {
            idle_seconds: None,
            locked: None,
            screen_inactive: false,
            source: "unavailable",
        }
    }
}

#[derive(Clone, Default)]
struct Cache {
    observation: Option<Observation>,
    sampled: Option<Instant>,
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);
static START: Once = Once::new();

/// This is intentionally host-only. Call it from the controller/UI, never from
/// the sandbox guest process. Calling it cannot mark the person active.
pub fn snapshot(settings: &Settings) -> Snapshot {
    if settings.mode != Mode::Automatic {
        return classify(settings, None, None);
    }
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("human-presence".into())
            .spawn(|| {
                loop {
                    let observation = query_host();
                    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Cache {
                        observation: Some(observation),
                        sampled: Some(Instant::now()),
                    };
                    std::thread::sleep(SAMPLE_INTERVAL);
                }
            });
    });
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let age = cache.sampled.map(|t| t.elapsed());
    classify(settings, cache.observation.as_ref(), age)
}

fn classify(
    settings: &Settings,
    observation: Option<&Observation>,
    age: Option<Duration>,
) -> Snapshot {
    let manual = match settings.mode {
        Mode::Active => Some(Status::Active),
        Mode::Away => Some(Status::Away),
        Mode::Busy => Some(Status::Busy),
        Mode::Automatic => None,
    };
    if let Some(status) = manual {
        return Snapshot {
            status,
            automatic: false,
            idle_seconds: None,
            locked: None,
            source: "manual".into(),
            detail: "Set manually; message read receipts are separate.".into(),
        };
    }
    let unavailable = |detail: &str| Snapshot {
        status: Status::Unknown,
        automatic: true,
        idle_seconds: None,
        locked: None,
        source: "unavailable".into(),
        detail: detail.into(),
    };
    let (Some(observation), Some(age)) = (observation, age) else {
        return unavailable("Computer activity has not been measured yet.");
    };
    if age > STALE_AFTER {
        return unavailable("Computer activity information is stale; availability is unknown.");
    }
    let idle = observation
        .idle_seconds
        .map(|seconds| seconds.saturating_add(age.as_secs()));
    let status = if observation.locked == Some(true) || observation.screen_inactive {
        Status::Away
    } else if let Some(idle) = idle {
        if idle >= u64::from(settings.away_after_minutes) * 60 {
            Status::Away
        } else {
            Status::Active
        }
    } else {
        Status::Unknown
    };
    Snapshot {
        status,
        automatic: true,
        idle_seconds: idle,
        locked: observation.locked,
        source: observation.source.into(),
        detail: match status {
            Status::Active => "Recent input on this computer; this does not mean messages were read.",
            Status::Away => "Computer idle or screen inactive; replies may come later.",
            _ => "This desktop does not provide usable activity information; availability is unknown.",
        }
        .into(),
    }
}

#[cfg(target_os = "linux")]
fn query_host() -> Observation {
    let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|s| s == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some();
    // Xwayland idle describes only Xwayland clients, not the whole desktop.
    let mut observation = if !wayland {
        x11_idle().unwrap_or_else(Observation::unavailable)
    } else {
        Observation::unavailable()
    };
    if observation.idle_seconds.is_none()
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        && let Some(output) = bounded_output(
            "gdbus",
            &[
                "call",
                "--session",
                "--dest",
                "org.gnome.Mutter.IdleMonitor",
                "--object-path",
                "/org/gnome/Mutter/IdleMonitor/Core",
                "--method",
                "org.gnome.Mutter.IdleMonitor.GetIdletime",
            ],
        )
        && let Some(milliseconds) = parse_number(&output)
    {
        observation.idle_seconds = Some(milliseconds / 1000);
        observation.source = "host_session_idle_monitor";
    }
    if let Ok(session) = std::env::var("XDG_SESSION_ID")
        && !session.is_empty()
        && let Some(output) = bounded_output(
            "loginctl",
            &["show-session", &session, "--property=LockedHint", "--value"],
        )
    {
        observation.locked = match output.trim() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        };
    }
    observation
}

#[cfg(target_os = "linux")]
fn x11_idle() -> Option<Observation> {
    use x11rb::{connection::Connection, protocol::screensaver::ConnectionExt};
    let display = std::env::var("DISPLAY").ok()?;
    // Avoid unbounded network-display connects. A hung local display can at
    // worst stall this one sampler thread; callers still get stale/unknown.
    if !(display.starts_with(':') || display.starts_with("unix:")) {
        return None;
    }
    let (connection, screen) = x11rb::connect(Some(&display)).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let info = connection.screensaver_query_info(root).ok()?.reply().ok()?;
    Some(Observation {
        idle_seconds: Some(u64::from(info.ms_since_user_input) / 1000),
        locked: None,
        screen_inactive: matches!(info.state, 1 | 2),
        source: "host_x11_idle",
    })
}

#[cfg(target_os = "linux")]
fn parse_number(output: &str) -> Option<u64> {
    output
        .split_whitespace()
        .find_map(|word| word.trim_matches(['(', ')', ',']).parse().ok())
}

/// Optional desktop/system helpers return tiny metadata responses. No shell,
/// installation, auto-launched private bus, or unbounded output reader is used.
#[cfg(target_os = "linux")]
fn bounded_output(program: &str, args: &[&str]) -> Option<String> {
    use std::{
        io::Read,
        os::fd::AsRawFd,
        process::{Command, Stdio},
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut output = child.stdout.take()?;
    // An inherited pipe must not hold this thread waiting for a descendant's EOF.
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        loop {
            match output.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) if bytes.len() + n <= 4096 => bytes.extend_from_slice(&chunk[..n]),
                Ok(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A short helper may write and exit between the earlier pipe
                // read and try_wait. Drain its already-queued bytes once more.
                loop {
                    match output.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) if bytes.len() + n <= 4096 => bytes.extend_from_slice(&chunk[..n]),
                        Ok(_) => return None,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => return None,
                    }
                }
                return status
                    .success()
                    .then(|| String::from_utf8(bytes).ok())
                    .flatten();
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn query_host() -> Observation {
    #[repr(C)]
    struct LastInputInfo {
        size: u32,
        time: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetLastInputInfo(info: *mut LastInputInfo) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount() -> u32;
    }
    let mut info = LastInputInfo {
        size: std::mem::size_of::<LastInputInfo>() as u32,
        time: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return Observation::unavailable();
    }
    Observation {
        idle_seconds: Some(u64::from(unsafe { GetTickCount() }.wrapping_sub(info.time)) / 1000),
        locked: None,
        screen_inactive: false,
        source: "host_windows_input_idle",
    }
}

#[cfg(target_os = "macos")]
fn query_host() -> Observation {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    let seconds = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
    if !seconds.is_finite() || seconds < 0. {
        return Observation::unavailable();
    }
    Observation {
        idle_seconds: Some(seconds as u64),
        locked: None,
        screen_inactive: false,
        source: "host_macos_input_idle",
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn query_host() -> Observation {
    Observation::unavailable()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observed(seconds: Option<u64>, locked: Option<bool>) -> Observation {
        Observation {
            idle_seconds: seconds,
            locked,
            screen_inactive: false,
            source: "test_host",
        }
    }
    #[test]
    fn automatic_presence_uses_host_idle_threshold_and_lock_not_app_activity() {
        let settings = Settings::default();
        assert_eq!(
            classify(
                &settings,
                Some(&observed(Some(299), None)),
                Some(Duration::ZERO)
            )
            .status,
            Status::Active
        );
        assert_eq!(
            classify(
                &settings,
                Some(&observed(Some(300), None)),
                Some(Duration::ZERO)
            )
            .status,
            Status::Away
        );
        assert_eq!(
            classify(
                &settings,
                Some(&observed(Some(0), Some(true))),
                Some(Duration::ZERO)
            )
            .status,
            Status::Away
        );
        let sample = observed(Some(600), Some(false));
        for _ in 0..20 {
            // No heartbeat, model token, tool call or UI poll can reset idle.
            assert_eq!(
                classify(&settings, Some(&sample), Some(Duration::ZERO)).status,
                Status::Away
            );
        }
    }
    #[test]
    fn stale_or_unavailable_activity_stays_unknown() {
        let settings = Settings::default();
        assert_eq!(classify(&settings, None, None).status, Status::Unknown);
        assert_eq!(
            classify(
                &settings,
                Some(&observed(None, Some(false))),
                Some(Duration::ZERO)
            )
            .status,
            Status::Unknown
        );
        assert_eq!(
            classify(
                &settings,
                Some(&observed(Some(1), None)),
                Some(STALE_AFTER + Duration::from_secs(1))
            )
            .status,
            Status::Unknown
        );
    }
    #[test]
    fn overrides_and_threshold_changes_apply_without_new_input_or_sampling() {
        for (mode, status) in [
            (Mode::Active, Status::Active),
            (Mode::Away, Status::Away),
            (Mode::Busy, Status::Busy),
        ] {
            let settings = Settings {
                mode,
                ..Default::default()
            };
            let snapshot = classify(&settings, None, None);
            assert_eq!(snapshot.status, status);
            assert!(!snapshot.automatic);
            assert_eq!(snapshot.idle_seconds, None);
        }
        let settings = Settings {
            away_after_minutes: 20,
            ..Default::default()
        };
        assert_eq!(
            classify(
                &settings,
                Some(&observed(Some(600), None)),
                Some(Duration::ZERO)
            )
            .status,
            Status::Active
        );
        assert!(
            Settings {
                away_after_minutes: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn dbus_numbers_do_not_confuse_the_uint64_type_name_for_the_value() {
        assert_eq!(parse_number("(uint64 123456,)"), Some(123456));
        assert_eq!(parse_number("(uint32 0,)"), Some(0));
        assert_eq!(parse_number("(false,)"), None);
        assert_eq!(parse_number("unavailable"), None);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn optional_desktop_queries_are_bounded_and_collect_only_small_responses() {
        assert_eq!(
            bounded_output("sh", &["-c", "printf '(uint64 1000,)' "]),
            Some("(uint64 1000,)".into())
        );
        let started = Instant::now();
        assert!(bounded_output("sh", &["-c", "exec sleep 10"]).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
