//! Presentation-only command summaries. Never changes tool results or saved logs.
use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Default, Clone, Copy)]
struct Counts {
    passed: u64,
    failed: u64,
    skipped: u64,
}
impl Counts {
    fn total(self) -> u64 {
        self.passed + self.failed + self.skipped
    }
}
pub struct CommandOutput {
    pub command: String,
    pub path: PathBuf,
    file: Option<File>,
    partial: Vec<u8>,
    pub completed: Option<bool>,
    drained: bool,
    started: Instant,
    elapsed: Option<u64>,
    raw: VecDeque<String>,
    raw_bytes: usize,
    omitted: u64,
    lines: u64,
    failures: Vec<String>,
    counts: Counts,
    live: Counts,
    announced: u64,
    known: bool,
    total_known: bool,
}
impl CommandOutput {
    pub fn new(command: String, path: &Path) -> Self {
        Self {
            command,
            path: path.into(),
            file: File::open(path).ok(),
            partial: Vec::new(),
            completed: None,
            drained: false,
            started: Instant::now(),
            elapsed: None,
            raw: VecDeque::new(),
            raw_bytes: 0,
            omitted: 0,
            lines: 0,
            failures: Vec::new(),
            counts: Counts::default(),
            live: Counts::default(),
            announced: 0,
            known: false,
            total_known: false,
        }
    }
    pub fn finish(&mut self, passed: bool) {
        self.completed = Some(passed);
        self.elapsed = Some(self.started.elapsed().as_secs());
    }
    pub fn poll(&mut self, budget: usize) -> usize {
        if self.drained {
            return 0;
        }
        let Some(file) = &mut self.file else {
            return 0;
        };
        let mut bytes = vec![0; budget];
        let n = match file.read(&mut bytes) {
            Ok(n) => n,
            Err(_) => return 0,
        };
        self.partial.extend_from_slice(&bytes[..n]);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let row: Vec<_> = self.partial.drain(..=end).collect();
            self.line(&String::from_utf8_lossy(&row));
        }
        if self.partial.len() > 16000 {
            let row = std::mem::take(&mut self.partial);
            self.line(&String::from_utf8_lossy(&row));
        }
        if n < budget && self.completed.is_some() {
            let row = std::mem::take(&mut self.partial);
            if !row.is_empty() {
                self.line(&String::from_utf8_lossy(&row));
            }
            self.drained = true;
            self.file = None;
        }
        n
    }
    fn line(&mut self, text: &str) {
        let line = crate::ui::clean(text).trim_end().to_owned();
        self.lines += 1;
        self.observe(&line);
        let lower = line.to_lowercase();
        if self.failures.len() < 3
            && (lower.starts_with("error")
                || lower.starts_with("failed ")
                || lower.contains(" ... failed")
                || lower.starts_with("not ok ")
                || lower.contains("has been running for over"))
        {
            self.failures.push(crate::project::excerpt(&line, 220));
        }
        let line = crate::project::excerpt(&line, 4000);
        self.raw_bytes += line.len();
        self.raw.push_back(line);
        while self.raw_bytes > 20000 || self.raw.len() > 500 {
            if let Some(old) = self.raw.pop_front() {
                self.raw_bytes -= old.len();
                self.omitted += 1;
            }
        }
    }
    fn observe(&mut self, line: &str) {
        let t = line.trim();
        if let Some(n) = t
            .strip_prefix("running ")
            .and_then(|s| s.strip_suffix(" tests").or_else(|| s.strip_suffix(" test")))
            .and_then(|s| s.parse().ok())
        {
            self.announced = n;
            self.total_known = true;
            self.live = Counts::default();
            self.known = true;
        } else if t.starts_with("test result:") {
            self.total_known = true;
            let c = counts(t);
            self.counts.passed += c.passed;
            self.counts.failed += c.failed;
            self.counts.skipped += c.skipped;
            self.live = Counts::default();
            self.announced = 0;
            self.known = true;
        } else if t.starts_with("test ") {
            if t.ends_with(" ... ok") {
                self.live.passed += 1;
                self.known = true;
            } else if t.ends_with(" ... FAILED") {
                self.live.failed += 1;
                self.known = true;
            } else if t.contains(" ... ignored") {
                self.live.skipped += 1;
                self.known = true;
            }
        } else if (t.starts_with('=')
            && (t.contains(" passed") || t.contains(" failed") || t.contains(" skipped")))
            || t.starts_with("Tests:")
            || t.starts_with("Tests ")
        {
            let c = counts(t);
            if c.total() > 0 {
                self.total_known = true;
                self.counts = c;
                self.live = Counts::default();
                self.announced = 0;
                self.known = true;
            }
        } else if t.starts_with("# pass ")
            || t.starts_with("# fail ")
            || t.starts_with("# skipped ")
            || t.starts_with("# tests ")
        {
            let words: Vec<_> = t.split_whitespace().collect();
            if let Some(n) = words.get(2).and_then(|n| n.parse().ok()) {
                match words[1] {
                    "pass" => self.live.passed = n,
                    "fail" => self.live.failed = n,
                    "skipped" => self.live.skipped = n,
                    "tests" => {
                        self.announced = n;
                        self.total_known = true;
                    }
                    _ => {}
                }
                self.known = true;
            }
        }
    }
    pub fn summary(&self) -> String {
        let elapsed = self
            .elapsed
            .unwrap_or_else(|| self.started.elapsed().as_secs());
        let status = match self.completed {
            Some(true) => "✓",
            Some(false) => "×",
            None => "·",
        };
        let totals = if self.known {
            let p = self.counts.passed + self.live.passed;
            let f = self.counts.failed + self.live.failed;
            let skipped = self.counts.skipped + self.live.skipped;
            let total = self.counts.total() + self.announced.max(self.live.total());
            let mut parts = vec![if self.total_known {
                if total == 0 {
                    "No tests executed".into()
                } else {
                    format!("{p}/{total} tests passed")
                }
            } else {
                format!("{p} tests passed · total not reported")
            }];
            if f > 0 {
                parts.push(format!("{f} failed"));
            }
            if skipped > 0 {
                parts.push(format!("{skipped} skipped"));
            }
            if self.completed.is_none() {
                parts.push("running".into());
            } else if self.completed == Some(false) {
                parts.push("command failed".into());
            }
            if self.completed.is_some() && !self.drained {
                parts.push("reading remaining output".into());
            }
            parts.join(" · ")
        } else {
            format!(
                "{} · {} output lines",
                match self.completed {
                    Some(true) => "Command succeeded",
                    Some(false) => "Command failed",
                    None => "Running",
                },
                self.lines
            )
        };
        format!("{status} {totals} · {elapsed}s")
    }
    pub fn display(&self, expanded: bool) -> Vec<String> {
        let compact = crate::ui::clean(&self.command)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut title = compact.chars().take(120).collect::<String>();
        if compact.chars().count() > 120 {
            title.push('…');
        }
        let mut lines = vec![
            format!("{} $ {}", if expanded { "▾" } else { "▸" }, title),
            format!("  {}", self.summary()),
        ];
        if expanded {
            if compact.chars().count() > 120 || self.command.contains('\n') {
                lines.push(format!(
                    "  Command: {}",
                    crate::project::excerpt(&crate::ui::clean(&self.command), 12000)
                ));
            }
            if self.omitted > 0 {
                lines.push(format!(
                    "  … {} earlier lines omitted from this view",
                    self.omitted
                ));
            }
            for line in &self.raw {
                lines.push(format!("  │ {line}"));
            }
            if !self.partial.is_empty() {
                lines.push(format!(
                    "  │ {}",
                    crate::ui::clean(&String::from_utf8_lossy(&self.partial))
                ));
            }
            lines.push(format!("  Full log: {}", self.path.display()));
        } else {
            for failure in &self.failures {
                lines.push(format!("  {failure}"));
            }
        }
        lines
    }
}
fn counts(text: &str) -> Counts {
    let mut result = Counts::default();
    let words: Vec<_> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        if let Ok(n) = pair[0]
            .trim_matches(|c: char| !c.is_ascii_digit())
            .parse::<u64>()
        {
            match pair[1].trim_matches(|c: char| !c.is_ascii_alphabetic()) {
                "passed" => result.passed = n,
                "failed" => result.failed = n,
                "ignored" | "skipped" => result.skipped = n,
                _ => {}
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(log: &str, passed: bool) -> CommandOutput {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("check.log");
        std::fs::write(&path, log).unwrap();
        let mut output = CommandOutput::new("check".into(), &path);
        output.finish(passed);
        while !output.drained {
            output.poll(1024);
        }
        output
    }
    #[test]
    fn rust_suites_aggregate_without_double_counting_and_failures_stay_visible() {
        let mut log = "running 150 tests\n".to_owned();
        for i in 0..145 {
            log.push_str(&format!("test example_{i} ... ok\n"));
        }
        for i in 145..150 {
            log.push_str(&format!("test example_{i} ... FAILED\n"));
        }
        log.push_str("test result: FAILED. 145 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s\nrunning 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\n");
        let out = output(&log, false);
        assert!(out.summary().contains("145/150 tests passed · 5 failed"));
        let collapsed = out.display(false).join("\n");
        assert!(!collapsed.contains("example_0 "));
        assert!(collapsed.contains("example_145 ... FAILED"));
        assert!(out.display(false).len() <= 5);
        assert!(out.display(true).join("\n").contains("example_0 ... ok"));
    }
    #[test]
    fn recognizes_pytest_jest_and_tap_without_inventing_generic_counts() {
        for log in [
            "===== 145 passed, 5 failed in 1.20s =====\n",
            "Tests: 5 failed, 145 passed, 150 total\n",
            "# tests 150\n# pass 145\n# fail 5\n",
        ] {
            assert!(
                output(log, false)
                    .summary()
                    .contains("145/150 tests passed"),
                "{log}"
            );
        }
        assert!(
            output("download completed\n", true)
                .summary()
                .contains("Command succeeded")
        );
        assert!(
            !output("145 products passed inspection\n", true)
                .summary()
                .contains("tests passed")
        );
        assert!(
            output("test only_visible_test ... ok\n", true)
                .summary()
                .contains("total not reported")
        );
    }
    #[test]
    fn large_logs_continue_draining_after_completion_without_losing_final_totals() {
        let log = format!(
            "{}\ntest result: ok. 145 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out; finished in 1s\n",
            "diagnostic detail\n".repeat(10000)
        );
        let out = output(&log, true);
        assert!(out.summary().contains("145/150 tests passed · 5 skipped"));
        assert!(out.raw_bytes <= 20000);
        assert!(out.omitted > 0);
        assert!(
            out.display(true)
                .join("\n")
                .contains("earlier lines omitted")
        );
    }
}
