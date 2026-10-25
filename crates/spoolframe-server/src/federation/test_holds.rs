//! Where a test holds the federation mid-way through a race: each
//! [`Hold`] is a no-op until a test arms it, then the next time the
//! federation reaches it, it says so and waits to be let go.

/// A test's hold on the next redemption, dial, sync, mint, incoming
/// connection, answer or unpairing where the federation holds one: it
/// says it is there, and waits to go on.
#[derive(Debug, Default)]
pub(super) struct Hold(
    parking_lot::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
);

impl Hold {
    /// Hold the next one: what says it is held there, and what lets it go
    /// on.
    pub(super) fn next(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, held) = tokio::sync::oneshot::channel();
        let (go, going) = tokio::sync::oneshot::channel();
        *self.0.lock() = Some((reached, going));
        (held, go)
    }

    pub(super) async fn here(&self) {
        let held = self.0.lock().take();
        if let Some((reached, go)) = held {
            let _ = reached.send(());
            let _ = go.await;
        }
    }
}

/// Every hold the federation has, one per race the tests run.
#[derive(Debug, Default)]
pub(super) struct Holds {
    /// A redemption once its token is used up, before its session opens:
    /// the moment a revocation races.
    pub(super) redeeming: Hold,
    /// A dial once the pair answered, before its connection is kept; a
    /// sync once its pages are read, before they are written; and a mint
    /// once its assertion is checked, before its token is kept: the
    /// moments a key confirmed races (C8-01).
    pub(super) dialing: Hold,
    pub(super) committing: Hold,
    pub(super) minting: Hold,
    /// An incoming connection once its pair is looked up, before it goes
    /// on; and a sync once its request is checked, before its library is
    /// walked: the moments a key confirmed races on the dialled side
    /// (X8-01).
    pub(super) knowing: Hold,
    pub(super) answering: Hold,
    /// A pair's unpairing, once the pair is revoked and before this server
    /// has cleaned up after it, then again at the end: where its
    /// connection closing races the cleanup.
    pub(super) unpairing: Hold,
}
