//! What a device's music downloads follow, against its cursor (PX-51):
//! `MusicDownloadChanges`.
//!
//! Nothing on the server records a device's downloads (the user's
//! decision, 2026-10-14), and neither the catalog nor the playlists keep
//! a log of what left them: a playlist's revision names the entries an
//! edit removed, not their items, and an artist's albums are only what
//! the library has now. So each followed thing is answered with every
//! track it holds now, and the device compares that with what it keeps.
//! The cursor carries a hash of each answer: a thing that holds the same
//! tracks as at the cursor is `unchanged`, and sends nothing.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use serde::{Deserialize, Serialize};

use spoolframe_protocol::downloads::{self as wire, FollowedChange, FollowedTracks};
use spoolframe_types::MediaId;

use crate::playlists::{CATALOG_FETCHES, Followed, Playlists, PlaylistsError};
use crate::principal::Actor;

/// What a cursor holds: each followed thing's hash, by its key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    v: u32,
    sets: BTreeMap<String, String>,
}

impl Cursor {
    const VERSION: u32 = 1;

    /// The cursor `text` is; an empty one for any this build cannot read.
    fn read(text: Option<&str>) -> Self {
        text.and_then(|t| URL_SAFE_NO_PAD.decode(t).ok())
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|c| c.v == Self::VERSION)
            .unwrap_or_default()
    }

    fn text(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("a cursor is JSON"))
    }
}

/// `followed`'s key in a cursor.
fn key(followed: &Followed) -> String {
    match followed {
        Followed::Artist(id) => format!("artist:{id}"),
        Followed::Playlist(id) => format!("playlist:{id}"),
        Followed::Liked => "liked".to_owned(),
    }
}

/// A short hash of `tracks`, in order.
fn hash(tracks: &[MediaId]) -> String {
    let parts: Vec<String> = tracks.iter().map(ToString::to_string).collect();
    let mut hex = crate::files::hash_parts(&parts).to_hex().to_string();
    hex.truncate(16);
    hex
}

/// What each of `followed` holds for `actor` against cursor `since`, in
/// order, and the cursor to ask with next: `since`'s hashes, those asked
/// about now in their place (one gone, dropped). So a cursor from an ask
/// about some things still answers for the others (review 2026-10-20,
/// finding 9).
pub async fn changes(
    playlists: &Playlists,
    actor: &Actor,
    followed: &[Followed],
    since: Option<&str>,
) -> Result<(Vec<FollowedChange>, String), PlaylistsError> {
    let before = Cursor::read(since);
    let mut after = Cursor {
        v: Cursor::VERSION,
        sets: before.sets.clone(),
    };
    // Looked up a few at a time, in the order asked.
    let found: Vec<_> = stream::iter(followed.to_vec())
        .map(|thing| async move { playlists.followed_tracks(actor, &thing).await })
        .buffered(CATALOG_FETCHES)
        .try_collect()
        .await?;
    let mut changes = Vec::with_capacity(followed.len());
    for (thing, found) in followed.iter().zip(found) {
        let key = key(thing);
        let tracks = match found {
            None => {
                after.sets.remove(&key);
                FollowedTracks::Gone
            }
            Some(tracks) => {
                let now = hash(&tracks);
                let same = before.sets.get(&key) == Some(&now);
                after.sets.insert(key, now);
                if same {
                    FollowedTracks::Unchanged
                } else {
                    FollowedTracks::Tracks { tracks }
                }
            }
        };
        changes.push(FollowedChange {
            followed: to_wire(thing),
            tracks,
        });
    }
    Ok((changes, after.text()))
}

fn to_wire(followed: &Followed) -> wire::Followed {
    match followed {
        Followed::Artist(id) => wire::Followed::Artist { id: id.clone() },
        Followed::Playlist(id) => wire::Followed::Playlist { id: id.clone() },
        Followed::Liked => wire::Followed::Liked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_reads_back_as_it_was_written() {
        let mut cursor = Cursor {
            v: Cursor::VERSION,
            sets: BTreeMap::new(),
        };
        cursor
            .sets
            .insert("liked".into(), "0123456789abcdef".into());
        assert_eq!(Cursor::read(Some(&cursor.text())).sets, cursor.sets);
    }

    #[test]
    fn a_cursor_this_build_cannot_read_is_no_cursor() {
        assert!(Cursor::read(Some("not a cursor")).sets.is_empty());
    }

    #[test]
    fn the_same_tracks_in_another_order_hash_otherwise() {
        let a = MediaId::new(
            spoolframe_types::Namespace::Plex,
            spoolframe_types::MediaKind::Track,
            "1",
        )
        .unwrap();
        let b = MediaId::new(
            spoolframe_types::Namespace::Plex,
            spoolframe_types::MediaKind::Track,
            "2",
        )
        .unwrap();
        assert_ne!(hash(&[a.clone(), b.clone()]), hash(&[b, a]));
    }
}
