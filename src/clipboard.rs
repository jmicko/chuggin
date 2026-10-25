//! Human-initiated clipboard writes. Keep the native owner alive while the TUI
//! is open; never read clipboard contents or expose this facility to model tools.
use anyhow::{Result, ensure};
use std::{
    io::{self, IsTerminal},
    sync::{LazyLock, Mutex},
};

static OWNER: LazyLock<Mutex<Option<arboard::Clipboard>>> = LazyLock::new(Mutex::default);

#[derive(Debug, PartialEq)]
pub enum CopyResult {
    Copied,
    TerminalRequested,
}

pub fn copy(text: &str) -> Result<CopyResult> {
    ensure!(
        !text.is_empty() && text.len() <= 16_384,
        "Clipboard text is empty or too long"
    );
    let native = (|| -> Result<()> {
        let mut owner = OWNER
            .lock()
            .map_err(|_| anyhow::anyhow!("Clipboard owner unavailable"))?;
        if owner.is_none() {
            *owner = Some(arboard::Clipboard::new()?);
        }
        if let Err(e) = owner.as_mut().unwrap().set_text(text.to_owned()) {
            *owner = None;
            return Err(e.into());
        }
        Ok(())
    })();
    if native.is_ok() {
        return Ok(CopyResult::Copied);
    }
    ensure!(
        io::stdout().is_terminal(),
        "Desktop clipboard unavailable: {}",
        native.unwrap_err()
    );
    // Useful over SSH or on terminals without a locally accessible desktop.
    // OSC 52 has no success acknowledgement, so do not claim it copied.
    crossterm::execute!(
        io::stdout().lock(),
        crossterm::clipboard::CopyToClipboard::to_clipboard_from(text)
    )?;
    Ok(CopyResult::TerminalRequested)
}
