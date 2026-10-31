//! One player's playbacks, one after another ([`PlayerSession`]).

use super::{ManagedPlayback, RestartCause};
use crate::{Playback, PlaybackBackend, PlaybackOptions, SdkError, Session};
use parking_lot::Mutex;
use spoolframe_protocol::Selection;
use spoolframe_types::MediaId;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a player plays, started, replaced, restarted and ended in the
/// order asked: each waits for the one before. A start that a later
/// start or end overtook while it was asked is ended as its answer comes,
/// and fails with [`SdkError::PlaybackClosed`]: a slow server's late
/// reply never plays over what the viewer picked since.
///
/// The order is that of the calls to [`Self::start`] and [`Self::end`],
/// not of their polls, so make those calls where the viewer asks: a call
/// inside a spawned task is in the order the tasks run. Their futures own
/// what they need, to be spawned as they are. One dropped unpolled still
/// overtakes the start before it, and holds up nothing: the viewer asked
/// for something newer, and what was asked is not waited for.
#[derive(Clone)]
pub struct PlayerSession(Arc<Inner>);

/// How a player's items open.
enum Opener {
    /// All through one backend: a mock, or an app's own.
    Backend(Arc<dyn PlaybackBackend>),
    /// Each by what it is: a paired server's by its route.
    Session(Session),
}

impl Opener {
    async fn open(
        &self,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> Result<ManagedPlayback, SdkError> {
        match self {
            Self::Backend(backend) => {
                ManagedPlayback::open(backend.clone(), item, selection, options).await
            }
            Self::Session(session) => session.open_any_playback(&item, selection, options).await,
        }
    }
}

struct Inner {
    opener: Opener,
    /// Held through each step, so they run in order.
    line: tokio::sync::Mutex<()>,
    /// One more for each start and end asked: a start whose number is not
    /// the last one asked was overtaken.
    asked: AtomicU64,
    current: Mutex<Option<Playing>>,
}

struct Playing {
    /// The start's number: only what was asked after it ends it.
    asked: u64,
    playback: ManagedPlayback,
}

impl PlayerSession {
    /// A player whose items all open through `backend`. A paired server's
    /// item has no route this way ([`Self::for_session`]).
    pub fn new(backend: Arc<dyn PlaybackBackend>) -> Self {
        Self::with(Opener::Backend(backend))
    }

    /// A player whose items open as [`Session::open_any_playback`] opens
    /// them: a paired server's carries its route
    /// ([`ManagedPlayback::route`]), and its progress goes home.
    pub fn for_session(session: Session) -> Self {
        Self::with(Opener::Session(session))
    }

    fn with(opener: Opener) -> Self {
        Self(Arc::new(Inner {
            opener,
            line: tokio::sync::Mutex::new(()),
            asked: AtomicU64::new(0),
            current: Mutex::new(None),
        }))
    }

    /// What plays now.
    pub fn current(&self) -> Option<ManagedPlayback> {
        self.0.current.lock().as_ref().map(|p| p.playback.clone())
    }

    /// Play `item` in place of what plays: that one reports where it was
    /// and stops first.
    pub fn start(
        &self,
        item: MediaId,
        selection: Option<Selection>,
        options: PlaybackOptions,
    ) -> impl Future<Output = Result<ManagedPlayback, SdkError>> + Send + 'static + use<> {
        let mine = self.0.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let this = self.clone();
        async move {
            let _line = this.0.line.lock().await;
            this.end_before(mine).await;
            if this.0.asked.load(Ordering::SeqCst) != mine {
                return Err(SdkError::PlaybackClosed);
            }
            let playback = this.0.opener.open(item, selection, options).await?;
            if this.0.asked.load(Ordering::SeqCst) != mine {
                // Overtaken while it was asked: it never plays.
                let _ = playback.close().await;
                return Err(SdkError::PlaybackClosed);
            }
            *this.0.current.lock() = Some(Playing {
                asked: mine,
                playback: playback.clone(),
            });
            Ok(playback)
        }
    }

    /// Start what plays again ([`ManagedPlayback::restart`]), in line.
    pub async fn restart(&self, cause: RestartCause) -> Result<Playback, SdkError> {
        let _line = self.0.line.lock().await;
        let current = self.current().ok_or(SdkError::PlaybackClosed)?;
        current.restart(cause).await
    }

    /// Stop what plays, after its last report; a start still being asked
    /// is ended as its answer comes.
    pub fn end(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mine = self.0.asked.fetch_add(1, Ordering::SeqCst) + 1;
        let this = self.clone();
        async move {
            let _line = this.0.line.lock().await;
            this.end_before(mine).await;
        }
    }

    /// End what plays, if it was asked before `mine`: polled late, a
    /// request may find a newer one's playback there, and that one stays.
    async fn end_before(&self, mine: u64) {
        let current = self.0.current.lock().take_if(|p| p.asked < mine);
        if let Some(current) = current
            && let Err(error) = current.playback.flush_and_close().await
        {
            tracing::debug!(%error, "ending the playback");
        }
    }
}
