//! Process-wide shutdown signal.
//!
//! Long sleeps deep in helper code (e.g. GitHub rate-limit backoff in
//! `github.rs`) can't see the `gru lab` daemon's local shutdown flag. Signal
//! handlers call [`request_shutdown`] and those sleeps race against
//! [`global`] via [`ShutdownSignal::sleep`], so they return early instead of
//! holding the daemon open for up to a minute after Ctrl-C.
//!
//! Nothing triggers the global signal outside of `gru lab`'s signal handlers,
//! so other commands are unaffected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Result of [`ShutdownSignal::sleep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum SleepOutcome {
    /// The full duration elapsed.
    Elapsed,
    /// Shutdown was requested before the duration elapsed.
    Cancelled,
}

/// A one-shot, latching shutdown signal that any number of tasks can wait on.
pub(crate) struct ShutdownSignal {
    triggered: AtomicBool,
    notify: Notify,
}

impl ShutdownSignal {
    /// Create an untriggered signal. `const` so it can back a `static`.
    pub(crate) const fn new() -> Self {
        Self {
            triggered: AtomicBool::new(false),
            notify: Notify::const_new(),
        }
    }

    /// Latch the signal and wake every current waiter.
    pub(crate) fn trigger(&self) {
        self.triggered.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether [`trigger`](Self::trigger) has been called.
    pub(crate) fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    /// Wait until the signal is triggered. Returns immediately if it already was.
    pub(crate) async fn wait(&self) {
        loop {
            // Register interest before checking the flag so a trigger that
            // lands between the check and the await isn't missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_triggered() {
                return;
            }
            notified.await;
        }
    }

    /// Sleep for `duration`, returning early if the signal is triggered.
    pub(crate) async fn sleep(&self, duration: Duration) -> SleepOutcome {
        tokio::select! {
            biased;
            _ = self.wait() => SleepOutcome::Cancelled,
            _ = tokio::time::sleep(duration) => SleepOutcome::Elapsed,
        }
    }
}

static GLOBAL: ShutdownSignal = ShutdownSignal::new();

/// The process-wide shutdown signal.
pub(crate) fn global() -> &'static ShutdownSignal {
    &GLOBAL
}

/// Trigger the process-wide shutdown signal.
///
/// This is a one-way latch for the whole process. After it fires,
/// `github::run_gh` and `github::gh_api_with_retry` refuse to start new
/// requests and kill in-flight ones (both return `Err`), and retries are
/// skipped. Code that must still reach GitHub during
/// teardown (e.g. posting a final status comment) has to run before this is
/// called or use a path other than those helpers.
pub(crate) fn request_shutdown() {
    GLOBAL.trigger();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn sleep_elapses_when_not_triggered() {
        let signal = ShutdownSignal::new();
        let start = tokio::time::Instant::now();
        let outcome = signal.sleep(Duration::from_secs(60)).await;
        assert_eq!(outcome, SleepOutcome::Elapsed);
        assert!(start.elapsed() >= Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_returns_immediately_when_already_triggered() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        let start = tokio::time::Instant::now();
        let outcome = signal.sleep(Duration::from_secs(60)).await;
        assert_eq!(outcome, SleepOutcome::Cancelled);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn trigger_cancels_in_progress_sleep() {
        let signal = Arc::new(ShutdownSignal::new());
        let sleeper = {
            let signal = signal.clone();
            tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                let outcome = signal.sleep(Duration::from_secs(60)).await;
                (outcome, start.elapsed())
            })
        };

        tokio::time::sleep(Duration::from_secs(1)).await;
        signal.trigger();

        let (outcome, elapsed) = sleeper.await.unwrap();
        assert_eq!(outcome, SleepOutcome::Cancelled);
        assert!(elapsed < Duration::from_secs(2), "elapsed = {elapsed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn trigger_wakes_all_waiters() {
        let signal = Arc::new(ShutdownSignal::new());
        let sleepers: Vec<_> = (0..3)
            .map(|_| {
                let signal = signal.clone();
                tokio::spawn(async move { signal.sleep(Duration::from_secs(60)).await })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(10)).await;
        signal.trigger();

        for s in sleepers {
            assert_eq!(s.await.unwrap(), SleepOutcome::Cancelled);
        }
    }
}
