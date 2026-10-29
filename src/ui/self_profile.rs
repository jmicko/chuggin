//! Human inspection of the model's persisted self-description. This view never
//! starts inference or writes profile entries.
use super::*;
use crate::self_profile::Variant;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct ProfilePanel {
    source: Option<(PathBuf, Variant)>,
    last_poll: Option<Instant>,
    revision: Option<u64>,
    record: Option<Value>,
    recent: Vec<Value>,
    error: String,
}

impl ProfilePanel {
    pub fn poll(&mut self, config: &runner::Config, force: bool) {
        let variant = if config.activity_mode.is_chaos() {
            config.self_profile
        } else {
            Variant::Baseline
        };
        let source = (config.state_dir.clone(), variant);
        let changed_source = self.source.as_ref() != Some(&source);
        if !force
            && !changed_source
            && self
                .last_poll
                .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_poll = Some(Instant::now());
        if changed_source {
            self.source = Some(source);
            self.record = None;
            self.revision = None;
            self.recent.clear();
            self.error.clear();
        }
        if !variant.enabled() {
            return;
        }
        let result = (|| -> Result<()> {
            let revision = crate::self_profile::revision(&config.state_dir, variant)?;
            if !force && self.revision == Some(revision) && self.error.is_empty() {
                return Ok(());
            }
            let record = read_record(&config.state_dir, variant, revision)?;
            let history = crate::self_profile::call(
                &config.state_dir,
                variant,
                "read_self_profile",
                &json!({"action":"history","revision":revision,
                    "after_revision":revision.saturating_sub(5),"limit":5}),
            )?;
            let recent = history["revisions"]
                .as_array()
                .context("Profile history has no revisions")?;
            self.recent = recent.iter().rev().cloned().collect();
            self.record = Some(record);
            self.revision = Some(revision);
            self.error.clear();
            Ok(())
        })();
        if let Err(error) = result {
            self.error = format!("{error:#}");
        }
    }

    pub fn lines(&self, width: usize, filter: &str) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut add = |text: &str, color: Color, bold: bool| {
            for paragraph in clean(text).split('\n') {
                let wrapped = if paragraph.is_empty() {
                    vec![String::new()]
                } else {
                    textwrap::wrap(paragraph, width)
                        .into_iter()
                        .map(|line| line.into_owned())
                        .collect()
                };
                for line in wrapped {
                    let line = Line::from(line).fg(color);
                    lines.push(if bold { line.bold() } else { line });
                }
            }
        };
        let variant = self
            .source
            .as_ref()
            .map_or(Variant::Baseline, |(_, variant)| *variant);
        if !variant.enabled() {
            add("Baseline · profile is disabled", GOLD, true);
            add(
                "\nThis session has no active self-profile. Its model-authored preferences are not included as current profile memory.",
                FG,
                false,
            );
            add(
                "\nChoose an editable variant in Project settings → Self-profile experiment. Saved profiles and version history remain available there.",
                MUTED,
                false,
            );
        } else {
            add(
                &format!(
                    "{} · {}",
                    variant.label(),
                    self.revision
                        .map(|revision| format!("Version {revision}"))
                        .unwrap_or_else(|| "Version unavailable".into())
                ),
                ACCENT,
                true,
            );
            if !self.error.is_empty() {
                add("\nCould not refresh the saved profile", GOLD, true);
                add(&self.error, GOLD, false);
                if self.record.is_some() {
                    add(
                        "Showing the last successfully loaded version; retrying automatically.",
                        MUTED,
                        false,
                    );
                }
            }
            if let Some(record) = &self.record {
                if self.revision == Some(0) {
                    add(
                        "\nThe profile starts blank. The model has not saved a self-description yet.",
                        MUTED,
                        false,
                    );
                }
                if let Some(updated) = record["updated_at"].as_str() {
                    add(&format!("Updated: {updated}"), MUTED, false);
                }
                for (field, label) in [
                    ("tendencies", "WORKING TENDENCIES"),
                    ("interests", "INTERESTS"),
                    ("open_questions", "OPEN QUESTIONS"),
                    ("intentions", "POSSIBLE NEXT ACTIVITIES"),
                    ("reflections", "REFLECTIONS"),
                ] {
                    if let Some(entries) = record["profile"][field].as_array() {
                        add(&format!("\n{label}"), CYAN, true);
                        if entries.is_empty() {
                            add("None recorded.", MUTED, false);
                        } else {
                            for entry in entries.iter().filter_map(Value::as_str) {
                                add(&format!("• {entry}"), FG, false);
                            }
                        }
                    }
                }
                if let Some(reason) = record["reason"]
                    .as_str()
                    .filter(|reason| !reason.is_empty())
                {
                    add("\nWHY IT CHANGED", ACCENT, true);
                    add(reason, FG, false);
                }
                if let Some(evidence) = record["evidence"]
                    .as_array()
                    .filter(|entries| !entries.is_empty())
                {
                    add("\nREFERENCES FOR THIS CHANGE", ACCENT, true);
                    for item in evidence.iter().filter_map(Value::as_str) {
                        add(&format!("• {item}"), FG, false);
                    }
                }
                add("\nRECENT REVISIONS", ACCENT, true);
                if self.recent.is_empty() {
                    add("No changes saved yet.", MUTED, false);
                } else {
                    for revision in &self.recent {
                        add(
                            &format!(
                                "Version {} · {}",
                                revision["revision"].as_u64().unwrap_or(0),
                                revision["updated_at"].as_str().unwrap_or("No timestamp")
                            ),
                            MUTED,
                            false,
                        );
                        add(
                            &crate::project::excerpt(
                                revision["reason"].as_str().unwrap_or(""),
                                240,
                            ),
                            FG,
                            false,
                        );
                    }
                }
                add(
                    "\nModel-authored preferences and intentions: a self-description, not independently verified behavior.",
                    MUTED,
                    false,
                );
                add(
                    "\nRead only · updates appear automatically. Full versions are available in Project settings → Self-profile experiment.",
                    MUTED,
                    false,
                );
            } else if self.error.is_empty() {
                add("\nReading the saved profile…", MUTED, false);
            }
        }
        if !filter.is_empty() {
            let filter = filter.to_lowercase();
            lines.retain(|line| line.to_string().to_lowercase().contains(&filter));
        }
        lines
    }
}

