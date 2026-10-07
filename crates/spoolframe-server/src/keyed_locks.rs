//! Async exclusion per key, including cancellation-safe removal of idle locks.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Debug, Default)]
pub(crate) struct KeyedLocks {
    entries: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
}

impl KeyedLocks {
    pub async fn lock(&self, key: &str) -> Guard<'_> {
        let lock = {
            let mut entries = self.entries.lock();
            match entries.get(key).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    entries.insert(key.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        // Register cleanup before waiting: cancelling a waiter must also
        // release its reference and remove the entry if it was the last.
        let mut lease = Guard {
            owner: self,
            key: key.to_owned(),
            lock,
            held: None,
        };
        lease.held = Some(lease.lock.clone().lock_owned().await);
        lease
    }
}

pub(crate) struct Guard<'a> {
    owner: &'a KeyedLocks,
    key: String,
    lock: Arc<AsyncMutex<()>>,
    held: Option<OwnedMutexGuard<()>>,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.held.take();
        let mut entries = self.owner.entries.lock();
        if Arc::strong_count(&self.lock) == 1 {
            entries.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_waiters_do_not_remove_a_held_lock() {
        let locks = KeyedLocks::default();
        let first = locks.lock("image").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), locks.lock("image"))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), locks.lock("image"))
                .await
                .is_err()
        );
        let other = locks.lock("another image").await;
        drop(other);
        assert_eq!(locks.entries.lock().len(), 1);
        drop(first);
        assert!(locks.entries.lock().is_empty());
        drop(locks.lock("image").await);
        assert!(locks.entries.lock().is_empty());
    }
}
