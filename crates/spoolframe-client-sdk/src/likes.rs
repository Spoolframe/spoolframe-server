//! Hearts, as the Linux app keeps them (app/hearts.rs): what is known of
//! each track's heart, learnt from the replies an app reads anyway (an
//! album's details, a playlist's entries, the entry playing), a tap shown
//! at once and put back on a refusal, and a watch that says when any
//! heart changed so every heart showing that track is drawn again.

use std::collections::HashMap;

use parking_lot::Mutex;
use spoolframe_protocol::playlists::Liked;
use spoolframe_types::MediaId;
use tokio::sync::watch;

use crate::playlists::view::heart_likes;
use crate::{SdkError, Session};

/// The hearts of one session: an app keeps one beside it.
pub struct LikeCache {
    known: Mutex<HashMap<MediaId, bool>>,
    /// One more for each change, for whoever draws hearts.
    changed: watch::Sender<u64>,
}

/// A tap shown before the server answered: [`Pending::rollback`] puts the
/// hearts back when it refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a refused like is put back with rollback"]
pub struct Pending {
    /// Whether the tap likes (or un-likes).
    pub likes: bool,
    before: Vec<(MediaId, bool)>,
}

impl Pending {
    /// The hearts as they were before the tap.
    pub fn rollback(self, cache: &LikeCache) {
        cache.learn(self.before);
    }
}

impl Default for LikeCache {
    fn default() -> Self {
        Self {
            known: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

impl LikeCache {
    /// Whether track `id` is liked, as far as is known.
    pub fn liked(&self, id: &MediaId) -> Option<bool> {
        self.known.lock().get(id).copied()
    }

    /// The heart for `tracks` (one, or an album's): full when every one is
    /// liked, half when some are. A track not heard of counts as not
    /// liked.
    pub fn heart(&self, tracks: &[MediaId]) -> Liked {
        let known = self.known.lock();
        let liked = tracks
            .iter()
            .filter(|t| known.get(*t).copied().unwrap_or(false))
            .count();
        crate::playlists::view::heart_of(liked, tracks.len())
    }

    /// What the server says of these tracks now. Changes are told on
    /// [`Self::changes`].
    pub fn learn(&self, hearts: impl IntoIterator<Item = (MediaId, bool)>) {
        let changed = {
            let mut known = self.known.lock();
            hearts
                .into_iter()
                .filter(|(id, liked)| known.insert(id.clone(), *liked) != Some(*liked))
                .count()
        };
        if changed > 0 {
            self.changed.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// A tap on the heart of `tracks`, shown at once: a full heart
    /// un-likes them, any other likes them. Send it, and on a refusal
    /// [`Pending::rollback`].
    pub fn toggle(&self, tracks: &[MediaId]) -> Pending {
        let likes = heart_likes(self.heart(tracks));
        let before: Vec<(MediaId, bool)> = {
            let known = self.known.lock();
            tracks
                .iter()
                .map(|t| (t.clone(), known.get(t).copied().unwrap_or(false)))
                .collect()
        };
        self.learn(tracks.iter().map(|t| (t.clone(), likes)));
        Pending { likes, before }
    }

    /// Changes as they come: the number moves on each.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

impl Session {
    /// Like `items` (tracks or albums; an album likes every track), or
    /// take them out of Liked.
    pub async fn set_liked(&self, items: &[MediaId], liked: bool) -> Result<(), SdkError> {
        match liked {
            true => self.like(items).await.map(drop),
            false => self.unlike(items).await.map(drop),
        }
    }

    /// A tap on the heart of `tracks`: shown in `cache` at once, then
    /// `sends` (the track, or the album the tracks are) liked or not on
    /// the server; a refusal puts the hearts back and is returned.
    /// Whether it liked.
    pub async fn toggle_like(
        &self,
        cache: &LikeCache,
        sends: &MediaId,
        tracks: &[MediaId],
    ) -> Result<bool, SdkError> {
        let pending = cache.toggle(tracks);
        let likes = pending.likes;
        match self.set_liked(std::slice::from_ref(sends), likes).await {
            Ok(()) => Ok(likes),
            Err(e) => {
                pending.rollback(cache);
                Err(e)
            }
        }
    }

    /// Track `id`'s heart, from `cache`, or asked of the server (its
    /// details) and learnt when not known yet: the now-playing heart of a
    /// track played from elsewhere.
    pub async fn heart_of(&self, cache: &LikeCache, id: &MediaId) -> Result<bool, SdkError> {
        if let Some(liked) = cache.liked(id) {
            return Ok(liked);
        }
        let liked = matches!(self.item(id).await?.liked, Some(Liked::All));
        cache.learn([(id.clone(), liked)]);
        Ok(liked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{MediaKind, Namespace};

    fn track(key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Track, key).unwrap()
    }

    #[test]
    fn a_tap_shows_at_once_and_a_refusal_puts_it_back() {
        let cache = LikeCache::default();
        cache.learn([(track("1"), true)]);
        let album = [track("1"), track("2")];
        assert_eq!(cache.heart(&album), Liked::Some);
        let pending = cache.toggle(&album);
        assert!(pending.likes, "a half heart likes the rest");
        assert_eq!(cache.heart(&album), Liked::All);
        pending.rollback(&cache);
        assert_eq!(cache.heart(&album), Liked::Some);
        assert_eq!(cache.liked(&track("2")), Some(false));
    }

    #[test]
    fn a_full_heart_unlikes() {
        let cache = LikeCache::default();
        cache.learn([(track("1"), true)]);
        assert!(!cache.toggle(&[track("1")]).likes);
        assert_eq!(cache.liked(&track("1")), Some(false));
    }

    #[test]
    fn only_a_change_is_told() {
        let cache = LikeCache::default();
        let mut changes = cache.changes();
        cache.learn([(track("1"), true)]);
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        cache.learn([(track("1"), true)]);
        assert!(!changes.has_changed().unwrap());
        assert_eq!(cache.liked(&track("9")), None, "never heard of");
    }
}
