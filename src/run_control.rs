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
    paused: AtomicBool,
    clock: Mutex<Clock>,
    wake: Condvar,
    provider: Mutex<ProviderWait>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
}
impl RunControl {
    pub fn bind_stop(&self, stop: Arc<AtomicBool>) {
        *self.stop.lock().unwrap() = Some(stop);
    }
    pub fn pause_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
    pub fn is_paused(&self) -> bool {
        self.pause_requested() && self.paused.load(Ordering::SeqCst)
    }
    pub fn toggle_pause(&self) -> bool {
        if self.pause_requested() {
            self.resume();
            false
        } else {
            self.requested.store(true, Ordering::SeqCst);
            self.wake.notify_all();
            true
        }
    }
    pub fn resume(&self) {
        self.requested.store(false, Ordering::SeqCst);
        let mut clock = self.clock.lock().unwrap();
        if let Some(start) = clock.started.take() {
            clock.intervals.push((start, Instant::now()));
        }
        self.paused.store(false, Ordering::SeqCst);
        self.wake.notify_all();
    }
    pub fn wait_until_resumed(&self, should_release: impl Fn() -> bool) {
        while self.pause_requested() {
            let stopping = self
                .stop
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.load(Ordering::SeqCst));
            if stopping || should_release() {
                self.resume();
                break;
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
