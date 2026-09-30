//! Foreground terminal titles distinguish project windows in a crowded taskbar.
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
static ACTIVE: AtomicBool = AtomicBool::new(false);
pub struct Guard;
impl Guard {
    pub fn enter(project: &Path) -> Self {
        if io::stdout().is_terminal() {
            let name = project
                .file_name()
                .unwrap_or(project.as_os_str())
                .to_string_lossy();
            let name: String = name.chars().filter(|c| !c.is_control()).take(120).collect();
            let title = if name.is_empty() {
                "Chuggin".to_owned()
            } else {
                format!("{name} · Chuggin")
            };
            let mut out = io::stdout().lock();
            // xterm-compatible push/pop preserves the caller's title without a
            // terminal query that could consume user input or block startup.
            if write!(out, "\x1b[22;0t")
                .and_then(|_| crossterm::execute!(out, crossterm::terminal::SetTitle(title)))
                .is_ok()
            {
                ACTIVE.store(true, Ordering::SeqCst);
            }
        }
        Self
    }
}
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout().lock();
        let _ = write!(out, "\x1b[23;0t").and_then(|_| out.flush());
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}
