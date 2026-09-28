//! Full-screen observation and shared interactive surfaces.
use crate::{
    events::{self, Event},
    runner,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as Input, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
use ratatui::{prelude::*, widgets::*};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs, io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const BG: Color = Color::Rgb(15, 18, 26);
const PANEL: Color = Color::Rgb(23, 28, 39);
const FG: Color = Color::Rgb(220, 226, 238);
const MUTED: Color = Color::Rgb(139, 151, 173);
const ACCENT: Color = Color::Rgb(182, 158, 255);
const CYAN: Color = Color::Rgb(112, 221, 214);
const GREEN: Color = Color::Rgb(153, 220, 157);
const GOLD: Color = Color::Rgb(242, 199, 125);
thread_local! { static TERMINAL: RefCell<Option<ratatui::DefaultTerminal>> = const { RefCell::new(None) }; static NOTES: RefCell<String> = const { RefCell::new(String::new()) }; }
static ACTIVE: AtomicBool = AtomicBool::new(false);
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}
pub struct Screen;
impl Screen {
    pub fn enter() -> Result<Self> {
        let terminal = match ratatui::try_init() {
            Ok(t) => t,
            Err(e) => {
                ratatui::restore();
                return Err(e.into());
            }
        };
        ACTIVE.store(true, Ordering::SeqCst);
        TERMINAL.with(|t| *t.borrow_mut() = Some(terminal));
        let guard = Self;
        execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        Ok(guard)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
        TERMINAL.with(|t| *t.borrow_mut() = None);
    }
}
fn draw(f: impl FnOnce(&mut Frame)) -> Result<()> {
    TERMINAL.with(|t| -> Result<()> {
        t.borrow_mut()
            .as_mut()
            .context("Terminal is not initialized")?
            .draw(f)?;
        Ok(())
    })
}
fn base(f: &mut Frame) {
    f.render_widget(
        Block::default().style(Style::default().bg(BG).fg(FG)),
        f.area(),
    );
}
fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .padding(Padding::horizontal(1))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(59, 68, 88)))
        .title(Line::from(format!(" {title} ")).fg(ACCENT))
        .style(Style::default().bg(PANEL).fg(FG))
}
fn p(text: impl Into<Text<'static>>) -> Paragraph<'static> {
    Paragraph::new(text)
        .style(Style::default().fg(FG))
        .wrap(Wrap { trim: false })
}
pub(crate) fn clean(s: &str) -> String {
    let mut result = String::new();
    let mut escape = false;
    let mut csi = false;
    for ch in s.chars() {
        if escape {
            if ch == '[' {
                csi = true;
                continue;
            }
            if !csi || ('@'..='~').contains(&ch) {
                escape = false;
                csi = false;
            }
            continue;
        }
        if ch == '\x1b' {
            escape = true;
            continue;
        }
        if !ch.is_control() || ch == '\n' {
            result.push(ch);
        } else if ch == '\t' {
            result.push_str("    ");
        }
    }
    result
}
pub fn notice(text: String) {
    if !active() {
        println!("{text}");
        return;
    }
    NOTES.with(|n| {
        let mut n = n.borrow_mut();
        n.push_str(&clean(&text));
        n.push('\n');
        if n.len() > 24000 {
            let cut = n
                .char_indices()
                .find(|(i, _)| *i >= n.len() - 18000)
                .map(|(i, _)| i)
                .unwrap_or(0);
            n.drain(..cut);
        }
    });
    let _ = draw(|f| {
        base(f);
        let a = f.area().inner(Margin::new(3, 2));
        f.render_widget(p(text).block(panel("CHUGGIN · Working")), a);
    });
}
pub fn clear_notes() {
    NOTES.with(|n| n.borrow_mut().clear());
}
fn notes() -> String {
    NOTES.with(|n| n.borrow().clone())
}
fn input() -> Result<Option<Input>> {
    if event::poll(Duration::from_millis(100))? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}
fn pressed(e: &Input) -> Option<event::KeyEvent> {
    match e {
        Input::Key(k) if k.kind != KeyEventKind::Release => Some(*k),
        _ => None,
    }
}
fn cancelled(k: event::KeyEvent) -> bool {
    k.code == KeyCode::Esc
        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
}

const LOGO: &str = " ██████╗██╗  ██╗██╗   ██╗ ██████╗  ██████╗ ██╗███╗   ██╗\n██╔════╝██║  ██║██║   ██║██╔════╝ ██╔════╝ ██║████╗  ██║\n██║     ███████║██║   ██║██║  ███╗██║  ███╗██║██╔██╗ ██║\n██║     ██╔══██║██║   ██║██║   ██║██║   ██║██║██║╚██╗██║\n╚██████╗██║  ██║╚██████╔╝╚██████╔╝╚██████╔╝██║██║ ╚████║\n ╚═════╝╚═╝  ╚═╝ ╚═════╝  ╚═════╝  ╚═════╝ ╚═╝╚═╝  ╚═══╝";
fn splash(
    f: &mut Frame,
    items: &[String],
    selected: usize,
    project: &str,
    model: &str,
    status: &str,
) {
    base(f);
    let area = f.area().inner(Margin::new(3, 1));
    let rows = Layout::vertical([
        Constraint::Length(if area.height >= 28 { 10 } else { 4 }),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .split(area);
    let logo = if area.height >= 28 && area.width >= 58 {
        LOGO
    } else {
        "C H U G G I N"
    };
    let logo_width = Text::raw(logo).width().min(rows[0].width as usize) as u16;
    let logo_area = Rect::new(
        rows[0].x + rows[0].width.saturating_sub(logo_width) / 2,
        rows[0].y,
        logo_width,
        rows[0].height,
    );
    f.render_widget(Paragraph::new(logo).fg(ACCENT), logo_area);
    if rows[0].height > 7 {
        f.render_widget(
            Paragraph::new("Just chuggin along...")
                .fg(MUTED)
                .alignment(Alignment::Center),
            Rect::new(rows[0].x, rows[0].y + 8, rows[0].width, 1),
        );
    }
    let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(3)
        .split(rows[1]);
    let descriptions = [
        "Continue the next focused cycle",
        "The ambition guiding every cycle",
        "Saved checkpoints and check results",
        if items.first().is_some_and(|item| item == "Resume project") {
            "Change the model for this project"
        } else {
            "Choose a shared model default"
        },
        "Defaults for projects without overrides",
        "Return to your shell",
    ];
    let entries: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            ListItem::new(vec![
                Line::from(item.clone()).bold(),
                Line::from(match item.as_str() {
                    "Run duration" => "Finish the current cycle when time is up",
                    "Quit" => "Return to your shell",
                    _ => descriptions.get(i).copied().unwrap_or(""),
                })
                .fg(MUTED),
            ])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected));
    f.render_stateful_widget(
        List::new(entries)
            .block(panel("Start here"))
            .highlight_symbol(" › ")
            .highlight_style(Style::default().bg(Color::Rgb(47, 42, 70)).fg(ACCENT)),
        columns[0],
        &mut state,
    );
    let detail = vec![
        Line::from("PROJECT").fg(CYAN).bold(),
        Line::from(project.to_owned()),
        Line::from(""),
        Line::from("MODEL").fg(CYAN).bold(),
        Line::from(model.to_owned()),
        Line::from(""),
        Line::from(status.to_owned()).fg(MUTED),
        Line::from(""),
        Line::from("Fresh context. Persistent progress.").fg(ACCENT),
        Line::from(format!("v{} · Rust", env!("CARGO_PKG_VERSION"))).fg(MUTED),
    ];
    f.render_widget(p(Text::from(detail)).block(panel("Workspace")), columns[1]);
    f.render_widget(
        Paragraph::new("↑ ↓  navigate     Enter  select     Esc / q  quit")
            .fg(MUTED)
            .alignment(Alignment::Center),
        rows[2],
    );
}
pub fn home_select(
    items: &[String],
    project: &str,
    model: &str,
    status: &str,
) -> Result<Option<usize>> {
    clear_notes();
    let mut index = 0;
    loop {
        draw(|f| splash(f, items, index, project, model, status))?;
        if let Some(e) = input()?
            && let Some(k) = pressed(&e)
        {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => index = (index + items.len() - 1) % items.len(),
                KeyCode::Down | KeyCode::Char('j') => index = (index + 1) % items.len(),
                KeyCode::Enter => return Ok(Some(index)),
                KeyCode::Char('q') => return Ok(None),
                _ if cancelled(k) => return Ok(None),
                _ => {}
            }
        }
    }
}
pub fn select(title: &str, items: &[String], default: usize) -> Result<Option<usize>> {
    let mut index = default.min(items.len().saturating_sub(1));
    let mut offset = 0u16;
    loop {
        draw(|f| {
            base(f);
            let a = f.area().inner(Margin::new(3, 1));
            let r = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length((items.len() as u16 + 2).min(12)),
                Constraint::Length(1),
            ])
            .split(a);
            f.render_widget(
                Paragraph::new("CHUGGIN  /  ".to_owned() + title)
                    .fg(ACCENT)
                    .bold(),
                r[0],
            );
            f.render_widget(
                p(notes())
                    .scroll((offset, 0))
                    .block(panel("Details · PgUp / PgDn to scroll")),
                r[1],
            );
            let mut s = ListState::default().with_selected(Some(index));
            f.render_stateful_widget(
                List::new(items.iter().map(|s| ListItem::new(s.clone())))
                    .block(panel(title))
                    .highlight_symbol(" › ")
                    .highlight_style(Style::default().bg(Color::Rgb(47, 42, 70)).fg(ACCENT)),
                r[2],
                &mut s,
            );
            f.render_widget(
                Paragraph::new("↑ ↓ select · Enter confirm · Esc back").fg(MUTED),
                r[3],
            );
        })?;
        if let Some(e) = input()?
            && let Some(k) = pressed(&e)
        {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => index = index.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    index = (index + 1).min(items.len().saturating_sub(1))
                }
                KeyCode::PageUp => offset = offset.saturating_sub(8),
                KeyCode::PageDown => {
                    offset = offset.saturating_add(8).min(notes().len().min(6000) as u16)
                }
                KeyCode::Enter => return Ok(Some(index)),
                _ if cancelled(k) => return Ok(None),
                _ => {}
            }
        }
    }
}
struct Editor {
    value: String,
    cursor: usize,
    fresh: bool,
}
impl Editor {
    fn new(value: &str) -> Self {
        Self {
            value: value.into(),
            cursor: value.len(),
            fresh: true,
        }
    }
    fn insert(&mut self, text: &str) {
        if self.fresh {
            self.value.clear();
            self.cursor = 0;
            self.fresh = false;
        }
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    fn key(&mut self, code: KeyCode) {
        self.fresh = false;
        match code {
            KeyCode::Left => {
                self.cursor = self.value[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            }
            KeyCode::Right => {
                self.cursor += self.value[self.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0)
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Backspace => {
                let before = self.cursor;
                self.key(KeyCode::Left);
                self.value.drain(self.cursor..before);
            }
            KeyCode::Delete => {
                let after = self.cursor
                    + self.value[self.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                self.value.drain(self.cursor..after);
            }
            _ => {}
        }
    }
}
pub fn ask(label: &str, default: &str) -> Result<String> {
    ask_field(label, default, false)
}
pub fn ask_secret(label: &str) -> Result<String> {
    ask_field(label, "", true)
}
fn ask_field(label: &str, default: &str, secret: bool) -> Result<String> {
    let mut edit = Editor::new(default);
    loop {
        draw(|f| {
            base(f);
            let r = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(7),
                Constraint::Length(2),
            ])
            .split(f.area().inner(Margin::new(3, 1)));
            f.render_widget(
                Paragraph::new("CHUGGIN  /  ".to_owned() + label)
                    .fg(ACCENT)
                    .bold(),
                r[0],
            );
            f.render_widget(p(notes()).block(panel("Context")), r[1]);
            let mut visible = if secret {
                "•".repeat(edit.value.chars().count())
            } else {
                edit.value.clone()
            };
            let cursor = if secret {
                edit.value[..edit.cursor].chars().count() * '•'.len_utf8()
            } else {
                edit.cursor
            };
            visible.insert(cursor, '▏');
            let width = r[2].width.saturating_sub(4).max(1) as usize;
            let lines: Vec<String> = visible
                .split('\n')
                .flat_map(|l| {
                    if l.is_empty() {
                        vec![String::new()]
                    } else {
                        textwrap::wrap(l, width)
                            .into_iter()
                            .map(|s| s.into_owned())
                            .collect()
                    }
                })
                .collect();
            let row = lines.iter().position(|l| l.contains('▏')).unwrap_or(0);
            let top = row.saturating_sub(r[2].height.saturating_sub(3) as usize);
            f.render_widget(
                p(lines.into_iter().skip(top).collect::<Vec<_>>().join("\n"))
                    .block(panel(label))
                    .fg(CYAN),
                r[2],
            );
            f.render_widget(
                Paragraph::new(
                    "← → edit · Enter save · Alt+Enter newline · Ctrl+U clear · Esc back",
                )
                .fg(MUTED),
                r[3],
            );
        })?;
        if let Some(e) = input()? {
            if let Input::Paste(s) = &e {
                edit.insert(&clean(s));
            }
            if let Some(k) = pressed(&e) {
                match k.code {
                    _ if cancelled(k) => anyhow::bail!("Returned without saving this field."),
                    KeyCode::Enter if k.modifiers.contains(KeyModifiers::ALT) => edit.insert("\n"),
                    KeyCode::Enter if !edit.value.trim().is_empty() => {
                        return Ok(edit.value.trim().into());
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        edit = Editor::new("");
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        edit.insert(&c.to_string())
                    }
                    code => edit.key(code),
                }
            }
        }
    }
}

pub fn show(title: &str, text: &str) -> Result<()> {
    clear_notes();
    notice(text.into());
    select(title, &["Back".into()], 0)?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Activity,
    Model,
    Check,
}
struct Entry {
    kind: Kind,
    text: String,
    command: Option<crate::command_output::CommandOutput>,
}
struct Resource {
    previous: Option<(u64, u64)>,
    cpu: f64,
    memory: f64,
    total_gib: f64,
    rss_mib: f64,
    sampled: Instant,
}
impl Resource {
    fn new() -> Self {
        Self {
            previous: None,
            cpu: 0.,
            memory: 0.,
            total_gib: 0.,
            rss_mib: 0.,
            sampled: Instant::now() - Duration::from_secs(2),
        }
    }
    fn refresh(&mut self) {
        if self.sampled.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.sampled = Instant::now();
        if let Ok(s) = fs::read_to_string("/proc/stat") {
            let nums: Vec<u64> = s
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .skip(1)
                .take(8)
                .filter_map(|n| n.parse().ok())
                .collect();
            let total: u64 = nums.iter().sum();
            let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
            if let Some((pt, pi)) = self.previous {
                let dt: u64 = total.saturating_sub(pt);
                if dt > 0 {
                    self.cpu = 100. * (1. - idle.saturating_sub(pi) as f64 / dt as f64);
                }
            }
            self.previous = Some((total, idle));
        }
        if let Ok(s) = fs::read_to_string("/proc/meminfo") {
            let get = |key: &str| {
                s.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.)
            };
            let total = get("MemTotal:");
            if total > 0. {
                self.memory = 100. * (1. - get("MemAvailable:") / total);
                self.total_gib = total / 1048576.;
            }
        }
        if let Ok(s) = fs::read_to_string("/proc/self/status") {
            self.rss_mib = s
                .lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.)
                / 1024.;
        }
    }
}
pub(crate) fn outcome_label(disposition: &str) -> &str {
    match disposition {
        "checkpoint" => "Saved · checks passed",
        "checkpoint/checks-failing" => "Saved · checks failing",
        "checkpoint/unverified" => "Saved · unverified",
        "unchanged" => "No file changes",
        historical => historical,
    }
}
struct Dashboard {
    workspace: String,
    workspace_branch: String,
    workspace_head: String,
    recovery: String,
    commit_pending: Option<String>,
    controls: Arc<crate::run_control::RunControl>,
    entries: VecDeque<Entry>,
    phase: String,
    task: String,
    cycle: u64,
    calls: u64,
    prompt: u64,
    generated: u64,
    speed: f64,
    request_active: bool,
    provider_wait: Option<u64>,
    checkpoints: u64,
    completed_cycles: u64,
    started: Instant,
    phase_started: Instant,
    last_activity: Instant,
    resources: Resource,
    tab: usize,
    active_model: String,
    settings_selected: usize,
    settings_edit: Option<String>,
    settings_error: String,
    nudges: crate::nudge::Store,
    nudge_edit: Option<String>,
    nudge_error: String,
    nudge_id: Option<u64>,
    follow: bool,
    scroll: usize,
    rows: usize,
    total_rows: usize,
    filter: String,
    searching: bool,
    help: bool,
    finished: Option<String>,
    finished_at: Option<Instant>,
    finished_active_elapsed: Option<Duration>,
    finished_phase_elapsed: Option<Duration>,
    expanded_commands: bool,
    last_output: Instant,
    partial_model: String,
    partial_check: String,
    command_status: Option<(String, u64, u64, Instant)>,
    history: VecDeque<String>,
    last_check: Option<bool>,
    last_passing_checkpoint: Option<String>,
    dropped: u64,
}
impl Dashboard {
    fn new(config: &runner::Config) -> Self {
        let mut d = Self {
            workspace: config.repo.display().to_string(),
            workspace_branch: String::new(),
            workspace_head: String::new(),
            recovery: String::new(),
            commit_pending: None,
            controls: Arc::default(),
            entries: VecDeque::new(),
            phase: "Ready".into(),
            task: "Waiting for the next task".into(),
            cycle: 0,
            calls: 0,
            prompt: 0,
            generated: 0,
            speed: 0.,
            request_active: false,
            provider_wait: None,
            checkpoints: 0,
            completed_cycles: 0,
            started: Instant::now(),
            phase_started: Instant::now(),
            last_activity: Instant::now(),
            resources: Resource::new(),
            tab: 0,
            active_model: config.model.clone(),
            settings_selected: 0,
            settings_edit: None,
            settings_error: String::new(),
            nudges: crate::nudge::Store::default(),
            nudge_edit: None,
            nudge_error: String::new(),
            nudge_id: None,
            follow: true,
            scroll: 0,
            rows: 1,
            total_rows: 0,
            filter: String::new(),
            searching: false,
            help: false,
            finished: None,
            finished_at: None,
            finished_active_elapsed: None,
            finished_phase_elapsed: None,
            expanded_commands: false,
            last_output: Instant::now(),
            partial_model: String::new(),
            partial_check: String::new(),
            command_status: None,
            history: VecDeque::new(),
            last_check: None,
            last_passing_checkpoint: None,
            dropped: 0,
        };
        if let Ok(bytes) = fs::read(config.state_dir.join("state.json"))
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            d.cycle = v["cycle"].as_u64().unwrap_or(0);
            d.workspace_branch = v["working_branch"].as_str().unwrap_or("").into();
            d.workspace_head = v["branch_head"].as_str().unwrap_or("").into();
            d.recovery = v["working_ref"].as_str().unwrap_or("").into();
            d.commit_pending = v["commit_pending"].as_str().map(str::to_owned);
            d.last_passing_checkpoint = v["last_checks_passed_ref"]
                .as_str()
                .filter(|reference| !reference.is_empty())
                .map(|reference| reference.chars().take(8).collect());
            if let Some(recent) = v["recent"].as_array() {
                for o in recent.iter().rev().take(5) {
                    d.history.push_back(format!(
                        "#{}  {}\n{}",
                        o["cycle"],
                        outcome_label(o["disposition"].as_str().unwrap_or("")),
                        o["task"].as_str().unwrap_or("")
                    ));
                }
            }
        }
        d.restore_history(&config.state_dir);
        d.push(
            Kind::Activity,
            "── New session · full diagnostic history remains in .chuggin/ ──".into(),
        );
        d
    }
    fn restore_history(&mut self, dir: &Path) {
        let path = dir.join("conversation.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                self.push(Kind::Activity, format!("Saved history unavailable: {e}"));
                return;
            }
        };
        let saved: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(saved) => saved,
            Err(e) => {
                self.push(
                    Kind::Activity,
                    format!("Saved history could not be displayed: {e}"),
                );
                return;
            }
        };
        let Some(messages) = saved["messages"].as_array() else {
            return;
        };
        self.push(
            Kind::Activity,
            "── Recent saved conversation · model messages and tool activity ──".into(),
        );
        for message in messages.iter().skip(messages.len().saturating_sub(200)) {
            if message["role"] == "assistant" {
                for field in ["thinking", "content"] {
                    if let Some(text) = message[field].as_str() {
                        // Only a display tail: never change the saved conversation.
                        let start = text
                            .char_indices()
                            .rev()
                            .nth(24000)
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        if start > 0 {
                            self.push(Kind::Model, "[Earlier text omitted from display]".into());
                        }
                        self.push(Kind::Model, text[start..].into());
                    }
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    for call in calls {
                        let name = call["function"]["name"].as_str().unwrap_or("tool");
                        let args = &call["function"]["arguments"];
                        let detail = args["path"]
                            .as_str()
                            .or(args["title"].as_str())
                            .unwrap_or("");
                        self.push(Kind::Activity, format!("↳ {name} {detail}"));
                    }
                }
            }
        }
    }
    fn push(&mut self, kind: Kind, text: String) {
        for line in clean(&text).lines() {
            self.entries.push_back(Entry {
                kind,
                text: crate::project::excerpt(line, 12000),
                command: None,
            });
            if self.entries.len() > 2000 {
                self.entries.pop_front();
                self.dropped += 1;
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
    }
    fn stream(&mut self, kind: Kind, text: &str) {
        let partial = if kind == Kind::Model {
            &mut self.partial_model
        } else {
            &mut self.partial_check
        };
        partial.push_str(&clean(text));
        let split = partial.rfind('\n').map(|n| n + 1).unwrap_or(0);
        let complete: String = partial.drain(..split).collect();
        self.push(kind, complete);
        let partial = if kind == Kind::Model {
            &mut self.partial_model
        } else {
            &mut self.partial_check
        };
        if partial.len() > 8000 {
            let s = std::mem::take(partial);
            self.push(kind, s);
        }
    }
    fn flush_model(&mut self) {
        let s = std::mem::take(&mut self.partial_model);
        self.push(Kind::Model, s);
    }
    fn poll_tail(&mut self) {
        let mut budget = 65536;
        for entry in &mut self.entries {
            if let Some(command) = &mut entry.command {
                let n = command.poll(budget);
                budget = budget.saturating_sub(n);
                if n > 0 {
                    self.last_activity = Instant::now();
                    self.last_output = Instant::now();
                }
                if budget == 0 {
                    break;
                }
            }
        }
    }
    fn quiet_activity(&self) -> Option<String> {
        if self.finished.is_some()
            || self.controls.is_paused()
            || self.last_output.elapsed() < Duration::from_millis(1500)
        {
            return None;
        }
        let message = if self.controls.pause_requested() {
            "Finishing current operation to pause"
        } else if self.provider_wait.is_some() {
            "Waiting for provider"
        } else if self.request_active {
            "Waiting for response"
        } else if self.command_status.is_some()
            || self
                .entries
                .iter()
                .any(|e| e.command.as_ref().is_some_and(|c| c.completed.is_none()))
        {
            "Command running"
        } else {
            "Working"
        };
        Some(format!(
            "{} {message}",
            activity_bar(self.started.elapsed().as_millis() as u64)
        ))
    }
    fn active_elapsed(&self) -> Duration {
        self.finished_active_elapsed.unwrap_or_else(|| {
            if let Some(end) = self.finished_at {
                // The live session records its active duration when the worker finishes.
                // Keep a wall-time fallback for historical/test dashboards.
                end.saturating_duration_since(self.started)
            } else {
                self.controls.active_elapsed(self.started)
            }
        })
    }
    fn phase_elapsed(&self) -> Duration {
        self.finished_phase_elapsed.unwrap_or_else(|| {
            if let Some(end) = self.finished_at {
                end.saturating_duration_since(self.phase_started)
            } else {
                self.controls.active_elapsed(self.phase_started)
            }
        })
    }
    fn freeze_elapsed(&mut self) {
        self.finished_active_elapsed = Some(self.controls.active_elapsed(self.started));
        self.finished_phase_elapsed = Some(self.controls.active_elapsed(self.phase_started));
        self.finished_at = Some(Instant::now());
    }
    fn apply(&mut self, event: Event) {
        self.last_activity = Instant::now();
        match event {
            Event::Workspace {
                path,
                branch,
                head,
                recovery,
                pending,
            } => {
                self.workspace = path;
                self.workspace_branch = branch;
                self.workspace_head = head;
                self.recovery = recovery;
                self.commit_pending = pending;
            }
            Event::RequestModel(name) => self.active_model = name,
            Event::RequestFinished => self.request_active = false,
            Event::ProviderWait { reason, seconds } => {
                self.request_active = false;
                self.provider_wait = Some(seconds);
                self.phase = format!("Waiting for provider · {seconds}s · {reason}");
            }
            Event::Log(s) => self.push(Kind::Activity, concise_activity(&s)),
            Event::Phase(s) => {
                self.provider_wait = None;
                self.request_active = false;
                self.flush_model();
                if s == "Check" {
                    self.last_check = None;
                }
                if self.phase != s {
                    self.phase = s;
                    self.phase_started = Instant::now();
                    self.push(Kind::Activity, format!("── {} ──", self.phase));
                }
            }
            Event::Cycle(n) => {
                self.cycle = n;
                self.calls = 0;
                self.last_check = None;
            }
            Event::Task(s) => self.task = s,
            Event::Request => {
                if self.provider_wait.take().is_some() {
                    self.phase = "Work".into();
                }
                self.flush_model();
                self.calls += 1;
                self.request_active = true;
            }
            Event::Delta(s) => {
                self.last_output = Instant::now();
                self.stream(Kind::Model, &s);
            }
            Event::Metrics {
                prompt,
                generated,
                seconds,
            } => {
                self.prompt = prompt;
                self.generated += generated;
                self.speed = if seconds > 0. {
                    generated as f64 / seconds
                } else {
                    0.
                };
                self.request_active = false;
                self.flush_model();
            }
            Event::Tool(s) => {
                self.flush_model();
                self.push(Kind::Activity, format!("↳ {s}"));
            }
            Event::Check { command, path } => {
                self.poll_tail();
                self.entries.push_back(Entry {
                    kind: Kind::Check,
                    text: String::new(),
                    command: Some(crate::command_output::CommandOutput::new(command, &path)),
                });
                if self.entries.len() > 2000 {
                    self.entries.pop_front();
                    self.dropped += 1;
                }
            }
            Event::CommandStatus {
                id,
                elapsed,
                next_review,
                running,
            } => {
                self.command_status = running.then(|| (id, elapsed, next_review, Instant::now()));
            }
            Event::CheckDone(ok) => {
                self.command_status = None;
                if let Some(command) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .filter_map(|e| e.command.as_mut())
                    .find(|c| c.completed.is_none())
                {
                    command.finish(ok);
                    self.poll_tail();
                } else {
                    self.push(
                        Kind::Check,
                        if ok {
                            "✓ Command succeeded"
                        } else {
                            "× Command failed · work is kept for repair"
                        }
                        .into(),
                    );
                }
            }
            Event::ValidationDone { passed, checkpoint } => {
                self.last_check = Some(passed);
                if passed && let Some(reference) = checkpoint {
                    self.last_passing_checkpoint = Some(reference.chars().take(8).collect());
                }
                self.push(
                    Kind::Check,
                    if passed {
                        "✓ Configured checks passed"
                    } else {
                        "× Validation needs attention · work is kept for repair"
                    }
                    .into(),
                );
            }
            Event::Outcome { disposition, task } => {
                self.completed_cycles += 1;
                if disposition.starts_with("checkpoint") {
                    self.checkpoints += 1;
                }
                let label = outcome_label(&disposition);
                self.history
                    .push_front(format!("#{}  {label}\n{task}", self.cycle));
                self.history.truncate(5);
                self.push(Kind::Activity, format!("{label} · {task}"));
            }
        }
    }
    fn back(&mut self, n: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(n);
    }
    fn forward(&mut self, n: usize) {
        let bottom = self.total_rows.saturating_sub(self.rows);
        self.scroll = self.scroll.saturating_add(n).min(bottom);
        if self.scroll == bottom {
            self.follow = true;
        }
    }
    fn lines(&self, width: usize, goal: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let width = width.max(1);
        let query = self.filter.to_lowercase();
        let mut add = |kind: Kind, text: &str| {
            if text.is_empty() {
                return;
            }
            if self.tab == 1 && kind != Kind::Model || self.tab == 2 && kind != Kind::Check {
                return;
            }
            if !query.is_empty() && !text.to_lowercase().contains(&query) {
                return;
            }
            let color = if !query.is_empty() {
                GOLD
            } else {
                match kind {
                    Kind::Activity => CYAN,
                    Kind::Model => FG,
                    Kind::Check if text.contains(" failed") => GOLD,
                    Kind::Check if text.trim_start().starts_with('✓') => GREEN,
                    Kind::Check
                        if text.trim_start().starts_with('×') || text.contains(" ... FAILED") =>
                    {
                        GOLD
                    }
                    Kind::Check if text.starts_with("▸") || text.starts_with("▾") => CYAN,
                    Kind::Check => MUTED,
                }
            };
            for line in textwrap::wrap(text, width) {
                lines.push(Line::from(line.into_owned()).fg(color));
            }
        };
        if self.tab == 3 {
            let mut detail = format!(
                "WORKING FOLDER\n{}\nBranch: {}\nNormal commit: {}\nRecovery save: {}\n{}\n\nPROJECT GOAL\n{}",
                self.workspace,
                self.workspace_branch,
                self.workspace_head,
                self.recovery,
                self.commit_pending
                    .as_ref()
                    .map(|s| format!("Commit deferred: {s}"))
                    .unwrap_or_default(),
                goal
            );
            if let Some(n) = &self.nudges.active {
                detail.push_str(&format!("\n\nACTIVE NUDGE #{}\n{}", n.id, n.request));
            }
            for n in self.nudges.history.iter().rev() {
                detail.push_str(&format!(
                    "\n\nNudge #{} · {}{}\n{}\n{}\n{}",
                    n.id,
                    n.status,
                    if n.status == "completed" {
                        " (model reported)"
                    } else {
                        ""
                    },
                    n.request,
                    n.summary,
                    n.evidence
                ));
            }
            for line in clean(&detail).lines() {
                for part in textwrap::wrap(line, width) {
                    lines.push(Line::from(part.into_owned()).fg(FG));
                }
            }
        } else {
            for entry in &self.entries {
                if let Some(command) = &entry.command {
                    for line in command.display(self.expanded_commands || !query.is_empty()) {
                        add(Kind::Check, &line);
                    }
                } else {
                    add(entry.kind, &entry.text);
                }
            }
            add(Kind::Model, &self.partial_model);
            add(Kind::Check, &self.partial_check);
        }
        lines
    }
}
fn concise_activity(text: &str) -> String {
    if text.starts_with("Watchdog ")
        && let Some(start) = text.find('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[start..])
    {
        let decision = match v["verdict"].as_str() {
            Some("stalled") => "Hang detected",
            Some("productive") => "Keep running",
            Some("uncertain") => "Needs more observation",
            _ => "Assessment unavailable; keep running",
        };
        let reason = v["reason"]
            .as_str()
            .or_else(|| v["error"].as_str())
            .unwrap_or("");
        return format!(
            "Watchdog · {decision}\n{}",
            crate::project::excerpt(reason, 240)
        );
    }
    text.into()
}
fn activity_bar(milliseconds: u64) -> String {
    let step = (milliseconds / 140) % 28;
    let (position, arrow) = if step < 14 {
        (step as i32 - 3, ['=', '=', '>'])
    } else {
        (10 - (step - 14) as i32, ['<', '=', '='])
    };
    let mut cells = [' '; 10];
    for (offset, ch) in arrow.into_iter().enumerate() {
        let index = position + offset as i32;
        if (0..10).contains(&index) {
            cells[index as usize] = ch;
        }
    }
    format!("[{}]", cells.iter().collect::<String>())
}
fn duration(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
fn setting_value(c: &runner::Config, field: usize) -> String {
    match field {
        0 => c.model.clone(),
        1 => format!("{}", c.request_timeout_seconds as f64 / 60.0),
        2 => format!("{}", c.run_duration_seconds as f64 / 3600.0),
        4 => if c.allow_goal_completion { "on" } else { "off" }.into(),
        _ => c.command_review_seconds.to_string(),
    }
}
fn settings_lines(d: &Dashboard, c: &runner::Config) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("PROJECT SETTINGS").fg(ACCENT).bold(),
        Line::from("Only this project; saved automatically."),
        Line::from(""),
    ];
    for (index, label) in [
        "Model (exact installed name)",
        "Request timeout (minutes; 0 unlimited)",
        "Run duration (hours; 0 unlimited)",
        "Command first review (seconds)",
        "Allow goal completion (on/off)",
    ]
    .iter()
    .enumerate()
    {
        let value = if index == d.settings_selected {
            d.settings_edit
                .clone()
                .unwrap_or_else(|| setting_value(c, index))
        } else {
            setting_value(c, index)
        };
        lines.push(
            Line::from(format!(
                "{} {label}: {value}{}",
                if index == d.settings_selected {
                    "›"
                } else {
                    " "
                },
                if index == d.settings_selected && d.settings_edit.is_some() {
                    "▏"
                } else {
                    ""
                }
            ))
            .fg(if index == d.settings_selected {
                ACCENT
            } else {
                FG
            }),
        );
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from("↑↓ select · Enter edit/save · Ctrl+U clear · Esc cancel"),
        Line::from("Model and timeout apply to the NEXT request."),
        Line::from(format!("Current/last request model: {}", d.active_model)),
        Line::from("Timer changes apply now, measured from this run's start."),
        Line::from("An expired timer finishes the cycle; it never cancels a call."),
        Line::from("Command review changes apply when the next command starts."),
        Line::from(d.settings_error.clone()).fg(GOLD),
    ]);
    lines
}
fn render_dashboard(f: &mut Frame, d: &mut Dashboard, c: &runner::Config, stopping: bool) {
    base(f);
    let a = f.area().inner(Margin::new(1, 0));
    let paused = d.finished.is_none() && d.controls.is_paused();
    let pausing = d.finished.is_none() && d.controls.pause_requested() && !paused;
    if d.nudge_edit.is_none() && d.finished.is_some() && (a.height < 20 || a.width < 44) {
        f.render_widget(
            p(format!(
                "WORK PAUSED\nNo work is running.\n\n{}\n\nR resume · Enter / Q home",
                d.finished.as_deref().unwrap_or("")
            ))
            .fg(GOLD)
            .bold(),
            a,
        );
        return;
    }
    if a.height < 14 || a.width < 44 {
        let state = if paused {
            "PAUSED IN CYCLE · P resume"
        } else if pausing {
            "PAUSING · P cancel pause"
        } else {
            "Working · P pause after current operation"
        };
        f.render_widget(p(format!("CHUGGIN\n\nResize to at least 46 × 14 to view the dashboard.\n{state}\nT retry provider now\nCtrl+C: finish cycle; again: force stop")).fg(ACCENT),a);
        return;
    }
    let r = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(if d.nudges.active.is_some() { 5 } else { 3 }),
        Constraint::Min(4),
        Constraint::Length(2),
        Constraint::Length(if d.finished.is_some() {
            6
        } else if paused {
            4
        } else if pausing || d.provider_wait.is_some() {
            2
        } else {
            1
        }),
    ])
    .split(a);
    let state = if d.finished.is_some() {
        "PAUSED"
    } else if stopping {
        "DRAINING"
    } else if paused {
        "PAUSED IN CYCLE"
    } else if pausing {
        "PAUSING"
    } else {
        "LIVE"
    };
    let wide = f.area().width >= 100;
    let mut header = vec![Line::from(vec![
        Span::styled(" CHUGGIN ", Style::default().fg(BG).bg(ACCENT).bold()),
        Span::styled(
            format!("  {state}"),
            Style::default().fg(if stopping || paused || pausing {
                GOLD
            } else {
                CYAN
            }),
        ),
        Span::styled(
            format!(
                "  ·  {}",
                c.repo.file_name().unwrap_or_default().to_string_lossy()
            ),
            Style::default().fg(FG),
        ),
    ])];
    if !wide {
        header.push(
            Line::from(format!(
                "Overall cycle: #{} · session {} done",
                d.cycle, d.completed_cycles
            ))
            .fg(MUTED),
        );
    }
    f.render_widget(Paragraph::new(header), r[0]);
    let phases = ["Orient", "Work", "Check", "Review", "Checkpoint"];
    let mut steps = Vec::new();
    for (i, s) in phases.iter().enumerate() {
        if i > 0 {
            steps.push(Span::raw("  ›  "));
        }
        steps.push(Span::styled(
            s.to_string(),
            Style::default()
                .fg(if d.finished.is_none() && !paused && d.phase == *s {
                    BG
                } else {
                    MUTED
                })
                .bg(if d.finished.is_none() && !paused && d.phase == *s {
                    CYAN
                } else {
                    BG
                })
                .add_modifier(if d.finished.is_none() && !paused && d.phase == *s {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(steps)).block(panel(&format!(
            "{} · {}",
            if d.finished.is_some() {
                "Last stage"
            } else {
                &d.phase
            },
            duration(d.phase_elapsed().as_secs())
        ))),
        r[1],
    );
    f.render_widget(
        p(match &d.nudges.active {
            Some(n) => format!("{}\nNudge #{}: {}", d.task, n.id, n.request),
            None => d.task.clone(),
        })
        .block(panel(&format!(
            "{} · {}",
            if d.finished.is_some() {
                "Last task"
            } else {
                "Current task"
            },
            d.workspace_branch
        ))),
        r[2],
    );
    let body = Layout::horizontal(if wide {
        vec![Constraint::Min(40), Constraint::Length(29)]
    } else {
        vec![Constraint::Percentage(100), Constraint::Length(0)]
    })
    .spacing(if wide { 1 } else { 0 })
    .split(r[3]);
    let log = Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).split(body[0]);
    f.render_widget(
        Tabs::new(vec![
            "1 Live",
            "2 Model",
            "3 Checks",
            "4 Goal",
            "5 Settings",
        ])
        .select(d.tab)
        .style(Style::default().fg(MUTED))
        .highlight_style(Style::default().fg(ACCENT).bold())
        .divider(" │ "),
        log[0],
    );
    let label = if d.finished.is_some() && !d.searching && d.filter.is_empty() {
        "Saved output · scroll to review".into()
    } else if d.searching {
        format!("Search: {}▏", d.filter)
    } else if !d.filter.is_empty() {
        format!("Filter: {} · Esc clears", d.filter)
    } else if d.follow {
        "Output · following live".into()
    } else {
        "Output · scrollback · F to follow".into()
    };
    let label = if matches!(d.tab, 0 | 2) {
        format!(
            "{label} · E {}",
            if d.expanded_commands {
                "collapse output"
            } else {
                "expand output"
            }
        )
    } else {
        label
    };
    let block = panel(&label);
    let inner = block.inner(log[1]);
    f.render_widget(block, log[1]);
    let show_activity = d.tab <= 2 && d.finished.is_none() && inner.height >= 2;
    let output = Rect {
        height: inner.height.saturating_sub(u16::from(show_activity)),
        ..inner
    };
    if show_activity && let Some(activity) = d.quiet_activity() {
        f.render_widget(
            Paragraph::new(activity).fg(CYAN),
            Rect::new(inner.x, inner.y + output.height, inner.width, 1),
        );
    }
    let lines = if d.tab == 4 {
        settings_lines(d, c)
    } else {
        d.lines(inner.width.saturating_sub(1) as usize, &c.goal)
    };
    d.total_rows = lines.len();
    d.rows = output.height as usize;
    if d.follow {
        d.scroll = d.total_rows.saturating_sub(d.rows);
    } else {
        d.scroll = d.scroll.min(d.total_rows.saturating_sub(d.rows));
    }
    f.render_widget(
        Paragraph::new(Text::from(
            lines
                .into_iter()
                .skip(d.scroll)
                .take(d.rows)
                .collect::<Vec<_>>(),
        )),
        output,
    );
    if d.total_rows > d.rows {
        let mut state = ScrollbarState::new(d.total_rows.saturating_sub(d.rows)).position(d.scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(ACCENT)),
            log[1],
            &mut state,
        );
    }
    if wide {
        let block = panel("Run health");
        let inner = block.inner(body[1]);
        f.render_widget(block, body[1]);
        let side = Layout::vertical([
            Constraint::Length(7),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Min(0),
        ])
        .split(inner);
        let model = vec![
            Line::from("MODEL").fg(CYAN).bold(),
            Line::from(d.active_model.clone()),
            Line::from(if d.finished.is_some() {
                "○ Idle · no work running".to_owned()
            } else if paused {
                "Ⅱ Paused in current cycle".to_owned()
            } else if let Some(seconds) = d.provider_wait {
                format!("◷ Provider retry in {seconds}s")
            } else if d.request_active {
                "● Receiving response".to_owned()
            } else {
                if let Some((id, elapsed, review, at)) = &d.command_status {
                    format!(
                        "Cmd {id}: {}s · review {}s",
                        elapsed + at.elapsed().as_secs(),
                        review.saturating_sub(at.elapsed().as_secs())
                    )
                } else {
                    "○ Between requests".to_owned()
                }
            })
            .fg(if d.request_active { GREEN } else { MUTED }),
            Line::from(format!("{:.1} tok/s · last reply", d.speed)),
            Line::from(format!("Requests this cycle: {}", d.calls)),
            Line::from(format!("Generated: {} tokens", d.generated)),
        ];
        f.render_widget(p(Text::from(model)), side[0]);
        f.render_widget(
            p(Text::from(vec![
                Line::from("CONTEXT · LAST RESPONSE").fg(CYAN).bold(),
                Line::from(format!("{} / {} tokens", d.prompt, c.context_tokens)),
            ])),
            side[1],
        );
        if side[1].height > 2 {
            f.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(ACCENT).bg(BG))
                    .ratio((d.prompt as f64 / c.context_tokens.max(1) as f64).clamp(0., 1.))
                    .label(""),
                Rect::new(side[1].x, side[1].y + 2, side[1].width, 1),
            );
        }
        let session = vec![
            Line::from("CYCLES & SESSION").fg(CYAN).bold(),
            Line::from(format!("Overall cycle: #{}", d.cycle)),
            Line::from(format!("This session: {} finished", d.completed_cycles)),
            Line::from(format!("Recovery saves: {}", d.checkpoints)),
            Line::from(format!(
                "Elapsed {}",
                duration(d.active_elapsed().as_secs())
            )),
            Line::from(format!(
                "Last activity {}s ago",
                d.last_activity.elapsed().as_secs()
            )),
            Line::from(match d.last_check {
                Some(true) => "✓ Latest checks passed",
                Some(false) => "× Checks need attention",
                None if d.finished.is_some() => "No check result recorded",
                None => "Checks pending / running",
            })
            .fg(match d.last_check {
                Some(true) => GREEN,
                Some(false) => GOLD,
                None => MUTED,
            }),
            Line::from(format!(
                "Last passing: {}",
                d.last_passing_checkpoint.as_deref().unwrap_or("none")
            ))
            .fg(MUTED),
        ];
        f.render_widget(p(Text::from(session)), side[2]);
        let mut history = vec![Line::from("RECENT CYCLES").fg(CYAN).bold()];
        for h in d.history.iter().take(3) {
            for line in textwrap::wrap(h, inner.width.max(1) as usize) {
                history.push(Line::from(line.into_owned()).fg(MUTED));
            }
            history.push(Line::from(""));
        }
        f.render_widget(p(Text::from(history)), side[3]);
    }
    let health = if d.resources.total_gib > 0. {
        format!(
            " Local CPU {:>3.0}%  ·  RAM {:.0}% of {:.1} GiB  ·  Chuggin {:.0} MiB  │  Ollama: {}",
            d.resources.cpu,
            d.resources.memory,
            d.resources.total_gib,
            d.resources.rss_mib,
            c.ollama_url
        )
    } else {
        format!(" Local resources unavailable  │  Ollama: {}", c.ollama_url)
    };
    f.render_widget(Paragraph::new(health).fg(MUTED), r[4]);
    if r[4].height > 1 && d.resources.total_gib > 0. {
        let bars = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(2)
            .split(Rect::new(r[4].x, r[4].y + 1, r[4].width, 1));
        f.render_widget(
            LineGauge::default()
                .filled_style(Style::default().fg(CYAN))
                .unfilled_style(Style::default().fg(Color::Rgb(51, 59, 75)))
                .ratio((d.resources.cpu / 100.).clamp(0., 1.))
                .label("CPU"),
            bars[0],
        );
        f.render_widget(
            LineGauge::default()
                .filled_style(Style::default().fg(ACCENT))
                .unfilled_style(Style::default().fg(Color::Rgb(51, 59, 75)))
                .ratio((d.resources.memory / 100.).clamp(0., 1.))
                .label("RAM"),
            bars[1],
        );
    }
    let footer = if let Some(s) = &d.finished {
        format!("{s} · R resume · Enter / q returns home")
    } else if paused {
        if d.provider_wait.is_some() {
            "P resume current cycle · T retry on resume · Ctrl+C finish cycle".into()
        } else {
            "P resume current cycle · Ctrl+C finish cycle".into()
        }
    } else if pausing {
        "Pause requested · P cancel pause\nFinishing current response/tool before pausing".into()
    } else if c.run_duration_seconds > 0 && d.active_elapsed().as_secs() >= c.run_duration_seconds {
        "Time limit reached · Finishing this cycle, then saving".into()
    } else if stopping {
        "Finishing this cycle · R resume · Ctrl+C again force-stops".into()
    } else if d.provider_wait.is_some() {
        "Waiting for provider · T retry now · P pause\nCtrl+C finish cycle · N nudge · ? help"
            .into()
    } else if c.run_duration_seconds > 0 {
        format!(
            "Time left {} · P pause · Ctrl+C finish cycle · N nudge · ? help",
            duration(
                c.run_duration_seconds
                    .saturating_sub(d.active_elapsed().as_secs())
            )
        )
    } else {
        "P pause · Ctrl+C finish cycle · N nudge · 1–5 views · ? help".into()
    };
    if let Some(reason) = &d.finished {
        f.render_widget(
            Paragraph::new(vec![
                Line::from("WORK PAUSED").bold(),
                Line::from("No work is running."),
                Line::from(reason.clone()),
                Line::from("R resume work  ·  N nudge  ·  Enter / Q return home"),
            ])
            .alignment(Alignment::Center)
            .style(Style::default().fg(GOLD).bg(Color::Rgb(42, 34, 20)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(GOLD)),
            ),
            r[5],
        );
    } else if paused {
        let timer = if c.run_duration_seconds > 0 {
            format!(
                "Timer held · {} left",
                duration(
                    c.run_duration_seconds
                        .saturating_sub(d.active_elapsed().as_secs())
                )
            )
        } else {
            "Timer held · Started commands may still finish".into()
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from("CYCLE PAUSED").bold(),
                Line::from(timer),
                Line::from(footer),
            ])
            .style(Style::default().fg(GOLD).bg(Color::Rgb(42, 34, 20))),
            r[5],
        );
    } else {
        f.render_widget(
            Paragraph::new(footer).fg(if stopping || pausing { GOLD } else { MUTED }),
            r[5],
        );
    }
    if let Some(draft) = &d.nudge_edit {
        let area = Rect::new(
            a.x + 2,
            a.y + 2,
            a.width.saturating_sub(4),
            a.height.saturating_sub(4),
        );
        f.render_widget(Clear, area);
        f.render_widget(p(format!("Temporary priority within the project goal.\nApplies on the next model call; paused work stays paused.\n\n{draft}▏\n\n{}\n\nEnter save · Esc close · Ctrl+U clear\nCtrl+D cancel active · Ctrl+R reopen latest\nCompletion reports and history: Goal tab", d.nudge_error)).block(panel("Nudge")), area);
    }
    if d.help {
        let area = a.inner(Margin::new(2, 1));
        f.render_widget(Clear, area);
        f.render_widget(p("P              Pause / continue current cycle\nT              Retry provider now; queue while paused\nCtrl+C / Q     Finish cycle, then stop\nCtrl+C again   Force stop immediately\nR              Cancel stop / resume saved run\nN              Add / manage a temporary nudge\n1–5 / Tab      Live, model, checks, goal, settings\n↑↓ / wheel     Scroll a few lines\nPgUp / PgDn    Scroll a page\nHome / End     Oldest / latest output\nF              Follow live output\nE              Expand / collapse command output\n/              Search current view\nEsc            Clear search / close help\n\nPause waits for the current operation; the run timer holds.\nStarted commands may finish. Full logs stay in .chuggin/.\nResources describe this computer, not the remote GPU.").block(panel("Keyboard guide · ? / Esc closes")),area);
    }
}

