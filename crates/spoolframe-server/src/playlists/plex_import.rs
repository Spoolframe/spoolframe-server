//! Importing a user's Plex playlists, once (PX-40): each video or music
//! playlist of their linked account, on every Plex server here, becomes a
//! personal playlist of theirs, its items mapped to this server's. What
//! does not map is reported. Nothing is synced afterwards: the playlist
//! is ours only. A smart playlist is imported as what it holds now.
//!
//! A playlist given a picture of its own in Plex brings it as its cover
//! (PX-53), fetched through the PMS's photo transcoder and taken as any
//! upload is; one with only Plex's composite of what it holds gets this
//! server's mosaic instead.
//!
//! The tracks the user rated in Plex, with any number of stars, are
//! liked here (PX-43): Plex's rating is what its apps' heart sets, so
//! they go into Liked, the last rated at the top, not into a playlist of
//! their own. Imported twice, they are liked once.

use spoolframe_plex::playlists::PlexPlaylistItem;
use spoolframe_protocol::playlists::{PlexImport, PlexImported, PlexUnmapped};
use spoolframe_types::{Item, MediaId};

use super::{Playlists, PlaylistsError};
use crate::plex_sync::{SyncError, WatchSync};
use crate::principal::Actor;
use crate::store::users::LocalUserId;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The user has no Plex account linked, or the server links none.
    #[error("no Plex account is linked")]
    NotLinked,
    #[error(transparent)]
    Sync(SyncError),
    #[error(transparent)]
    Playlists(#[from] PlaylistsError),
}

impl From<SyncError> for ImportError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::NotLinked => ImportError::NotLinked,
            other => ImportError::Sync(other),
        }
    }
}

/// What the report calls Liked: its rated tracks' "playlist".
const LIKED: &str = "Liked";

/// What a Plex item is called, for the report.
fn title(item: &Item) -> String {
    match item {
        Item::Movie(m) => m.title.clone(),
        Item::Show(s) => s.title.clone(),
        Item::Season(s) => s.title.clone(),
        Item::Episode(e) => e.title.clone(),
        Item::Artist(a) => a.name.clone(),
        Item::Album(a) => a.title.clone(),
        Item::Track(t) => t.title.clone(),
    }
}

/// Import `user`'s Plex playlists as `actor` (the same person), or with
/// `dry_run` say what would be made.
pub async fn import(
    playlists: &Playlists,
    sync: &WatchSync,
    actor: &Actor,
    user: LocalUserId,
    dry_run: bool,
) -> Result<PlexImport, ImportError> {
    let (views, mut notes) = sync.user_views(user).await?;
    let (mut made, mut unmapped) = (Vec::new(), Vec::new());
    let mut rated: Vec<(MediaId, String)> = Vec::new();
    for (server, plex) in views {
        match plex.rated_tracks().await {
            Ok(tracks) => rated.extend(tracks.iter().map(|track| {
                let id = crate::catalog::ids::media(&server, track.id().clone());
                (id, title(track))
            })),
            Err(e) => {
                tracing::warn!(server, error = %e, "cannot read a Plex account's rated tracks");
                notes.push(format!(
                    "the rated tracks on Plex server {server} could not be read"
                ));
            }
        }
        let lists = match plex.playlists().await {
            Ok(lists) => lists,
            Err(e) => {
                tracing::warn!(server, error = %e, "cannot read a Plex account's playlists");
                notes.push(format!(
                    "the playlists on Plex server {server} could not be read"
                ));
                continue;
            }
        };
        for list in lists {
            let entries = match plex.playlist_items(&list.key).await {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(server, error = %e, "cannot read a Plex playlist");
                    unmapped.push(PlexUnmapped {
                        playlist: list.title.clone(),
                        title: None,
                        reason: "Plex did not give its items".into(),
                    });
                    continue;
                }
            };
            let mut items: Vec<(MediaId, String)> = Vec::new();
            for entry in entries {
                match entry {
                    PlexPlaylistItem::Item(item) => {
                        let id = crate::catalog::ids::media(&server, item.id().clone());
                        items.push((id, title(&item)));
                    }
                    PlexPlaylistItem::Unmapped { title, why } => unmapped.push(PlexUnmapped {
                        playlist: list.title.clone(),
                        title: Some(title),
                        reason: why,
                    }),
                }
            }
            let (playlist_id, count, left_out) = playlists
                .import(actor, &list.title, &items, dry_run)
                .await?;
            unmapped.extend(left_out.into_iter().map(|(title, reason)| PlexUnmapped {
                playlist: list.title.clone(),
                title: Some(title),
                reason,
            }));
            if count == 0 {
                unmapped.push(PlexUnmapped {
                    playlist: list.title.clone(),
                    title: None,
                    reason: "nothing in it is in this server's libraries".into(),
                });
                continue;
            }
            if let (Some(id), Some(art)) = (&playlist_id, &list.art) {
                let cover = async {
                    let image = plex.playlist_art(art).await.map_err(|e| e.to_string())?;
                    playlists
                        .import_art(actor, id, &image.bytes)
                        .await
                        .map_err(|e| e.to_string())
                };
                if let Err(e) = cover.await {
                    tracing::warn!(server, error = %e, "cannot import a Plex playlist's picture");
                    notes.push(format!(
                        "the picture of the Plex playlist {} could not be imported",
                        list.title
                    ));
                }
            }
            made.push(PlexImported {
                name: list.title.clone(),
                playlist_id,
                entries: count,
            });
        }
    }
    if !rated.is_empty() {
        let (playlist_id, count, left_out) = playlists.import_liked(actor, &rated, dry_run).await?;
        unmapped.extend(left_out.into_iter().map(|(title, reason)| PlexUnmapped {
            playlist: LIKED.into(),
            title: Some(title),
            reason,
        }));
        if count > 0 {
            made.push(PlexImported {
                name: LIKED.into(),
                playlist_id,
                entries: count,
            });
        }
    }
    Ok(PlexImport {
        made,
        unmapped,
        notes,
    })
}
