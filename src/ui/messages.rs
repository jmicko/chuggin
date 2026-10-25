//! Human mailbox view. Merely displaying an unfocused/away terminal is not a read receipt.
use super::*;
use crate::messenger::{self, Message, Sender, Status};

#[derive(Default)]
pub(super) struct MessagesPanel {
    pub status: Option<Status>,
    messages: Vec<Message>,
    before: Option<u64>,
    older_before: Option<u64>,
    has_more: bool,
    pub draft: String,
    cursor: usize,
    pub error: String,
    reply_to: Option<u64>,
    operation: Option<String>,
    last_poll: Option<Instant>,
    row_ids: Vec<Option<u64>>,
    pub visible: Vec<u64>,
}
impl MessagesPanel {
    pub fn poll(&mut self, dir: &Path, force: bool) {
        if !force
            && self
                .last_poll
                .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        self.last_poll = Some(Instant::now());
        let result = (|| -> Result<()> {
            let status = messenger::status(dir)?;
            if force
                || self
                    .status
                    .as_ref()
                    .is_none_or(|old| old.revision != status.revision)
            {
                let page = messenger::human_page(dir, self.before, 30)?;
                self.messages = page.messages;
                self.older_before = page.older_before;
                self.has_more = page.has_more;
                self.status = Some(page.status);
            } else {
                self.status = Some(status);
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.error = format!("{e:#}");
        }
    }
    pub fn acknowledge(
        &mut self,
        dir: &Path,
        focused: bool,
        recently_interacted: bool,
        presence: crate::presence::Status,
    ) {
        if !should_acknowledge(focused, recently_interacted, presence) {
            return;
        }
        let ids: Vec<_> = self
            .messages
            .iter()
            .filter(|m| {
                m.sender == Sender::Model && m.read_unix.is_none() && self.visible.contains(&m.id)
            })
            .map(|m| m.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        match messenger::mark_human_read(dir, &ids) {
            Ok(status) => {
                self.status = Some(status);
                self.poll(dir, true);
            }
            Err(e) => self.error = format!("{e:#}"),
        }
    }
    pub fn append(&mut self, value: &str) {
        let text = clean(value);
        if self.draft.len() + text.len() <= 8000 {
            self.draft.insert_str(self.cursor, &text);
            self.cursor += text.len();
            self.operation = None;
        } else {
            self.error = "Messages are limited to 8,000 bytes.".into();
        }
    }
    pub fn key(&mut self, k: event::KeyEvent, dir: &Path) -> Result<bool> {
        match k.code {
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => return Ok(false),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(false),
            KeyCode::Left | KeyCode::Right | KeyCode::Delete | KeyCode::Backspace => {
                let mut editor = Editor {
                    value: std::mem::take(&mut self.draft),
                    cursor: self.cursor,
                    fresh: false,
                };
                editor.key(k.code);
                self.cursor = editor.cursor;
                self.draft = editor.value;
                if matches!(k.code, KeyCode::Delete | KeyCode::Backspace) {
                    self.operation = None;
                }
            }
            KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => self.cursor = 0,
            KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.draft.len()
            }
            KeyCode::F(2) => {
                if self.has_more {
                    self.before = self.older_before;
                    self.poll(dir, true);
                }
            }
            KeyCode::F(3) => {
                self.before = None;
                self.poll(dir, true);
            }
            KeyCode::F(5) => {
                crate::setup::presence_menu()?;
            }
            KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                let choices: Vec<_> = self
                    .messages
                    .iter()
                    .filter(|m| m.sender == Sender::Model)
                    .rev()
                    .collect();
                let mut labels: Vec<String> = choices
                    .iter()
                    .map(|m| {
                        format!(
                            "Reply to #{}: {}",
                            m.id,
                            crate::project::excerpt(&clean(&m.body).replace('\n', " "), 90)
                        )
                    })
                    .collect();
                labels.push("New message · no specific reply".into());
                if let Some(index) = super::select("Reply to a message", &labels, 0)? {
                    self.reply_to = choices.get(index).map(|m| m.id);
                    self.operation = None;
                }
            }
            KeyCode::Enter if k.modifiers.contains(KeyModifiers::ALT) => self.append("\n"),
            KeyCode::Enter if !self.draft.trim().is_empty() => {
                let operation = self.operation.get_or_insert_with(|| {
                    format!(
                        "tui-{}-{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    )
                });
                match messenger::human_send_with_id(dir, &self.draft, self.reply_to, operation) {
                    Ok(_) => {
                        self.draft.clear();
                        self.cursor = 0;
                        self.reply_to = None;
                        self.operation = None;
                        self.error.clear();
                        self.before = None;
                        self.poll(dir, true);
                    }
                    Err(e) => self.error = format!("{e:#}"),
                }
            }
            KeyCode::Esc => {
                self.draft.clear();
                self.cursor = 0;
                self.reply_to = None;
                self.operation = None;
                self.error.clear();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.draft.clear();
                self.cursor = 0;
                self.operation = None;
            }
            KeyCode::Char(ch)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.append(&ch.to_string())
            }
            _ => {}
        }
        Ok(true)
    }
    pub fn composer_title(&self) -> String {
        match self.reply_to {
            Some(id) => format!("Reply to #{id} · Ctrl+R change · Enter send"),
            None => "Message to the loop · Enter send · Ctrl+R reply".into(),
        }
    }
    pub fn composer(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let text = format!(
            "› {}▏{}",
            &self.draft[..self.cursor],
            &self.draft[self.cursor..]
        );
        let rows = textwrap::wrap(&text, width.max(1));
        let prefix = format!("› {}▏", &self.draft[..self.cursor]);
        let cursor_row = textwrap::wrap(&prefix, width.max(1))
            .len()
            .saturating_sub(1);
        rows.into_iter()
            .skip(cursor_row.saturating_sub(height.saturating_sub(1)))
            .take(height)
            .map(|row| Line::from(row.into_owned()))
            .collect()
    }
    pub fn lines(&mut self, width: usize) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        self.row_ids.clear();
        let mut add = |line: Line<'static>, id| {
            rows.push(line);
            self.row_ids.push(id);
        };
        add(Line::from("MESSAGES WITH THE LOOP").fg(ACCENT).bold(), None);
        add(
            Line::from(
                "Messages are fetched through a tool. Sending does not start or resume work.",
            )
            .fg(MUTED),
            None,
        );
        add(
            Line::from("Read receipts mean fetched/displayed; availability is separate.").fg(MUTED),
            None,
        );
        add(
            Line::from(format!(
                "{}F3 latest · Ctrl+R reply · F5 availability",
                if self.has_more { "F2 older · " } else { "" }
            ))
            .fg(MUTED),
            None,
        );
        if self.before.is_some() {
            add(
                Line::from("Viewing an older page · F3 returns to newest messages").fg(GOLD),
                None,
            );
        }
        add(Line::from(""), None);
        if self.messages.is_empty() {
            add(
                Line::from("No messages yet. You can leave a message below.").fg(MUTED),
                None,
            );
        }
        for m in &self.messages {
            let color = if m.sender == Sender::Human {
                CYAN
            } else {
                ACCENT
            };
            let receipt = match (m.sender, m.read_unix.is_some()) {
                (Sender::Human, true) => "fetched by model",
                (Sender::Human, false) => "waiting for model",
                (Sender::Model, true) => "read",
                (Sender::Model, false) => "new",
            };
            let time = chrono::DateTime::from_timestamp(m.created_unix as i64, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%b %d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let title = format!(
                "{} · #{} · {time} · {receipt}{}",
                if m.sender == Sender::Human {
                    "You"
                } else {
                    "Model"
                },
                m.id,
                m.reply_to
                    .map(|id| format!(" · reply to #{id}"))
                    .unwrap_or_default()
            );
            for row in textwrap::wrap(&title, width.max(1)) {
                add(Line::from(row.into_owned()).fg(color).bold(), None);
            }
            let body = clean(&m.body);
            let wrapped = textwrap::wrap(&body, width.max(1));
            let count = wrapped.len();
            for (index, row) in wrapped.into_iter().enumerate() {
                add(
                    Line::from(row.into_owned()),
                    (index + 1 == count).then_some(m.id),
                );
            }
            add(Line::from(""), None);
        }
        if !self.error.is_empty() {
            for row in textwrap::wrap(&self.error, width.max(1)) {
                add(Line::from(row.into_owned()).fg(GOLD), None);
            }
        }
        rows
    }
    pub fn viewport(&mut self, start: usize, rows: usize) {
        self.visible = self
            .row_ids
            .iter()
            .skip(start)
            .take(rows)
            .flatten()
            .copied()
            .collect();
        self.visible.sort_unstable();
        self.visible.dedup();
    }
}
fn should_acknowledge(focused: bool, interacted: bool, presence: crate::presence::Status) -> bool {
    focused && (interacted || presence == crate::presence::Status::Active)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn message_editor_keeps_unicode_cursor_and_wrapped_draft_visible() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = MessagesPanel::default();
        panel.append("Hello 🌱world");
        panel
            .key(
                event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                dir.path(),
            )
            .unwrap();
        panel.append("A ");
        assert_eq!(panel.draft, "A Hello 🌱world");
        panel
            .key(
                event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                dir.path(),
            )
            .unwrap();
        for _ in 0..6 {
            panel
                .key(
                    event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                    dir.path(),
                )
                .unwrap();
        }
        panel
            .key(
                event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                dir.path(),
            )
            .unwrap();
        assert_eq!(panel.draft, "A Hello world");
        panel
            .key(
                event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                dir.path(),
            )
            .unwrap();
        panel.append(&" long draft".repeat(20));
        assert!(
            panel
                .composer(30, 2)
                .iter()
                .any(|l| l.to_string().contains('▏'))
        );
        panel
            .key(
                event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                dir.path(),
            )
            .unwrap();
        panel.append("New message");
        assert_eq!(panel.draft, "New message");
    }
    #[test]
    fn background_or_away_view_does_not_mark_read() {
        use crate::presence::Status::*;
        assert!(!should_acknowledge(false, true, Active));
        assert!(!should_acknowledge(true, false, Away));
        assert!(!should_acknowledge(true, false, Unknown));
        assert!(should_acknowledge(true, true, Away));
        assert!(should_acknowledge(true, false, Active));
    }
    #[test]
    fn receipt_requires_a_visible_model_message() {
        let dir = tempfile::tempdir().unwrap();
        messenger::model_send(dir.path(), &serde_json::json!({"body":"Can you see me?"})).unwrap();
        let mut panel = MessagesPanel::default();
        panel.poll(dir.path(), true);
        panel.lines(80);
        panel.viewport(0, 4);
        panel.acknowledge(dir.path(), true, true, crate::presence::Status::Active);
        assert_eq!(messenger::status(dir.path()).unwrap().unread_to_human, 1);
        panel.viewport(0, 100);
        panel.acknowledge(dir.path(), false, true, crate::presence::Status::Active);
        assert_eq!(messenger::status(dir.path()).unwrap().unread_to_human, 1);
        panel.acknowledge(dir.path(), true, true, crate::presence::Status::Active);
        assert_eq!(messenger::status(dir.path()).unwrap().unread_to_human, 0);
    }
    #[test]
    fn sending_is_local_and_keeps_a_persistent_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut panel = MessagesPanel::default();
        panel.append("Hello loop");
        panel
            .key(
                event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                dir.path(),
            )
            .unwrap();
        assert!(panel.draft.is_empty());
        assert_eq!(messenger::status(dir.path()).unwrap().unread_to_model, 1);
        assert!(!dir.path().join("conversation.json").exists());
    }
}
