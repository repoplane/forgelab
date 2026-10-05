//! A line every so often during a long phase: how far along it is, how fast it is going, and
//! whether the forge has asked it to wait. A fleet of thousands on a rate-limited forge applies
//! for hours, and silence for hours is indistinguishable from a hang.
//!
//! It goes to stderr, so what a command prints on stdout -- the plan, the verdict -- is the
//! same whether or not a run was long enough to report on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;

use crate::forge::http::rate_limited_for;
use crate::util::go_duration;

/// How often a running phase says where it is. A phase shorter than this says nothing.
pub const EVERY: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Progress {
    phase: Mutex<Option<(String, Instant)>>,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl Progress {
    /// Starts reporting on behalf of `forge` -- named in the rate-limit note -- until the
    /// returned handle is dropped. Outside a tokio runtime it only counts.
    pub fn start(forge: &str) -> Arc<Progress> {
        let p = Arc::new(Progress::default());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let weak: Weak<Progress> = Arc::downgrade(&p);
            let forge = forge.to_string();
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(EVERY).await;
                    let Some(p) = weak.upgrade() else { return };
                    if let Some(line) = p.line(&forge) {
                        eprintln!("{line}");
                    }
                }
            });
        }
        p
    }

    /// A new phase of `total` steps. A phase of none is no phase.
    pub fn phase(&self, label: &str, total: usize) {
        self.done.store(0, Ordering::SeqCst);
        self.total.store(total, Ordering::SeqCst);
        *self.phase.lock().unwrap_or_else(|p| p.into_inner()) =
            (total > 0).then(|| (label.to_string(), Instant::now()));
    }

    pub fn add(&self, n: usize) {
        self.done.fetch_add(n, Ordering::SeqCst);
    }

    /// `  apply   345/2016 · 1h20m · 259/h · about 6h27m left · GitHub rate limit, 41s more`
    fn line(&self, forge: &str) -> Option<String> {
        let (label, started) = self
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()?;
        let (done, total) = (
            self.done.load(Ordering::SeqCst),
            self.total.load(Ordering::SeqCst),
        );
        let elapsed = started.elapsed();
        let mut line = format!(
            "  {label:<7} {done}/{total} · {}",
            go_duration(round(elapsed))
        );
        if done > 0 && done < total {
            let per_step = elapsed / done as u32;
            let per_hour = Duration::from_secs(3600).as_secs_f64() / per_step.as_secs_f64();
            line.push_str(&format!(
                " · {}/h · about {} left",
                per_hour.round() as u64,
                go_duration(round(per_step * (total - done) as u32))
            ));
        }
        if let Some(wait) = rate_limited_for() {
            line.push_str(&format!(
                " · {forge} rate limit, {} more",
                go_duration(round(wait))
            ));
        }
        Some(line)
    }
}

/// To the second: a progress line has no use for milliseconds.
fn round(d: Duration) -> Duration {
    Duration::from_secs(d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn says_how_far_how_fast_and_how_long() {
        let p = Progress::default();
        assert_eq!(p.line("GitHub"), None, "no phase, nothing to say");
        p.phase("apply", 100);
        tokio::time::advance(Duration::from_secs(60)).await;
        p.add(10);
        let line = p.line("GitHub").unwrap();
        assert!(
            line.starts_with("  apply   10/100 · 1m0s · 600/h · about 9m0s left"),
            "{line}"
        );
        p.phase("check", 0);
        assert_eq!(p.line("GitHub"), None, "an empty phase is no phase");
    }
}