pub fn dashboard(path: &Path, stop: Arc<AtomicBool>, running: Arc<AtomicBool>) -> Result<()> {
    while dashboard_session(path, stop.clone(), running.clone())? {}
    Ok(())
}

fn dashboard_session(path: &Path, stop: Arc<AtomicBool>, running: Arc<AtomicBool>) -> Result<bool> {
    let mut config = runner::load(path)?;
    // Identity setup can prompt, so complete it on the thread that owns the terminal.
    crate::setup::ensure_git_identity(&config.repo)?;
    let mut d = Dashboard::new(&config);
    let rx = events::subscribe();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let owned_path = path.to_owned();
    let worker_stop = stop.clone();
    let worker_controls = d.controls.clone();
    stop.store(false, Ordering::SeqCst);
    running.store(true, Ordering::SeqCst);
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            runner::run_controlled(&owned_path, None, worker_stop, worker_controls)
        });
        let result = match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("Runner panicked; the working files and logs remain on disk.".into()),
        };
        let _ = done_tx.send(result);
    });
    let result = (|| -> Result<bool> {
        loop {
            for e in rx.try_iter().take(4096) {
                d.apply(e);
            }
            d.poll_tail();
            match crate::nudge::read(&config.state_dir) {
                Ok(store) => d.nudges = store,
                Err(e) => d.nudge_error = e.to_string(),
            }
            d.resources.refresh();
            if let Ok(updated) = runner::load(path) {
                config = updated;
            }
            if d.finished.is_none()
                && let Ok(result) = done_rx.try_recv()
            {
                d.freeze_elapsed();
                d.finished = Some(match result {
                    Ok(())
                        if config.state_dir.join("goal-completion.json").exists()
                            && config.allow_goal_completion =>
                    {
                        "Model reports project complete".into()
                    }
                    Ok(()) => "Run saved".into(),
                    Err(e) => format!("Stopped: {e}"),
                });
                d.request_active = false;
                running.store(false, Ordering::SeqCst);
                d.flush_model();
            }
            draw(|f| render_dashboard(f, &mut d, &config, stop.load(Ordering::SeqCst)))?;
            if let Some(e) = input()? {
                if let Input::Mouse(m) = e {
                    match m.kind {
                        MouseEventKind::ScrollUp => d.back(3),
                        MouseEventKind::ScrollDown => d.forward(3),
                        _ => {}
                    }
                }
                if let Input::Paste(value) = &e
                    && let Some(draft) = d.nudge_edit.as_mut()
                {
                    if draft.len() + value.len() <= 8000 {
                        draft.push_str(value);
                    } else {
                        d.nudge_error = "Nudge is limited to 8000 bytes".into();
                    }
                    continue;
                }
                if let Some(k) = pressed(&e) {
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        if d.finished.is_some() {
                            break;
                        }
                        if stop.swap(true, Ordering::SeqCst) {
                            restore();
                            crate::project::kill_active_check();
                            std::process::exit(130);
                        }
                        d.controls.resume();
                        continue;
                    }
                    if let Some(draft) = d.nudge_edit.as_mut() {
                        let action = match k.code {
                            KeyCode::Esc => {
                                d.nudge_edit = None;
                                continue;
                            }
                            KeyCode::Enter => {
                                Some(crate::nudge::set(&config.state_dir, draft, d.nudge_id))
                            }
                            KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                Some(
                                    d.nudge_id
                                        .context("No active nudge")
                                        .and_then(|id| crate::nudge::cancel(&config.state_dir, id)),
                                )
                            }
                            KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                Some(crate::nudge::reopen(&config.state_dir))
                            }
                            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                draft.clear();
                                None
                            }
                            KeyCode::Backspace => {
                                draft.pop();
                                None
                            }
                            KeyCode::Char(ch) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                                if draft.len() + ch.len_utf8() <= 8000 {
                                    draft.push(ch);
                                }
                                None
                            }
                            _ => None,
                        };
                        if let Some(result) = action {
                            match result {
                                Ok(store) => {
                                    d.nudges = store;
                                    d.nudge_edit = None;
                                    d.nudge_error.clear();
                                }
                                Err(e) => d.nudge_error = e.to_string(),
                            }
                        }
                        continue;
                    }
                    if d.tab == 4 {
                        if let Some(text) = d.settings_edit.as_mut() {
                            match k.code {
                                KeyCode::Esc => {
                                    d.settings_edit = None;
                                    d.settings_error.clear();
                                }
                                KeyCode::Enter => {
                                    let value = text.clone();
                                    match crate::setup::save_live_setting(
                                        path,
                                        d.settings_selected,
                                        &value,
                                    ) {
                                        Ok(()) => {
                                            d.settings_edit = None;
                                            d.settings_error.clear();
                                            events::log("Project setting saved. Timer changes apply now; request settings apply on the next call.".into());
                                        }
                                        Err(e) => d.settings_error = e.to_string(),
                                    }
                                }
                                KeyCode::Backspace => {
                                    text.pop();
                                }
                                KeyCode::Char('u')
                                    if k.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    text.clear()
                                }
                                KeyCode::Char(ch)
                                    if !k.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    text.push(ch)
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match k.code {
                            KeyCode::Up => {
                                d.settings_selected = (d.settings_selected + 4) % 5;
                                continue;
                            }
                            KeyCode::Down => {
                                d.settings_selected = (d.settings_selected + 1) % 5;
                                continue;
                            }
                            KeyCode::Enter => {
                                d.settings_edit = Some(setting_value(&config, d.settings_selected));
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if d.searching {
                        match k.code {
                            KeyCode::Enter => d.searching = false,
                            KeyCode::Esc => {
                                d.searching = false;
                                d.filter.clear();
                            }
                            KeyCode::Backspace => {
                                d.filter.pop();
                            }
                            KeyCode::Char(c) => d.filter.push(c),
                            _ => {}
                        }
                        d.scroll = 0;
                        d.follow = false;
                        continue;
                    }
                    if d.help {
                        if k.code == KeyCode::Esc || k.code == KeyCode::Char('?') {
                            d.help = false;
                        }
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('p' | 'P') if d.finished.is_none() => {
                            if !stop.load(Ordering::SeqCst) {
                                let was_paused = d.controls.is_paused();
                                let pausing = d.controls.toggle_pause();
                                events::log(if pausing {
                                    "Pause requested; finishing the current operation.".into()
                                } else if was_paused {
                                    "Resumed current cycle.".into()
                                } else {
                                    "Pause cancelled; continuing the current cycle.".into()
                                });
                            }
                        }
                        KeyCode::Char('t' | 'T')
                            if d.finished.is_none() && d.provider_wait.is_some() =>
                        {
                            if d.controls.retry_now() {
                                events::log(if d.controls.pause_requested() {
                                    "Retry queued; provider wait will be skipped when you resume."
                                        .into()
                                } else {
                                    "Retry requested; checking the provider now.".into()
                                });
                            }
                        }
                        KeyCode::Char('n' | 'N') => {
                            d.nudge_id = d.nudges.active.as_ref().map(|n| n.id);
                            d.nudge_edit = Some(
                                d.nudges
                                    .active
                                    .as_ref()
                                    .map(|n| n.request.clone())
                                    .unwrap_or_default(),
                            );
                            d.nudge_error.clear();
                        }
                        KeyCode::Char('r' | 'R') if d.finished.is_some() => {
                            runner::reopen_goal(path)?;
                            return Ok(true);
                        }
                        KeyCode::Char('r' | 'R') => {
                            if stop.swap(false, Ordering::SeqCst) {
                                events::log("Stop cancelled; continuing normally.".into());
                            }
                        }
                        KeyCode::Enter | KeyCode::Char('q') if d.finished.is_some() => break,
                        KeyCode::Char('q') => {
                            stop.store(true, Ordering::SeqCst);
                            d.controls.resume();
                        }
                        KeyCode::Char('?') => d.help = true,
                        KeyCode::Char('e' | 'E') if matches!(d.tab, 0 | 2) => {
                            d.expanded_commands = !d.expanded_commands;
                        }
                        KeyCode::Up | KeyCode::Char('k') => d.back(3),
                        KeyCode::Down | KeyCode::Char('j') => d.forward(3),
                        KeyCode::PageUp => d.back(d.rows.saturating_sub(1)),
                        KeyCode::PageDown => d.forward(d.rows.saturating_sub(1)),
                        KeyCode::Home => {
                            d.follow = false;
                            d.scroll = 0;
                        }
                        KeyCode::End | KeyCode::Char('f') => d.follow = true,
                        KeyCode::Tab => {
                            d.tab = (d.tab + 1) % 5;
                            d.follow = d.tab != 3;
                            d.scroll = 0;
                        }
                        KeyCode::Char(c @ '1'..='5') => {
                            d.tab = (c as u8 - b'1') as usize;
                            d.follow = d.tab != 3;
                            d.scroll = 0;
                        }
                        KeyCode::Char('/') => {
                            d.searching = true;
                            d.filter.clear();
                        }
                        KeyCode::Esc => d.filter.clear(),
                        _ => {}
                    }
                }
            }
        }
        Ok(false)
    })();
    events::unsubscribe();
    if d.finished.is_some() || worker.is_finished() {
        let _ = worker.join();
    } else if result.is_err() {
        stop.store(true, Ordering::SeqCst);
        d.controls.resume();
    }
    result
}

