//! What Server › Optimized (PX-37) asks of the pool: its place and
//! budget, what the budget counts, who reads a copy or track, and
//! "Delete them now" for outdated copies.

use std::path::Path;

use super::{Deleted, Pool};
use crate::store::StoreError;
use crate::store::optimized::CopyTarget;

/// What [`Pool::delete_outdated`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutdatedDeleted {
    /// Copies gone, with their files.
    pub now: u64,
    /// Copies being read or made, which go once they are not.
    pub deferred: u64,
}

impl Pool {
    /// Where the copies are kept.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The most the copies may take, in bytes.
    pub fn max_size(&self) -> u64 {
        self.inner.max_size
    }

    /// What the budget counts: the tracks made, and the one being made at
    /// what it is written to or expected to grow to, whichever is more, as
    /// the sweep counts them.
    pub async fn used_bytes(&self) -> Result<u64, StoreError> {
        let tracks = self.inner.store.optimized_tracks().await?;
        Ok(self.inner.used(&tracks).await)
    }

    /// Whether copy or track `id` is lent to a reader now.
    pub fn is_read(&self, id: &str) -> bool {
        self.inner.state.lock().leases.contains_key(id)
    }

    /// Delete the outdated copies: profile `profile`'s, or every one
    /// when `None`. One being read goes when its last reader is done.
    pub async fn delete_outdated(
        &self,
        profile: Option<&str>,
    ) -> Result<OutdatedDeleted, StoreError> {
        let mut done = OutdatedDeleted::default();
        for copy in self.inner.store.optimized_copies().await? {
            let wanted = match (&copy.target, profile) {
                (_, None) => true,
                (CopyTarget::Profile(name), Some(profile)) => name == profile,
                (CopyTarget::Ahead | CopyTarget::Original { .. }, Some(_)) => false,
            };
            if !copy.outdated || !wanted {
                continue;
            }
            match self.delete(&copy.id).await? {
                Deleted::Now => done.now += 1,
                Deleted::Deferred => done.deferred += 1,
                Deleted::NotFound => {}
            }
        }
        Ok(done)
    }
}