fn read_record(state: &Path, variant: Variant, revision: u64) -> Result<Value> {
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let page = crate::self_profile::call(
            state,
            variant,
            "read_self_profile",
            &json!({"action":"read","revision":revision,"offset":offset,"limit":65536}),
        )?;
        text.push_str(
            page["record_json"]
                .as_str()
                .context("Profile page has no record")?,
        );
        anyhow::ensure!(
            text.len() <= 8 * 1024 * 1024,
            "Saved profile record is too large"
        );
        if page["complete"] == true {
            return serde_json::from_str(&text).context("Saved profile record is invalid");
        }
        let next = page["next_offset"]
            .as_u64()
            .context("Profile page has no continuation")?;
        anyhow::ensure!(next > offset, "Profile paging did not advance");
        offset = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn config(state: &Path, variant: Variant) -> runner::Config {
        serde_json::from_value(json!({"repo":state,"state_dir":state,"activity_mode":"type2",
            "model":"fixture","ollama_url":"http://unused","self_profile":variant,
            "context_tokens":8192,"output_tokens":1024,"implementation_calls":4,"checks":[],"retry_seconds":0})).unwrap()
    }

    fn update(state: &Path, expected: u64, reason: &str, patch: Value) {
        crate::self_profile::call(state, Variant::SelfDirected, "update_self_profile",
            &json!({"expected_revision":expected,"reason":reason,"evidence":["An observed example"],"patch":patch})).unwrap();
    }

    fn text(panel: &ProfilePanel, width: usize) -> String {
        panel
            .lines(width, "")
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn baseline_profile_pane_is_read_only_and_explains_how_to_enable_it() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let config = config(&state, Variant::Baseline);
        let mut panel = ProfilePanel::default();
        panel.poll(&config, false);
        let output = text(&panel, 100);
        assert!(output.contains("Baseline · profile is disabled"));
        assert!(output.contains("Self-profile experiment"));
        assert!(
            !state.exists(),
            "Viewing Baseline must not create profile files"
        );
        assert!(panel.record.is_none());
    }

    #[test]
    fn saved_profile_refreshes_only_on_revision_change_and_survives_cold_start() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path(), Variant::SelfDirected);
        let mut panel = ProfilePanel::default();
        panel.poll(&config, false);
        assert!(text(&panel, 100).contains("The profile starts blank"));
        update(
            directory.path(),
            0,
            "Curiosity after reading",
            json!({"tendencies":["Compare sources"],
            "interests":["Trees"],"open_questions":["Why do leaves differ?"],
            "intentions":["Look at leaf shapes"],"reflections":["Shapes repeat"]}),
        );
        panel.poll(&config, false);
        assert_eq!(
            panel.revision,
            Some(0),
            "Refresh is throttled while observing"
        );
        panel.last_poll = Some(Instant::now() - Duration::from_secs(2));
        panel.poll(&config, false);
        assert_eq!(panel.revision, Some(1));
        let output = text(&panel, 120);
        for expected in [
            "Compare sources",
            "Trees",
            "Why do leaves differ?",
            "Look at leaf shapes",
            "Shapes repeat",
            "Curiosity after reading",
            "An observed example",
            "RECENT REVISIONS",
        ] {
            assert!(output.contains(expected), "Missing {expected}: {output}");
        }
        assert_eq!(panel.recent.len(), 1);
        let bytes = fs::read(
            directory
                .path()
                .join("self-profile/self_directed/head.json"),
        )
        .unwrap();
        let mut cold = ProfilePanel::default();
        cold.poll(&config, false);
        assert_eq!(text(&cold, 120), output);
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join("self-profile/self_directed/head.json")
            )
            .unwrap(),
            bytes
        );
        let filtered = panel
            .lines(120, "leaves")
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(filtered.contains("Why do leaves differ?"));
        assert!(!filtered.contains("Compare sources"));
    }

    #[test]
    fn large_profile_pages_are_complete_and_recent_history_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path(), Variant::SelfDirected);
        let entry = format!("{} LAST_UNICODE_🙂", "An observation. ".repeat(3000));
        update(
            directory.path(),
            0,
            "A larger saved description",
            json!({"tendencies":[entry.clone(),entry]}),
        );
        for revision in 1..8 {
            update(
                directory.path(),
                revision,
                &format!("Change {revision}"),
                json!({"interests":[format!("Interest {revision}")]}),
            );
        }
        let mut panel = ProfilePanel::default();
        panel.poll(&config, false);
        assert!(panel.error.is_empty(), "{}", panel.error);
        assert_eq!(panel.revision, Some(8));
        assert_eq!(panel.recent.len(), 5);
        assert_eq!(panel.recent[0]["revision"], 8);
        assert_eq!(panel.recent[4]["revision"], 4);
        let saved = panel.record.as_ref().unwrap()["profile"]["tendencies"]
            .as_array()
            .unwrap();
        assert_eq!(saved.len(), 2);
        assert!(saved[1].as_str().unwrap().ends_with("LAST_UNICODE_🙂"));
        assert!(
            serde_json::to_vec(panel.record.as_ref().unwrap())
                .unwrap()
                .len()
                > 65536
        );
    }

    #[test]
    fn profile_tab_is_available_only_for_unassigned_sessions_and_wraps_at_small_sizes() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path(), Variant::SelfDirected);
        update(
            directory.path(),
            0,
            "A chosen tendency",
            json!({"tendencies":["Inspect before deciding"]}),
        );
        for width in [42, 60, 80, 110] {
            let labels = dashboard_tabs(true, 7, width)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(labels.contains("8 Profile"));
            assert!(labels.lines().all(|line| line.chars().count() <= width));
            assert!(
                !dashboard_tabs(false, 0, width)
                    .iter()
                    .any(|line| line.to_string().contains("8 Profile"))
            );
        }
        for (width, height) in [(130, 42), (110, 36), (80, 24), (46, 24)] {
            let mut dashboard = Dashboard::new(&config);
            dashboard.tab = 7;
            dashboard.follow = false;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_dashboard(frame, &mut dashboard, &config, false))
                .unwrap();
            let output = screen(&terminal);
            assert!(output.contains("8 Profile"));
            assert!(output.contains("Self-profile"));
            assert!(output.contains("Self-directed"));
            if width >= 110 {
                assert!(output.contains("Inspect before deciding"), "{output}");
            }
            assert_eq!(dashboard.calls, 0);
        }
        assert_eq!(dashboard_tab_count(true), 8);
        assert_eq!(dashboard_tab_count(false), 7);
    }
}
