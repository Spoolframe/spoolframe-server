//! What a playback tells the server as it plays, from what the app's
//! player says it does ([`ManagedPlayback::set_state`]).
//!
//! The SDK reports where the player is every 10 s while it plays, at
//! once when it moves backwards, and when it pauses. The server ends a
//! playback it has not heard from for a while; until it keeps paused
//! playbacks itself, a paused one reports again every 60 s, and a
//! converted one that reads nothing (its buffer is full) reads a byte.
//!
//! The timer runs on the playback's own runtime: nothing the app gives
//! needs to be `Send`. The app pushes the state from its own ticks.

use super::{Inner, ManagedPlayback};
use crate::SdkError;
use spoolframe_protocol::PlaybackPlan;
use spoolframe_types::Delivery;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// What the app's player does now, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Playing { pos: Duration },
    Paused { pos: Duration },
}

impl PlayerState {
    pub fn position(self) -> Duration {
        match self {
            PlayerState::Playing { pos } | PlayerState::Paused { pos } => pos,
        }
    }
}

/// What a tick sends.
enum Tick {
    Report(Duration),
    ReadByte,
}

impl ManagedPlayback {
    /// How often a playing playback reports where it is: what play
    /// history asks for, one report crediting at most 30 s of play.
    pub const PROGRESS_EVERY: Duration = Duration::from_secs(10);
    /// How often an idle playback is used, so the server does not end it.
    pub const KEEP_ALIVE: Duration = Duration::from_secs(60);
    /// How long it plays on end before its restart budget is whole again.
    pub const REFILL_AFTER: Duration = Duration::from_secs(120);
    /// A move back of more than this is a seek back, reported at once;
    /// less is a player's jitter.
    const BACKWARDS: Duration = Duration::from_secs(1);

    /// What the player does now: call it from the player's own ticks,
    /// and on every play, pause and seek. The SDK reports from it, at a
    /// seek's target while the seek is in flight
    /// ([`Self::seek_started`]).
    pub fn set_state(&self, player: PlayerState) -> Result<(), SdkError> {
        let mut state = self.0.state.lock();
        let (at, _) = state.gate.position(player.position());
        let player = match player {
            PlayerState::Playing { .. } => PlayerState::Playing { pos: at },
            PlayerState::Paused { .. } => PlayerState::Paused { pos: at },
        };
        if state.finishing || state.closed {
            return Err(SdkError::PlaybackClosed);
        }
        if let Some(ended) = &state.ended {
            return Err(ended.clone());
        }
        let before = state.player.replace(player);
        let pos = player.position();
        state.position = Some(pos);
        let report = match (before, player) {
            (Some(PlayerState::Paused { .. }), PlayerState::Paused { .. }) => false,
            (_, PlayerState::Paused { .. }) => true,
            (Some(before), PlayerState::Playing { pos }) => {
                pos + Self::BACKWARDS < before.position()
            }
            (None, PlayerState::Playing { .. }) => false,
        };
        match player {
            PlayerState::Playing { .. } => {
                state.playing_since.get_or_insert_with(Instant::now);
            }
            PlayerState::Paused { .. } => state.playing_since = None,
        }
        if report {
            state.pending = Some(pos);
            self.0.notify.notify_one();
        }
        Ok(())
    }

    /// Whether an idle playback must be used to stay alive: true until
    /// the server says it keeps paused playbacks.
    #[cfg(test)]
    pub(crate) fn set_keepalive_needed(&self, needed: bool) {
        self.0.state.lock().keepalive_needed = needed;
    }

    /// Report where the player is, then stop: after every report queued
    /// before it. As [`Self::finish`] at the last position known.
    pub async fn flush_and_close(&self) -> Result<(), SdkError> {
        let at = self.0.state.lock().position;
        self.close_at(at).await
    }

    /// What this tick sends, if anything.
    fn tick(&self) -> Option<Tick> {
        let mut state = self.0.state.lock();
        if state.finishing || state.closed || state.ended.is_some() {
            return None;
        }
        let now = Instant::now();
        if let Some(since) = state.playing_since
            && now.duration_since(since) >= Self::REFILL_AFTER
        {
            state.resumes.clear();
            state.playing_since = Some(now);
        }
        let keep_alive = state.keepalive_needed;
        match state.player? {
            PlayerState::Playing { pos } => {
                let progressive = matches!(
                    state.playback.plan,
                    PlaybackPlan::Converted {
                        delivery: Delivery::Progressive,
                        ..
                    }
                );
                // A full buffer reads nothing; the encode must not idle out.
                if keep_alive
                    && progressive
                    && now.duration_since(state.read_at) >= Self::KEEP_ALIVE
                {
                    state.pending = Some(pos);
                    return Some(Tick::ReadByte);
                }
                Some(Tick::Report(pos))
            }
            PlayerState::Paused { pos } => {
                if !keep_alive || now.duration_since(state.contact) < Self::KEEP_ALIVE {
                    return None;
                }
                // A paired server's item reports home, not to the
                // server that plays it: that one is kept by a read.
                if self.0.backend.route().is_some() {
                    state.pending = Some(pos);
                    return Some(Tick::ReadByte);
                }
                Some(Tick::Report(pos))
            }
        }
    }

    async fn on_tick(&self) {
        let Some(tick) = self.tick() else { return };
        if let Tick::ReadByte = tick {
            let playback = self.snapshot();
            let read = match playback.plan {
                PlaybackPlan::Converted {
                    delivery: Delivery::Progressive,
                    ..
                } => self.read_chunk(&playback, 0, 1).await.map(drop),
                PlaybackPlan::Converted { .. } => self
                    .manifest(spoolframe_protocol::ManifestKind::Media, 0)
                    .await
                    .map(drop),
                _ => self.read(0, 1).await.map(drop),
            };
            if let Err(error) = read {
                tracing::debug!(%error, "keeping the playback alive");
            }
        }
        if let Tick::Report(pos) = tick {
            self.0.state.lock().pending = Some(pos);
        }
        if let Err(error) = self.flush_inner().await {
            let _ = self.0.errors.send(error);
        }
    }
}

/// Tick for the playback, on its own runtime, until it is dropped.
pub(super) fn follow(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    inner.runtime.spawn(async move {
        let mut ticks = tokio::time::interval(ManagedPlayback::PROGRESS_EVERY);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            ticks.tick().await;
            let Some(inner) = weak.upgrade() else { return };
            ManagedPlayback(inner).on_tick().await;
        }
    });
}
