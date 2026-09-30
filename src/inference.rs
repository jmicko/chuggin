//! Requests share capacity; a paused agent never holds an inference slot.
use anyhow::Result;
use std::{
    collections::HashMap,
    fs::{self, File},
    hash::{Hash, Hasher},
    sync::{Condvar, LazyLock, Mutex},
    time::Duration,
};
#[derive(Default)]
struct Resource {
    active: bool,
    interactive: usize,
}
static QUEUE: LazyLock<(Mutex<HashMap<String, Resource>>, Condvar)> =
    LazyLock::new(Default::default);
pub struct Permit {
    key: String,
    _file: File,
}
impl Drop for Permit {
    fn drop(&mut self) {
        QUEUE.0.lock().unwrap().get_mut(&self.key).unwrap().active = false;
        QUEUE.1.notify_all();
    }
}
struct Waiting {
    key: String,
    interactive: bool,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        if self.interactive {
            QUEUE
                .0
                .lock()
                .unwrap()
                .get_mut(&self.key)
                .unwrap()
                .interactive -= 1;
        }
        QUEUE.1.notify_all();
    }
}
pub fn acquire(
    key: &str,
    controls: &crate::run_control::RunControl,
    stopped: &std::sync::atomic::AtomicBool,
) -> Result<Permit> {
    let interactive = interactive_actor(&crate::events::actor());
    {
        let mut q = QUEUE.0.lock().unwrap();
        let r = q.entry(key.into()).or_default();
        if interactive {
            r.interactive += 1;
        }
    }
    let _waiting = Waiting {
        key: key.into(),
        interactive,
    };
    let base = crate::setup::settings_path()?
        .parent()
        .unwrap()
        .join("inference-locks");
    fs::create_dir_all(&base)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.trim_end_matches('/').hash(&mut hash);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join(format!("{:016x}.lock", hash.finish())))?;
    loop {
        controls.wait_until_resumed(|| stopped.load(std::sync::atomic::Ordering::SeqCst));
        if stopped.load(std::sync::atomic::Ordering::SeqCst) || controls.stopped_while_held() {
            return Err(crate::provider::Stopped(
                "Request cancelled while waiting for inference".into(),
            )
            .into());
        }
        let mut map = QUEUE.0.lock().unwrap();
        let r = map.get_mut(key).unwrap();
        let locked = if !r.active && (interactive || r.interactive == 0) {
            match file.try_lock() {
                Ok(()) => true,
                Err(std::fs::TryLockError::WouldBlock) => false,
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        } else {
            false
        };
        if locked {
            // Recheck after admission; don't return a permit to an already-held worker.
            if controls.pause_requested() {
                let _ = file.unlock();
            } else {
                r.active = true;
                drop(map);
                return Ok(Permit {
                    key: key.into(),
                    _file: file,
                });
            }
        }
        drop(
            QUEUE
                .1
                .wait_timeout(map, Duration::from_millis(100))
                .unwrap(),
        );
    }
}

/// User conversations may interrupt background inference queues. An investigation
/// is work requested by the loop, so it must not inherit user-chat priority merely
/// because its output is recorded under a separate actor name.
fn interactive_actor(actor: &str) -> bool {
    actor != "loop" && !actor.starts_with("agent/")
}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_actors_do_not_jump_the_user_queue() {
        assert!(!super::interactive_actor("loop"));
        assert!(!super::interactive_actor("agent/investigate-42"));
        assert!(super::interactive_actor("operator-session-42"));
    }
}
