//! Civil-time windows. Pure evaluation is separate from request admission.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Schedule {
    #[default]
    Always,
    Shared,
    Custom {
        window: Window,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub start: String,
    pub end: String,
    pub timezone: String,
    /// Monday = 0. An overnight interval belongs to its opening day.
    pub days: Vec<u32>,
    #[serde(default)]
    pub closing: Closing,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Closing {
    #[default]
    Call,
    Cycle,
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub open: bool,
    pub next_transition: Option<DateTime<Utc>>,
    pub closing: Closing,
    pub description: String,
}
impl Default for Window {
    fn default() -> Self {
        Self {
            start: "01:00".into(),
            end: "06:00".into(),
            timezone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
            days: (0..7).collect(),
            closing: Closing::Call,
        }
    }
}
fn minutes(text: &str) -> Result<u32> {
    let (h, m) = text
        .split_once(':')
        .context("Use HH:MM, for example 01:00")?;
    let h: u32 = h.parse()?;
    let m: u32 = m.parse()?;
    ensure!(h < 24 && m < 60, "Hours must be 00:00–23:59");
    Ok(h * 60 + m)
}
fn boundary(zone: Tz, mut local: NaiveDateTime, closing: bool) -> Result<DateTime<Utc>> {
    // DST gaps, including skipped civil days. Never assume an offset change is an hour.
    for _ in 0..=2880 {
        match zone.from_local_datetime(&local) {
            LocalResult::Single(t) => return Ok(t.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) => {
                return Ok(if closing { a.max(b) } else { a.min(b) }.with_timezone(&Utc));
            }
            LocalResult::None => local += Duration::minutes(1),
        }
    }
    anyhow::bail!("Cannot resolve the schedule boundary in {zone}")
}
impl Window {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            minutes(&self.start)? != minutes(&self.end)?,
            "Choose different opening and closing times, or Always allowed"
        );
        self.timezone
            .parse::<Tz>()
            .context("Choose a named time zone, for example America/Chicago")?;
        ensure!(
            !self.days.is_empty() && self.days.iter().all(|d| *d < 7),
            "Select at least one weekday (Monday=0 through Sunday=6)"
        );
        Ok(())
    }
    pub fn at(&self, now: DateTime<Utc>) -> Result<Status> {
        self.validate()?;
        let zone: Tz = self.timezone.parse()?;
        let start = minutes(&self.start)?;
        let end = minutes(&self.end)?;
        let date = now.with_timezone(&zone).date_naive();
        let mut intervals: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        for delta in -2..=9 {
            let day = date + Duration::days(delta);
            if !self.days.contains(&day.weekday().num_days_from_monday()) {
                continue;
            }
            let end_day = day + Duration::days(i64::from(end < start));
            let a = boundary(
                zone,
                day.and_hms_opt(start / 60, start % 60, 0).unwrap(),
                false,
            )?;
            let b = boundary(
                zone,
                end_day.and_hms_opt(end / 60, end % 60, 0).unwrap(),
                true,
            )?;
            if b <= a {
                continue;
            }
            if let Some(last) = intervals.last_mut()
                && a <= last.1
            {
                last.1 = last.1.max(b);
            } else {
                intervals.push((a, b));
            }
        }
        let active = intervals.iter().find(|(a, b)| *a <= now && now < *b);
        let next = active
            .map(|(_, b)| *b)
            .or_else(|| intervals.iter().find(|(a, _)| *a > now).map(|(a, _)| *a));
        let transition = next
            .map(|t| t.with_timezone(&zone).format("%a %H:%M %Z").to_string())
            .unwrap_or_default();
        Ok(Status {
            open: active.is_some(),
            next_transition: next,
            closing: self.closing,
            description: if active.is_some() {
                format!("Active hours until {transition}")
            } else {
                format!("Outside active hours · resumes {transition}")
            },
        })
    }
}
impl Schedule {
    pub fn at(&self, now: DateTime<Utc>) -> Result<Status> {
        match self {
            Self::Always => Ok(Status {
                open: true,
                next_transition: None,
                closing: Closing::Call,
                description: "Always allowed".into(),
            }),
            Self::Custom { window } => window.at(now),
            Self::Shared => {
                let shared = crate::setup::settings()?.active_hours;
                ensure!(
                    !matches!(shared, Self::Shared),
                    "Shared hours cannot inherit themselves"
                );
                shared.at(now)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn at(w: &Window, t: &str) -> Status {
        w.at(t.parse().unwrap()).unwrap()
    }
    #[test]
    fn overnight_weekday_and_exclusive_close() {
        let w = Window {
            start: "22:00".into(),
            end: "06:00".into(),
            timezone: "UTC".into(),
            days: vec![0],
            closing: Closing::Call,
        };
        assert!(!at(&w, "2026-09-28T21:59:59Z").open);
        assert!(at(&w, "2026-09-28T22:00:00Z").open);
        assert!(at(&w, "2026-09-29T05:59:59Z").open);
        assert!(!at(&w, "2026-09-29T06:00:00Z").open);
        assert!(!at(&w, "2026-09-29T23:00:00Z").open);
    }
    #[test]
    fn daylight_savings_gap_and_repeat() {
        let mut w = Window {
            timezone: "America/Chicago".into(),
            start: "01:30".into(),
            end: "02:30".into(),
            ..Window::default()
        };
        assert!(at(&w, "2026-03-08T07:30:00Z").open);
        assert_eq!(
            at(&w, "2026-03-08T07:30:00Z")
                .next_transition
                .unwrap()
                .to_rfc3339(),
            "2026-03-08T08:00:00+00:00"
        );
        w.end = "01:45".into();
        assert!(at(&w, "2026-11-01T06:40:00Z").open);
        assert!(at(&w, "2026-11-01T07:20:00Z").open);
        assert!(!at(&w, "2026-11-01T07:45:00Z").open);
    }
    #[test]
    fn invalid_windows_and_half_hour_dst() {
        let mut w = Window {
            start: "01:00".into(),
            end: "01:00".into(),
            ..Window::default()
        };
        assert!(w.validate().is_err());
        w.timezone = "Australia/Lord_Howe".into();
        w.start = "02:15".into();
        w.end = "03:00".into();
        assert!(at(&w, "2026-10-03T15:30:00Z").open);
        assert!(!at(&w, "2026-10-03T16:00:00Z").open);
    }
}
