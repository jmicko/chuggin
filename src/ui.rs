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
    fs,
    io::{self, Read, Seek, SeekFrom},
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
fn clean(s: &str) -> String {
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
            Paragraph::new("Small models. Fresh starts. Lasting progress.")
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
        "Accepted work and recent attempts",
        "Choose a shared model default",
        "Connection and working budgets",
        "Return to your shell",
    ];
    let entries: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            ListItem::new(vec![
                Line::from(item.clone()).bold(),
                Line::from(descriptions[i]).fg(MUTED),
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
struct Tail {
    file: fs::File,
    offset: u64,
}
struct Dashboard {
    entries: VecDeque<Entry>,
    phase: String,
    task: String,
    cycle: u64,
    calls: u64,
    prompt: u64,
    generated: u64,
    speed: f64,
    request_active: bool,
    accepted: u64,
    retried: u64,
    started: Instant,
    phase_started: Instant,
    last_activity: Instant,
    resources: Resource,
    tab: usize,
    follow: bool,
    scroll: usize,
    rows: usize,
    total_rows: usize,
    filter: String,
    searching: bool,
    help: bool,
    finished: Option<String>,
    tail: Option<Tail>,
    partial_model: String,
    partial_check: String,
    history: VecDeque<String>,
    last_check: Option<bool>,
    dropped: u64,
}
impl Dashboard {
    fn new(config: &runner::Config) -> Self {
        let mut d = Self {
            entries: VecDeque::new(),
            phase: "Ready".into(),
            task: "Waiting for the next task".into(),
            cycle: 0,
            calls: 0,
            prompt: 0,
            generated: 0,
            speed: 0.,
            request_active: false,
            accepted: 0,
            retried: 0,
            started: Instant::now(),
            phase_started: Instant::now(),
            last_activity: Instant::now(),
            resources: Resource::new(),
            tab: 0,
            follow: true,
            scroll: 0,
            rows: 1,
            total_rows: 0,
            filter: String::new(),
            searching: false,
            help: false,
            finished: None,
            tail: None,
            partial_model: String::new(),
            partial_check: String::new(),
            history: VecDeque::new(),
            last_check: None,
            dropped: 0,
        };
        if let Ok(bytes) = fs::read(config.state_dir.join("state.json"))
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            d.cycle = v["cycle"].as_u64().unwrap_or(0);
            if let Some(recent) = v["recent"].as_array() {
                for o in recent.iter().rev().take(5) {
                    d.history.push_back(format!(
                        "#{}  {}\n{}",
                        o["cycle"],
                        o["disposition"].as_str().unwrap_or(""),
                        o["task"].as_str().unwrap_or("")
                    ));
                }
            }
        }
        d.push(
            Kind::Activity,
            "Session started. Full diagnostic history remains in .chuggin/.".into(),
        );
        d
    }
    fn push(&mut self, kind: Kind, text: String) {
        for line in clean(&text).lines() {
            self.entries.push_back(Entry {
                kind,
                text: crate::project::excerpt(line, 12000),
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
        let mut text = String::new();
        if let Some(t) = self.tail.as_mut() {
            let _ = t.file.seek(SeekFrom::Start(t.offset));
            let mut bytes = Vec::new();
            let _ = (&mut t.file).take(32768).read_to_end(&mut bytes);
            t.offset += bytes.len() as u64;
            text = String::from_utf8_lossy(&bytes).into();
        }
        if !text.is_empty() {
            self.last_activity = Instant::now();
            self.stream(Kind::Check, &text);
        }
    }
    fn apply(&mut self, event: Event) {
        self.last_activity = Instant::now();
        match event {
            Event::Log(s) => self.push(Kind::Activity, s),
            Event::Phase(s) => {
                self.request_active = false;
                self.flush_model();
                self.phase = s;
                self.phase_started = Instant::now();
                self.push(Kind::Activity, format!("── {} ──", self.phase));
            }
            Event::Cycle(n) => {
                self.cycle = n;
                self.calls = 0;
                self.last_check = None;
            }
            Event::Task(s) => self.task = s,
            Event::Request => {
                self.flush_model();
                self.calls += 1;
                self.request_active = true;
            }
            Event::Delta(s) => self.stream(Kind::Model, &s),
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
                self.tail = fs::File::open(path)
                    .ok()
                    .map(|file| Tail { file, offset: 0 });
                self.push(Kind::Check, format!("$ {command}"));
                self.last_check = None;
            }
            Event::CheckDone(ok) => {
                self.poll_tail();
                self.tail = None;
                let s = std::mem::take(&mut self.partial_check);
                self.push(Kind::Check, s);
                self.last_check = Some(ok);
                self.push(
                    Kind::Check,
                    if ok {
                        "✓ Checks passed"
                    } else {
                        "× Checks failed · candidate remains unaccepted"
                    }
                    .into(),
                );
            }
            Event::Outcome { disposition, task } => {
                if disposition.starts_with("accepted") {
                    self.accepted += 1;
                } else {
                    self.retried += 1;
                }
                self.history
                    .push_front(format!("#{}  {disposition}\n{task}", self.cycle));
                self.history.truncate(5);
                self.push(Kind::Activity, format!("{disposition} · {task}"));
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
                    Kind::Check => MUTED,
                }
            };
            for line in textwrap::wrap(text, width) {
                lines.push(Line::from(line.into_owned()).fg(color));
            }
        };
        if self.tab == 3 {
            for line in clean(goal).lines() {
                for part in textwrap::wrap(line, width) {
                    lines.push(Line::from(part.into_owned()).fg(FG));
                }
            }
        } else {
            for entry in &self.entries {
                add(entry.kind, &entry.text);
            }
            add(Kind::Model, &self.partial_model);
            add(Kind::Check, &self.partial_check);
        }
        lines
    }
}
fn duration(s: u64) -> String {
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
fn render_dashboard(f: &mut Frame, d: &mut Dashboard, c: &runner::Config, stopping: bool) {
    base(f);
    let a = f.area().inner(Margin::new(1, 0));
    if a.height < 14 || a.width < 44 {
        f.render_widget(p("CHUGGIN\n\nResize to at least 46 × 14 to view the dashboard.\nThe agent continues working.\n\nCtrl+C: finish cycle; again: force stop").fg(ACCENT),a);
        return;
    }
    let r = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(a);
    let state = if d.finished.is_some() {
        "FINISHED"
    } else if stopping {
        "DRAINING"
    } else {
        "LIVE"
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" CHUGGIN ", Style::default().fg(BG).bg(ACCENT).bold()),
            Span::styled(
                format!("  {state}  ·  cycle {}", d.cycle),
                Style::default().fg(if stopping { GOLD } else { CYAN }),
            ),
            Span::styled(
                format!(
                    "  ·  {}",
                    c.repo.file_name().unwrap_or_default().to_string_lossy()
                ),
                Style::default().fg(FG),
            ),
        ])),
        r[0],
    );
    let phases = ["Discovery", "Shape", "Implement", "Verify", "Review"];
    let mut steps = Vec::new();
    for (i, s) in phases.iter().enumerate() {
        if i > 0 {
            steps.push(Span::raw("  ›  "));
        }
        steps.push(Span::styled(
            s.to_string(),
            Style::default()
                .fg(if d.phase.starts_with(s) { BG } else { MUTED })
                .bg(if d.phase.starts_with(s) { CYAN } else { BG })
                .add_modifier(if d.phase.starts_with(s) {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(steps)).block(panel(&format!(
            "{} · {}",
            d.phase,
            duration(d.phase_started.elapsed().as_secs())
        ))),
        r[1],
    );
    f.render_widget(p(d.task.clone()).block(panel("Current task")), r[2]);
    let wide = f.area().width >= 100;
    let body = Layout::horizontal(if wide {
        vec![Constraint::Min(40), Constraint::Length(29)]
    } else {
        vec![Constraint::Percentage(100), Constraint::Length(0)]
    })
    .spacing(if wide { 1 } else { 0 })
    .split(r[3]);
    let log = Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).split(body[0]);
    f.render_widget(
        Tabs::new(vec!["1 Live", "2 Model", "3 Checks", "4 Goal"])
            .select(d.tab)
            .style(Style::default().fg(MUTED))
            .highlight_style(Style::default().fg(ACCENT).bold())
            .divider(" │ "),
        log[0],
    );
    let label = if d.searching {
        format!("Search: {}▏", d.filter)
    } else if !d.filter.is_empty() {
        format!("Filter: {} · Esc clears", d.filter)
    } else if d.follow {
        "Output · following live".into()
    } else {
        "Output · scrollback · F to follow".into()
    };
    let block = panel(&label);
    let inner = block.inner(log[1]);
    f.render_widget(block, log[1]);
    let lines = d.lines(inner.width.saturating_sub(1) as usize, &c.goal);
    d.total_rows = lines.len();
    d.rows = inner.height as usize;
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
        inner,
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
            Constraint::Length(7),
            Constraint::Min(0),
        ])
        .split(inner);
        let model = vec![
            Line::from("MODEL").fg(CYAN).bold(),
            Line::from(c.model.clone()),
            Line::from(if d.request_active {
                "● Receiving response"
            } else {
                "○ Between requests"
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
            Line::from("THIS SESSION").fg(CYAN).bold(),
            Line::from(format!("{} accepted · {} retries", d.accepted, d.retried)),
            Line::from(format!(
                "Elapsed {}",
                duration(d.started.elapsed().as_secs())
            )),
            Line::from(format!(
                "Last activity {}s ago",
                d.last_activity.elapsed().as_secs()
            )),
            Line::from(match d.last_check {
                Some(true) => "✓ Latest checks passed",
                Some(false) => "× Latest checks failed",
                None => "Checks pending / running",
            })
            .fg(match d.last_check {
                Some(true) => GREEN,
                Some(false) => GOLD,
                None => MUTED,
            }),
        ];
        f.render_widget(p(Text::from(session)), side[2]);
        let mut history = vec![Line::from("RECENT OUTCOMES").fg(CYAN).bold()];
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
        format!("{s} · Enter / q returns home")
    } else if stopping {
        "Finishing this cycle, then returning home · Ctrl+C again force-stops".into()
    } else {
        "↑↓ / wheel scroll · PgUp/PgDn · F follow · 1–4 views · / search · ? help · Ctrl+C finish cycle".into()
    };
    f.render_widget(
        Paragraph::new(footer).fg(if stopping { GOLD } else { MUTED }),
        r[5],
    );
    if d.help {
        let area = Rect::new(
            a.x + a.width / 8,
            a.y + a.height / 6,
            a.width * 3 / 4,
            a.height * 2 / 3,
        );
        f.render_widget(Clear, area);
        f.render_widget(p("Observe without interrupting work\n\n↑ / ↓ or mouse wheel    Scroll a few lines\nPgUp / PgDn             Scroll a page\nHome / End              Oldest / latest output\nF                       Resume live following\n1–4 or Tab              Live, model, checks, goal\n/                       Search the current view\nEsc                     Clear search / close help\nCtrl+C or Q             Finish this cycle, then stop\nCtrl+C again            Force stop immediately\n\nScrollback is bounded; complete logs stay in .chuggin/.\nResources describe this computer, not the remote GPU.\nContext and token speed update after each model response.").block(panel("Keyboard guide · ? / Esc closes")),area);
    }
}