pub fn busy<T: Send + 'static>(
    label: &str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    if !active() {
        return work();
    }
    let rx = events::subscribe();
    let (tx, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let started = Instant::now();
    let mut output = String::new();
    let response = (|| -> Result<T> {
        loop {
            for event in rx.try_iter().take(4096) {
                if let Event::Delta(s) = event {
                    output.push_str(&clean(&s));
                }
            }
            if output.len() > 12000 {
                let cut = output
                    .char_indices()
                    .find(|(i, _)| *i >= output.len() - 10000)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                output.drain(..cut);
            }
            match result.try_recv() {
                Ok(value) => return value,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("Background operation ended unexpectedly")
                }
                Err(_) => {}
            }
            draw(|f| {
                base(f);
                let area = f.area().inner(Margin::new(3, 2));
                let spinner =
                    ["◐", "◓", "◑", "◒"][(started.elapsed().as_millis() / 200 % 4) as usize];
                let lines: Vec<_> =
                    textwrap::wrap(&output, area.width.saturating_sub(3).max(1) as usize)
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                let tail = lines
                    .iter()
                    .rev()
                    .take(area.height.saturating_sub(4) as usize)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                f.render_widget(
                    p(format!(
                        "{spinner} {label} · {}\n\n{tail}",
                        duration(started.elapsed().as_secs())
                    ))
                    .block(panel("CHUGGIN · Working")),
                    area,
                );
            })?;
            if let Some(e) = input()?
                && let Some(k) = pressed(&e)
                && k.code == KeyCode::Char('c')
                && k.modifiers.contains(KeyModifiers::CONTROL)
            {
                restore();
                std::process::exit(130);
            }
        }
    })();
    events::unsubscribe();
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    #[test]
    fn cold_start_restores_visible_history_without_session_activity() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = config();
        c.state_dir = dir.path().to_owned();
        let bytes = serde_json::json!({"messages":[
            {"role":"system","content":"INTERNAL_SYSTEM_PROMPT"},
            {"role":"assistant","content":"Earlier progress 🌱","tool_calls":[{"function":{"name":"write_file","arguments":{"path":"example.txt"}}}]},
            {"role":"tool","content":"RAW_TOOL_RESULT"}
        ]}).to_string();
        fs::write(dir.path().join("conversation.json"), &bytes).unwrap();
        let mut d = Dashboard::new(&c);
        let live = d
            .lines(100, &c.goal)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live.contains("Earlier progress 🌱"));
        assert!(live.contains("write_file example.txt"));
        assert!(!live.contains("INTERNAL_SYSTEM_PROMPT"));
        assert!(!live.contains("RAW_TOOL_RESULT"));
        assert!(live.find("Earlier progress").unwrap() < live.find("New session").unwrap());
        d.tab = 1;
        let model = d
            .lines(100, &c.goal)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(model.contains("Earlier progress"));
        assert!(!model.contains("write_file"));
        assert_eq!(d.calls, 0);
        assert_eq!(d.generated, 0);
        assert_eq!(d.completed_cycles, 0);
        assert_eq!(
            fs::read_to_string(dir.path().join("conversation.json")).unwrap(),
            bytes
        );
        fs::write(dir.path().join("conversation.json"), "invalid").unwrap();
        let d = Dashboard::new(&c);
        assert!(
            d.entries
                .iter()
                .any(|e| e.text.contains("could not be displayed"))
        );
    }
    #[test]
    fn nudge_editor_visible_while_paused() {
        let c = config();
        let mut d = Dashboard::new(&c);
        d.finished = Some("Run saved".into());
        d.nudge_edit = Some("Make it usable".into());
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("Make it usable"));
        assert!(text.contains("paused work stays paused"));
        assert!(text.contains("Ctrl+D cancel active"));
    }
    #[test]
    fn input_cursor_edits_unicode_without_splitting_codepoints() {
        let mut e = Editor::new("héllo");
        e.key(KeyCode::Home);
        e.key(KeyCode::Right);
        e.key(KeyCode::Delete);
        e.insert("é🙂");
        assert_eq!(e.value, "hé🙂llo");
        e.key(KeyCode::Backspace);
        assert_eq!(e.value, "héllo");
        e.key(KeyCode::End);
        e.insert("\nworld");
        assert_eq!(e.value, "héllo\nworld");
    }
    fn config() -> runner::Config {
        runner::Config {repo:"/projects/example-editor".into(),goal:"Build a complete word processor with a document model, editing, layout and reliable persistence.".into(),ollama_url:"http://localhost:11434".into(),model:"example-model:latest".into(),context_tokens:128000,output_tokens:8192,implementation_calls:48,checks:vec![],state_dir:"/nonexistent/chuggin-ui-tests".into(),retry_seconds:10,run_duration_seconds:0,allow_goal_completion:false,request_timeout_seconds:1800,command_review_seconds:120}
    }
    fn screen_text(t: &Terminal<TestBackend>) -> String {
        let b = t.backend().buffer();
        let area = b.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn renders_splash_and_live_dashboard_at_multiple_sizes() {
        let c = config();
        let items = vec![
            "Resume project",
            "Project goal",
            "Progress",
            "Choose model",
            "Settings",
            "Quit",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        for (w, h) in [(120, 40), (100, 30), (70, 24), (46, 14), (25, 8)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| {
                splash(
                    f,
                    &items,
                    0,
                    "/projects/example-editor",
                    &c.model,
                    "Ready after cycle 49",
                )
            })
            .unwrap();
            if w == 120 {
                let text = screen_text(&t);
                assert!(text.contains("Resume project"));
                if let Ok(dir) = std::env::var("CHUGGIN_UI_SNAPSHOTS") {
                    fs::create_dir_all(&dir).unwrap();
                    fs::write(Path::new(&dir).join("splash.txt"), text).unwrap();
                }
            }
            let mut d = Dashboard::new(&c);
            d.apply(Event::Cycle(50));
            d.apply(Event::Task(
                "Add document container with paragraph iteration".into(),
            ));
            d.apply(Event::Phase("Work".into()));
            d.apply(Event::Metrics {
                prompt: 7641,
                generated: 2137,
                seconds: 40.,
            });
            d.apply(Event::Tool("edit_file src/model/document.rs".into()));
            d.apply(Event::Delta("I’m adding a focused iterator and checking empty-document behavior.\nThe existing Paragraph API is preserved.\n".into()));
            d.apply(Event::CheckDone(true));
            d.resources.refresh();
            t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
            let text = screen_text(&t);
            if w >= 100 {
                assert!(text.contains("Run health"));
                assert!(text.contains("7641 / 128000"));
            }
            if w == 120 {
                assert!(text.contains("following live"));
                if let Ok(dir) = std::env::var("CHUGGIN_UI_SNAPSHOTS") {
                    fs::write(Path::new(&dir).join("dashboard.txt"), text).unwrap();
                }
            }
        }
    }
    #[test]
    fn provider_wait_is_visible_and_clears_when_requests_resume() {
        let c = config();
        let mut d = Dashboard::new(&c);
        d.apply(Event::Request);
        d.apply(Event::ProviderWait {
            reason: "Usage limit reached".into(),
            seconds: 601,
        });
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        assert!(screen_text(&t).contains("Waiting for provider"));
        assert!(screen_text(&t).contains("601s"));
        assert!(screen_text(&t).contains("T retry now"));
        assert!(!d.request_active);
        d.apply(Event::Request);
        assert!(d.request_active);
        assert!(d.provider_wait.is_none());
        assert_eq!(d.phase, "Work");
    }
    #[test]
    fn pending_pause_keeps_response_visible_until_the_runner_reaches_a_safe_point() {
        let c = config();
        let mut d = Dashboard::new(&c);
        d.apply(Event::Request);
        d.controls.toggle_pause();
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("PAUSING"));
        assert!(text.contains("Receiving response"));
        assert!(text.contains("P cancel pause"));
        assert!(!text.contains("CYCLE PAUSED"));
        let mut standard = Terminal::new(TestBackend::new(80, 24)).unwrap();
        standard
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        assert!(screen_text(&standard).contains("P cancel pause"));
        d.controls.resume();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        assert!(!screen_text(&t).contains("PAUSING"));
    }
    #[test]
    fn held_cycle_has_resume_and_queued_retry_controls_without_a_running_animation() {
        let mut c = config();
        c.run_duration_seconds = 3600;
        let mut d = Dashboard::new(&c);
        d.started = Instant::now() - Duration::from_secs(65);
        d.last_output = Instant::now() - Duration::from_secs(3);
        d.apply(Event::ProviderWait {
            reason: "Server unavailable".into(),
            seconds: 600,
        });
        d.controls.toggle_pause();
        let controls = d.controls.clone();
        let worker = std::thread::spawn(move || controls.wait_until_resumed(|| false));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !d.controls.is_paused() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let held = d.controls.is_paused();
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        let elapsed = d.active_elapsed();
        std::thread::sleep(Duration::from_millis(5));
        let still_elapsed = d.active_elapsed();
        let quiet = d.quiet_activity();
        let mut standard = Terminal::new(TestBackend::new(80, 24)).unwrap();
        standard
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        let standard_text = screen_text(&standard);
        let mut small = Terminal::new(TestBackend::new(42, 12)).unwrap();
        small
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        let small_text = screen_text(&small);
        d.controls.resume();
        worker.join().unwrap();
        d.freeze_elapsed();
        let phase_elapsed = d.phase_elapsed();
        std::thread::sleep(Duration::from_millis(5));
        assert!(held);
        assert!(text.contains("PAUSED IN CYCLE"));
        assert!(text.contains("CYCLE PAUSED"));
        assert!(text.contains("P resume current cycle"));
        assert!(text.contains("T retry on resume"));
        assert!(text.contains("Timer held"));
        assert!(text.contains("Elapsed 00:01:05"));
        assert_eq!(elapsed, still_elapsed);
        assert!(quiet.is_none());
        assert!(small_text.contains("PAUSED IN CYCLE"));
        assert!(!small_text.contains("Working"));
        assert!(standard_text.contains("P resume current cycle"));
        assert!(standard_text.contains("T retry on resume"));
        assert!(standard_text.contains("Ctrl+C finish cycle"));
        assert_eq!(d.phase_elapsed(), phase_elapsed);
        assert!(
            d.finished_at
                .unwrap()
                .saturating_duration_since(d.phase_started)
                .saturating_sub(phase_elapsed)
                >= Duration::from_millis(5)
        );
        assert!(d.finished.is_none());
    }
    #[test]
    fn observation_controls_and_help_fit_a_standard_terminal() {
        let c = config();
        let mut d = Dashboard::new(&c);
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("P pause"));
        assert!(text.contains("? help"));
        d.apply(Event::ProviderWait {
            reason: "Server unavailable".into(),
            seconds: 60,
        });
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("T retry now · P pause"));
        d.help = true;
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("Pause / continue current cycle"));
        assert!(text.contains("Retry provider now; queue while paused"));
        assert!(text.contains("Force stop immediately"));
        assert!(text.contains("Clear search / close help"));
        assert!(text.contains("Started commands may finish"));
    }
    #[test]
    fn scrollback_stays_put_and_search_filters_actual_output() {
        let c = config();
        let mut d = Dashboard::new(&c);
        for i in 0..100 {
            d.push(Kind::Activity, format!("Activity number {i}"));
        }
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        d.back(10);
        let position = d.scroll;
        d.push(Kind::Model, "new output".into());
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        assert_eq!(d.scroll, position);
        d.forward(3);
        assert!(!d.follow);
        d.forward(1000);
        assert!(d.follow);
        d.push(Kind::Activity, "output after returning to bottom".into());
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        assert_eq!(d.scroll, d.total_rows.saturating_sub(d.rows));
        d.filter = "number 42".into();
        let lines = d.lines(80, &c.goal);
        assert_eq!(lines.len(), 1);
        d.filter.clear();
        d.tab = 1;
        assert_eq!(d.lines(80, &c.goal).len(), 1);
        for i in 0..3000 {
            d.push(Kind::Activity, i.to_string());
        }
        assert_eq!(d.entries.len(), 2000);
        assert!(d.dropped > 0);
    }

    #[test]
    fn duration_is_visible_and_expiry_keeps_the_cycle_running() {
        let mut c = config();
        c.run_duration_seconds = 36000;
        let mut d = Dashboard::new(&c);
        let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
        terminal
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        assert!(screen_text(&terminal).contains("Time left 10:00:00"));
        c.run_duration_seconds = 1;
        d.started = Instant::now() - Duration::from_secs(2);
        terminal
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Time limit reached"));
        assert!(text.contains("Finishing this cycle"));
        assert!(d.finished.is_none());
    }

    #[test]
    fn stopped_dashboard_clearly_labels_saved_work_and_freezes_elapsed_time() {
        let c = config();
        let mut d = Dashboard::new(&c);
        let end = Instant::now();
        d.started = end - Duration::from_secs(65);
        d.phase_started = end - Duration::from_secs(10);
        d.finished_at = Some(end);
        d.finished = Some("Run saved".into());
        d.task = "Last completed task".into();
        for (width, height) in [(110, 36), (80, 24), (40, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| render_dashboard(f, &mut d, &c, false))
                .unwrap();
            let text = screen_text(&terminal);
            assert!(text.contains("WORK PAUSED"));
            assert!(text.contains("No work is running."));
            assert!(text.contains("R resume"));
            assert!(!text.contains("Current task"));
            assert!(!text.contains("following live"));
            assert!(!text.contains("agent continues working"));
            if width >= 100 {
                assert!(text.contains("Last task"));
                assert!(text.contains("Elapsed 00:01:05"));
                assert!(text.contains("Idle · no work running"));
            }
        }
    }
    #[test]
    fn terminal_control_sequences_do_not_escape_the_output_widget() {
        assert_eq!(clean("\x1b[31merror\x1b[0m\r\n\x07"), "error\n");
    }
    #[test]
    fn short_checks_are_visible_even_when_completed_between_frames() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("check.log");
        fs::write(&file, "test focused_behavior ... ok\n").unwrap();
        let mut d = Dashboard::new(&config());
        d.apply(Event::Check {
            command: "cargo test".into(),
            path: file,
        });
        d.apply(Event::CheckDone(true));
        assert!(
            d.entries
                .iter()
                .filter_map(|e| e.command.as_ref())
                .any(|c| c
                    .display(true)
                    .iter()
                    .any(|s| s.contains("focused_behavior")))
        );
        // An arbitrary command completing is not the configured validation result.
        assert_eq!(d.last_check, None);
    }

    #[test]
    fn failing_checkpoints_are_saved_without_claiming_they_passed() {
        let c = config();
        let mut d = Dashboard::new(&c);
        d.apply(Event::Cycle(1));
        d.apply(Event::CheckDone(true));
        d.apply(Event::ValidationDone {
            passed: true,
            checkpoint: Some("abc12345abcdef".into()),
        });
        d.apply(Event::Outcome {
            disposition: "checkpoint".into(),
            task: "Initial implementation".into(),
        });
        d.apply(Event::Cycle(2));
        d.apply(Event::Phase("Checkpoint".into()));
        d.apply(Event::CheckDone(false));
        d.apply(Event::ValidationDone {
            passed: false,
            checkpoint: Some("def98765abcdef".into()),
        });
        d.apply(Event::Outcome {
            disposition: "checkpoint/checks-failing".into(),
            task: "Continue repairing current work".into(),
        });
        assert_eq!(d.checkpoints, 2);
        assert_eq!(d.completed_cycles, 2);
        assert_eq!(d.last_passing_checkpoint.as_deref(), Some("abc12345"));
        assert_eq!(d.last_check, Some(false));
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        let text = screen_text(&t);
        assert!(text.contains("Overall cycle: #2"));
        assert!(text.contains("This session: 2 finished"));
        assert!(text.contains("Recovery saves: 2"));
        assert!(text.contains("work is kept for repair"));
        assert!(text.contains("Last passing: abc12345"));
        assert!(text.contains("Saved · checks failing"));
        assert!(!text.contains("unaccepted"));
        let b = t.backend().buffer();
        let highlighted = (0..b.area.height)
            .flat_map(|y| (0..b.area.width).map(move |x| (x, y)))
            .filter(|&pos| b[pos].bg == CYAN)
            .map(|pos| b[pos].symbol())
            .collect::<String>();
        assert_eq!(highlighted, "Checkpoint");
    }

    #[test]
    fn unverified_and_unchanged_cycles_do_not_claim_a_passing_checkpoint() {
        let mut d = Dashboard::new(&config());
        d.apply(Event::Outcome {
            disposition: "checkpoint/unverified".into(),
            task: "Interrupted request".into(),
        });
        d.apply(Event::Outcome {
            disposition: "unchanged".into(),
            task: "Inspect files".into(),
        });
        assert_eq!(d.checkpoints, 1);
        assert_eq!(d.completed_cycles, 2);
        assert!(d.last_passing_checkpoint.is_none());
    }

    #[test]
    fn resumed_dashboard_keeps_last_passing_checkpoint_when_new_checks_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = config();
        c.state_dir = dir.path().to_owned();
        fs::write(
            dir.path().join("state.json"),
            r#"{"cycle":8,"last_checks_passed_ref":"0123456789abcdef","recent":[{"cycle":8,"disposition":"checkpoint/checks-failing","task":"Repair existing changes"}]}"#,
        )
        .unwrap();
        let mut d = Dashboard::new(&c);
        d.apply(Event::Cycle(9));
        d.apply(Event::CheckDone(false));
        d.apply(Event::ValidationDone {
            passed: false,
            checkpoint: None,
        });
        d.apply(Event::Outcome {
            disposition: "checkpoint/checks-failing".into(),
            task: "Keep refining existing changes".into(),
        });
        assert_eq!(d.last_passing_checkpoint.as_deref(), Some("01234567"));
        assert_eq!(d.checkpoints, 1);
        assert_eq!(d.completed_cycles, 1);
        let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
        t.draw(|f| render_dashboard(f, &mut d, &c, false)).unwrap();
        assert!(screen_text(&t).contains("Last passing: 01234567"));
    }

    #[test]
    fn successful_commands_do_not_overwrite_failed_validation() {
        let mut d = Dashboard::new(&config());
        d.apply(Event::ValidationDone {
            passed: false,
            checkpoint: None,
        });
        d.apply(Event::CheckDone(true));
        d.apply(Event::Outcome {
            disposition: "unchanged".into(),
            task: "Inspect failures".into(),
        });
        assert_eq!(d.last_check, Some(false));
        assert!(d.last_passing_checkpoint.is_none());
    }
    #[test]
    fn command_cards_collapse_expand_and_quiet_animation_stops_when_paused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command.log");
        fs::write(&path,"running 150 tests\ntest hidden_success ... ok\ntest broken_example ... FAILED\ntest result: FAILED. 145 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s\n").unwrap();
        let c = config();
        let mut d = Dashboard::new(&c);
        d.apply(Event::Check {
            command: "cargo test".into(),
            path,
        });
        d.apply(Event::CheckDone(false));
        d.request_active = true;
        d.last_output = Instant::now() - Duration::from_secs(3);
        d.phase = "Work".into();
        d.task = "Repair the remaining test failures".into();
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        let collapsed = screen_text(&terminal);
        assert!(collapsed.contains("145/150 tests passed"));
        assert!(!collapsed.contains("hidden_success"));
        assert!(collapsed.contains("broken_example"));
        assert!(collapsed.contains("E expand output"));
        assert!(collapsed.contains("Waiting for response"));
        if let Ok(dir) = std::env::var("CHUGGIN_UI_SNAPSHOTS") {
            fs::create_dir_all(&dir).unwrap();
            fs::write(Path::new(&dir).join("collapsed-commands.txt"), &collapsed).unwrap();
        }
        d.expanded_commands = true;
        terminal
            .draw(|f| render_dashboard(f, &mut d, &c, false))
            .unwrap();
        assert!(screen_text(&terminal).contains("hidden_success"));
        assert_ne!(activity_bar(0), activity_bar(280));
        assert_eq!(activity_bar(0).len(), 12);
        d.finished = Some("Run saved".into());
        assert!(d.quiet_activity().is_none());
    }
}
