//! In-process observation controls; gates preserve the worker's exact continuation.
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Clock {
    started: Option<Instant>,
    intervals: Vec<(Instant, Instant)>,
}
#[derive(Default)]
struct ProviderWait {
    waiting: bool,
    retry: bool,
}
#[derive(Default)]
pub struct RunControl {
    requested: AtomicBool,
    remote: Mutex<Option<serde_json::Value>>,
    pub owns_project: AtomicBool,
    pub changed: std::sync::atomic::AtomicU64,
    holds: Mutex<std::collections::BTreeMap<String, String>>,
    config: Mutex<Option<std::path::PathBuf>>,
    cycle: AtomicBool,
    override_cycle: AtomicBool,
    finish_cycle: AtomicBool,
    override_until: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    stopped_held: AtomicBool,
    paused: AtomicBool,
    clock: Mutex<Clock>,
    wake: Condvar,
    provider: Mutex<ProviderWait>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
}
impl RunControl {
    pub fn observe_remote(&self, value: serde_json::Value) {
        if value["paused"] == true {
            self.clock
                .lock()
                .unwrap()
                .started
                .get_or_insert_with(Instant::now);
        } else {
            self.end_hold();
        }
        *self.remote.lock().unwrap() = Some(value);
    }
    pub fn reset_stopped(&self) {
        self.stopped_held.store(false, Ordering::SeqCst);
    }
    pub fn configure(&self, path: &std::path::Path) {
        *self.config.lock().unwrap() = Some(path.into());
        if let Ok(c) = crate::runner::load(path)
            && let Ok(v) =
                crate::operator::read_json(&c.state_dir.join("operator/manual-pause.json"))
        {
            self.requested.store(v["paused"] == true, Ordering::SeqCst);
        }
    }
    pub fn cycle_active(&self, active: bool) {
        self.cycle.store(active, Ordering::SeqCst);
        if !active {
            self.override_cycle.store(false, Ordering::SeqCst);
        }
    }
    pub fn respect_schedule(&self) {
        self.override_cycle.store(false, Ordering::SeqCst);
        self.finish_cycle.store(false, Ordering::SeqCst);
        *self.override_until.lock().unwrap() = None;
    }
    pub fn allow_one_cycle(&self) {
        *self.override_until.lock().unwrap() = None;
        self.override_cycle.store(true, Ordering::SeqCst);
        self.finish_cycle.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }
    pub fn finish_requested_cycle(&self) -> bool {
        self.finish_cycle.swap(false, Ordering::SeqCst)
    }
    pub fn resume_outside_hours(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let schedule = self
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| crate::runner::load(p).map(|c| c.active_hours))
            .transpose()?
            .unwrap_or_default();
        let status = schedule.at(now)?;
        if !status.open
            && let Some(opening) = status.next_transition
        {
            *self.override_until.lock().unwrap() = schedule.at(opening)?.next_transition;
        }
        self.resume();
        Ok(())
    }
    pub fn hold(&self, id: &str, reason: &str) {
        self.holds.lock().unwrap().insert(id.into(), reason.into());
        self.wake.notify_all();
    }
    pub fn release(&self, id: &str) {
        self.holds.lock().unwrap().remove(id);
        self.wake.notify_all();
    }
    pub fn manual_pause(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
    pub fn stopped_while_held(&self) -> bool {
        self.stopped_held.load(Ordering::SeqCst)
    }
    pub fn schedule_status(&self) -> anyhow::Result<crate::schedule::Status> {
        let path = self.config.lock().unwrap().clone();
        let schedule = match path {
            Some(p) => crate::runner::load(&p)?.active_hours,
            None => crate::schedule::Schedule::Always,
        };
        schedule.at(chrono::Utc::now())
    }
    pub fn reasons(&self) -> Vec<String> {
        if let Some(v) = self.remote.lock().unwrap().as_ref() {
            return v["holds"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
        }
        let mut reasons: Vec<_> = self.holds.lock().unwrap().values().cloned().collect();
        if self.manual_pause() {
            reasons.push("Paused by you".into());
        }
        match self.schedule_status() {
            Ok(s)
                if !s.open
                    && !self.override_cycle.load(Ordering::SeqCst)
                    && !self
                        .override_until
                        .lock()
                        .unwrap()
                        .is_some_and(|t| chrono::Utc::now() < t) =>
            {
                let finishing = s.closing == crate::schedule::Closing::Cycle
                    && self.cycle.load(Ordering::SeqCst)
                    && !self.provider.lock().unwrap().waiting;
                if !finishing {
                    reasons.push(s.description);
                }
            }
            Err(e) => reasons.push(format!("Active hours need attention: {e}")),
            _ => {}
        }
        reasons
    }
    pub fn bind_stop(&self, stop: Arc<AtomicBool>) {
        *self.stop.lock().unwrap() = Some(stop);
    }
    pub fn pause_requested(&self) -> bool {
        !self.reasons().is_empty()
    }
    pub fn is_paused(&self) -> bool {
        if let Some(v) = self.remote.lock().unwrap().as_ref() {
            return v["paused"] == true;
        }
        self.pause_requested() && self.paused.load(Ordering::SeqCst)
    }
    fn persist_pause(&self, paused: bool) {
        if let Some(path) = self.config.lock().unwrap().as_ref()
            && let Ok(c) = crate::runner::load(path)
        {
            let _ = crate::setup::save(
                &c.state_dir.join("operator/manual-pause.json"),
                &serde_json::json!({"paused":paused}),
            );
        }
    }
    pub fn toggle_pause(&self) -> bool {
        if self.manual_pause() {
            self.resume();
            false
        } else {
            self.requested.store(true, Ordering::SeqCst);
            self.persist_pause(true);
            self.wake.notify_all();
            true
        }
    }
    pub fn resume(&self) {
        self.requested.store(false, Ordering::SeqCst);
        self.persist_pause(false);
        if !self.pause_requested() {
            self.end_hold();
        }
        self.wake.notify_all();
    }
    fn end_hold(&self) {
        let mut clock = self.clock.lock().unwrap();
        if let Some(start) = clock.started.take() {
            clock.intervals.push((start, Instant::now()));
        }
        self.paused.store(false, Ordering::SeqCst);
        self.wake.notify_all();
    }
    pub fn wait_until_resumed(&self, should_release: impl Fn() -> bool) {
        loop {
            while self.pause_requested() {
                let stopping = self
                    .stop
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::SeqCst));
                if (stopping && crate::project::active_checks().is_empty()) || should_release() {
                    self.stopped_held.store(true, Ordering::SeqCst);
                    self.requested.store(false, Ordering::SeqCst);
                    self.end_hold();
                    return;
                }
                let mut clock = self.clock.lock().unwrap();
                if !self.pause_requested() {
                    break;
                }
                clock.started.get_or_insert_with(Instant::now);
                self.paused.store(true, Ordering::SeqCst);
                drop(
                    self.wake
                        .wait_timeout(clock, Duration::from_millis(100))
                        .unwrap(),
                );
            }
            self.end_hold();
            if !self.pause_requested() {
                break;
            }
        }
    }
    pub fn active_elapsed(&self, since: Instant) -> Duration {
        let clock = self.clock.lock().unwrap();
        let now = Instant::now();
        let mut held = Duration::ZERO;
        for &(start, end) in &clock.intervals {
            held += end.saturating_duration_since(start.max(since));
        }
        if let Some(start) = clock.started {
            held += now.saturating_duration_since(start.max(since));
        }
        now.saturating_duration_since(since).saturating_sub(held)
    }
    pub fn provider_wait_started(&self) {
        *self.provider.lock().unwrap() = ProviderWait {
            waiting: true,
            retry: false,
        };
    }
    pub fn provider_wait_finished(&self) {
        *self.provider.lock().unwrap() = ProviderWait::default();
    }
    pub fn retry_now(&self) -> bool {
        let mut wait = self.provider.lock().unwrap();
        if !wait.waiting {
            return false;
        }
        wait.retry = true;
        self.wake.notify_all();
        true
    }
    pub fn take_retry(&self) -> bool {
        std::mem::take(&mut self.provider.lock().unwrap().retry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_is_one_shot_and_cannot_leak_into_another_wait() {
        let c = RunControl::default();
        assert!(!c.retry_now());
        c.provider_wait_started();
        assert!(c.retry_now());
        assert!(c.retry_now());
        assert!(c.take_retry());
        assert!(!c.take_retry());
        c.retry_now();
        c.provider_wait_finished();
        c.provider_wait_started();
        assert!(!c.take_retry());
    }
    #[test]
    fn pause_freezes_active_time_and_soft_stop_releases_it() {
        let c = Arc::new(RunControl::default());
        let start = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        c.bind_stop(stop.clone());
        c.toggle_pause();
        assert!(!c.is_paused());
        let worker = c.clone();
        let thread = std::thread::spawn(move || worker.wait_until_resumed(|| false));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !c.is_paused() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let active = c.active_elapsed(start);
        let during = Instant::now();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(c.active_elapsed(start), active);
        assert_eq!(c.active_elapsed(during), Duration::ZERO);
        stop.store(true, Ordering::SeqCst);
        thread.join().unwrap();
        assert!(!c.pause_requested());
        assert!(!c.is_paused());
    }
}