pub fn dashboard(path: &Path, stop: Arc<AtomicBool>, running: Arc<AtomicBool>) -> Result<()> {
    let config = runner::load(path)?;
    let mut d = Dashboard::new(&config);
    let rx = events::subscribe();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let owned_path = path.to_owned();
    let worker_stop = stop.clone();
    stop.store(false, Ordering::SeqCst);
    running.store(true, Ordering::SeqCst);
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| runner::run(&owned_path, None, worker_stop));
        let result = match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("Runner panicked; candidate artifacts are preserved.".into()),
        };
        let _ = done_tx.send(result);
    });
    let result = (|| -> Result<()> {
        loop {
            for e in rx.try_iter().take(4096) {
                d.apply(e);
            }
            d.poll_tail();
            d.resources.refresh();
            if d.finished.is_none()
                && let Ok(result) = done_rx.try_recv()
            {
                d.finished = Some(match result {
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
                        continue;
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
                        KeyCode::Enter | KeyCode::Char('q') if d.finished.is_some() => break,
                        KeyCode::Char('q') => {
                            stop.store(true, Ordering::SeqCst);
                        }
                        KeyCode::Char('?') => d.help = true,
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
                            d.tab = (d.tab + 1) % 4;
                            d.follow = d.tab != 3;
                            d.scroll = 0;
                        }
                        KeyCode::Char(c @ '1'..='4') => {
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
        Ok(())
    })();
    events::unsubscribe();
    if worker.is_finished() {
        let _ = worker.join();
    } else if result.is_err() {
        stop.store(true, Ordering::SeqCst);
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
        runner::Config {repo:"/projects/example-editor".into(),goal:"Build a complete word processor with a document model, editing, layout and reliable persistence.".into(),ollama_url:"http://localhost:11434".into(),model:"example-model:latest".into(),context_tokens:128000,output_tokens:8192,implementation_calls:48,checks:vec![],state_dir:"/nonexistent/chuggin-ui-tests".into(),retry_seconds:10}
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
            d.apply(Event::Phase("Implement".into()));
            d.apply(Event::Metrics {
                prompt: 7641,
                generated: 2137,
                seconds: 40.,
            });
            d.apply(Event::Tool("edit_file src/model/document.rs".into()));
            d.apply(Event::Delta("I’m adding a focused iterator and checking empty-document behavior.\nThe accepted Paragraph API is preserved.\n".into()));
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
                .any(|e| e.text.contains("focused_behavior"))
        );
        assert_eq!(d.last_check, Some(true));
    }
}
