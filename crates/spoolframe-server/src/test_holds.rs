//! Where a test holds the server's work mid-way, to race it: each
//! [`Held`] is a no-op until a test arms it, then every piece of work that
//! reaches it says so and waits to be let go. Built for `cfg(test)` and
//! for the `test-support` feature the integration tests turn on, as
//! [`crate::test_support`] is.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::{Notify, watch};

/// A hold on the work that reaches it: how many are at it now, how many
/// went on to their end, and what lets them go on.
#[derive(Debug)]
pub struct Held {
    holding: AtomicBool,
    at_it: watch::Sender<usize>,
    go_on: Notify,
    done: AtomicUsize,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            holding: AtomicBool::new(false),
            at_it: watch::Sender::new(0),
            go_on: Notify::new(),
            done: AtomicUsize::new(0),
        }
    }
}

impl Held {
    /// From now on, what reaches this waits here.
    pub fn hold(&self) {
        self.holding.store(true, Ordering::Release);
    }

    /// Wait here while held, counted among those at it until let go or
    /// dropped: the server's side.
    pub async fn wait(&self) {
        if !self.holding.load(Ordering::Acquire) {
            return;
        }
        struct Leaving<'a>(&'a Held);
        impl Drop for Leaving<'_> {
            fn drop(&mut self) {
                self.0.at_it.send_modify(|n| *n -= 1);
            }
        }
        self.at_it.send_modify(|n| *n += 1);
        let _leaving = Leaving(self);
        self.go_on.notified().await;
        self.done.fetch_add(1, Ordering::AcqRel);
    }

    /// Until `n` are at it, within 5 s.
    pub async fn until_at_it(&self, n: usize) {
        let mut at_it = self.at_it.subscribe();
        let waited = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            at_it.wait_for(|at| *at == n),
        )
        .await;
        assert!(waited.is_ok(), "{} at it, not {n}", *self.at_it.borrow());
    }

    /// Let those at it go on; what reaches it after is held still.
    pub fn release(&self) {
        self.go_on.notify_waiters();
    }

    /// Let those at it go on, and hold nothing from now on.
    pub fn let_go(&self) {
        self.holding.store(false, Ordering::Release);
        self.go_on.notify_waiters();
    }

    /// How many went on to their end.
    pub fn done(&self) -> usize {
        self.done.load(Ordering::Acquire)
    }
}
