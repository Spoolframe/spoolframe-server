//! Playlists in the web UI (PX-40): the `me.playlist*` methods.
//!
//! The web UI talks to the admin API, not the client protocol, so these
//! are the same [`Playlists`] service the client protocol and the MCP
//! tools call, with the session user as the [`Actor`]. Who may do what is
//! decided there, never here: an admin's rights over server-wide
//! playlists come with `Actor::admin`, read from the user's role at each
//! call, so there is no `admin.*` twin of any of them.

use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use spoolframe_admin_api::{
    NewPlaylist, PlaylistChanges, PlaylistDetail, PlaylistEditOp, PlaylistEntryInfo, PlaylistInfo,
    PlaylistKinds, PlaylistOwner, PlaylistShare, PlaylistSharing, PlexPlaylistImport,
    PlexPlaylistMade, PlexPlaylistUnmapped, ShareableUser, error_code,
};
use spoolframe_protocol::playlists::{
    Kinds, PlaylistEdit, PlaylistOwnerKind, PlaylistShare as WireShare, PlaylistSummary, Visibility,
};
use spoolframe_types::MediaId;

use super::{AdminApi, error, internal};
use crate::playlists::plex_import::{self, ImportError};
use crate::playlists::{PlaylistUpdate, Playlists, PlaylistsError};
use crate::principal::{Actor, Principal};
use crate::store::sessions::ApiSessionUser;

impl AdminApi {
    /// The playlists service, or why there is none.
    pub(super) fn playlists_service(&self) -> RpcResult<&Playlists> {
        self.library
            .as_ref()
            .map(|l| l.playlists.as_ref())
            .ok_or_else(|| {
                error(
                    error_code::NOT_CONFIGURED,
                    "this server has no library for playlists",
                )
            })
    }

    pub(super) async fn my_playlists(&self, user: ApiSessionUser) -> RpcResult<Vec<PlaylistInfo>> {
        let (mut list, _notes) = self
            .playlists_service()?
            .list(&actor(&user))
            .await
            .map_err(playlists_error)?;
        // Pin notes are the apps' to show (Pinned is theirs): the web UI
        // leaves them, so dismissing them here never hides one from an app.
        list.sort_by_cached_key(|p| p.name.to_lowercase());
        Ok(list.iter().map(info).collect())
    }

    pub(super) async fn my_playlist(
        &self,
        user: ApiSessionUser,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> RpcResult<PlaylistDetail> {
        let page = self
            .playlists_service()?
            .get(&actor(&user), id, offset, limit)
            .await
            .map_err(playlists_error)?;
        Ok(PlaylistDetail {
            playlist: info(&page.playlist),
            entries: page
                .entries
                .into_iter()
                .map(|e| PlaylistEntryInfo {
                    entry: e.entry,
                    item: e.item,
                    unavailable: e.unavailable,
                })
                .collect(),
            offset: page.offset,
            total: page.total,
            playable: page.playable,
        })
    }

    pub(super) async fn create_playlist(
        &self,
        user: ApiSessionUser,
        playlist: NewPlaylist,
    ) -> RpcResult<PlaylistInfo> {
        let made = self
            .playlists_service()?
            .create(
                &actor(&user),
                &playlist.name,
                kinds(playlist.kinds),
                visibility(playlist.sharing),
            )
            .await
            .map_err(playlists_error)?;
        Ok(info(&made))
    }

    pub(super) async fn update_playlist(
        &self,
        user: ApiSessionUser,
        id: &str,
        changes: PlaylistChanges,
    ) -> RpcResult<PlaylistInfo> {
        let update = PlaylistUpdate {
            name: changes.name,
            kinds: changes.kinds.map(kinds),
            visibility: changes.sharing.map(visibility),
        };
        let updated = self
            .playlists_service()?
            .update(&actor(&user), id, update)
            .await
            .map_err(playlists_error)?;
        Ok(info(&updated))
    }

    pub(super) async fn edit_playlist(
        &self,
        user: ApiSessionUser,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEditOp>,
    ) -> RpcResult<u64> {
        let edits = edits
            .into_iter()
            .map(edit)
            .collect::<RpcResult<Vec<PlaylistEdit>>>()?;
        self.playlists_service()?
            .edit(&actor(&user), id, revision, edits)
            .await
            .map_err(playlists_error)
    }

    pub(super) async fn delete_playlist(&self, user: ApiSessionUser, id: &str) -> RpcResult<()> {
        self.playlists_service()?
            .delete(&actor(&user), id)
            .await
            .map_err(playlists_error)
    }

    /// Every enabled local user but `user`, and every portable user with a
    /// live grant, by the name they are shown as.
    pub(super) async fn share_candidates(
        &self,
        user: ApiSessionUser,
    ) -> RpcResult<Vec<ShareableUser>> {
        let store = self.auth.store();
        let mut candidates: Vec<ShareableUser> = store
            .list_users()
            .await?
            .into_iter()
            .filter(|u| u.id != user.user && u.disabled_at.is_none())
            .map(|u| ShareableUser {
                user_id: Principal::LocalUser(u.id).to_string(),
                display_name: u.display_name.unwrap_or(u.username),
            })
            .collect();
        candidates.extend(
            store
                .grants()
                .await
                .map_err(internal)?
                .into_iter()
                .map(|g| ShareableUser {
                    user_id: Principal::PortableUser(g.portable).to_string(),
                    display_name: g.shown_name(),
                }),
        );
        candidates.sort_by_cached_key(|c| c.display_name.to_lowercase());
        Ok(candidates)
    }

    pub(super) async fn import_plex_playlists(
        &self,
        user: ApiSessionUser,
        dry_run: bool,
    ) -> RpcResult<PlexPlaylistImport> {
        let playlists = self.playlists_service()?;
        let Some(sync) = &self.plex_sync else {
            return Err(error(
                error_code::NOT_CONFIGURED,
                "this server does not link Plex accounts",
            ));
        };
        let done = plex_import::import(playlists, sync, &actor(&user), user.user, dry_run)
            .await
            .map_err(|e| match e {
                ImportError::NotLinked => {
                    error(error_code::NOT_CONFIGURED, "link your Plex account first")
                }
                ImportError::Playlists(e) => playlists_error(e),
                ImportError::Sync(e) => {
                    tracing::warn!(error = %e, "a Plex playlist import failed");
                    error(error_code::UNAVAILABLE, "Plex could not be read")
                }
            })?;
        Ok(PlexPlaylistImport {
            made: done
                .made
                .into_iter()
                .map(|m| PlexPlaylistMade {
                    name: m.name,
                    playlist_id: m.playlist_id,
                    entries: m.entries,
                })
                .collect(),
            unmapped: done
                .unmapped
                .into_iter()
                .map(|u| PlexPlaylistUnmapped {
                    playlist: u.playlist,
                    title: u.title,
                    reason: u.reason,
                })
                .collect(),
            notes: done.notes,
        })
    }
}

/// The session's user, with their rights now.
fn actor(user: &ApiSessionUser) -> Actor {
    Actor {
        principal: Principal::LocalUser(user.user),
        admin: user.role.is_admin(),
    }
}

fn kinds(k: PlaylistKinds) -> Kinds {
    Kinds {
        movies: k.movies,
        episodes: k.episodes,
        tracks: k.tracks,
    }
}

fn visibility(sharing: PlaylistSharing) -> Visibility {
    match sharing {
        PlaylistSharing::Personal => Visibility::Personal,
        PlaylistSharing::ServerWide => Visibility::ServerWide,
        PlaylistSharing::Shared { with } => Visibility::Shared {
            with: with
                .into_iter()
                .map(|s| WireShare {
                    user_id: s.user_id,
                    can_edit: s.can_edit,
                })
                .collect(),
        },
    }
}

fn edit(op: PlaylistEditOp) -> RpcResult<PlaylistEdit> {
    Ok(match op {
        PlaylistEditOp::Add { id, before } => PlaylistEdit::Add {
            id: id.parse::<MediaId>().map_err(|_| {
                error(
                    error_code::INVALID_INPUT,
                    format!("{id:?} is not an item id"),
                )
            })?,
            before,
        },
        PlaylistEditOp::Remove { entry } => PlaylistEdit::Remove { entry },
        PlaylistEditOp::Move { entry, before } => PlaylistEdit::Move { entry, before },
    })
}

/// A playlist as the admin API gives it.
fn info(p: &PlaylistSummary) -> PlaylistInfo {
    PlaylistInfo {
        id: p.id.clone(),
        name: p.name.clone(),
        kinds: PlaylistKinds {
            movies: p.kinds.movies,
            episodes: p.kinds.episodes,
            tracks: p.kinds.tracks,
        },
        owner: match p.owner_kind {
            PlaylistOwnerKind::Mine => PlaylistOwner::Mine,
            PlaylistOwnerKind::ServerWide => PlaylistOwner::ServerWide,
            PlaylistOwnerKind::SharedWithMe | PlaylistOwnerKind::Unknown => {
                PlaylistOwner::SharedWithMe
            }
        },
        sharing: match &p.visibility {
            Visibility::ServerWide => PlaylistSharing::ServerWide,
            Visibility::Shared { with } => PlaylistSharing::Shared {
                with: with
                    .iter()
                    .map(|s| PlaylistShare {
                        user_id: s.user_id.clone(),
                        can_edit: s.can_edit,
                    })
                    .collect(),
            },
            Visibility::Personal | Visibility::Unknown => PlaylistSharing::Personal,
        },
        can_edit: p.can_edit,
        can_manage: p.can_manage,
        revision: p.revision,
    }
}

/// The service's refusal, as the admin API says it.
fn playlists_error(e: PlaylistsError) -> ErrorObjectOwned {
    match e {
        PlaylistsError::NotFound => error(error_code::NO_SUCH_PLAYLIST, "no such playlist"),
        PlaylistsError::NoItem(_) | PlaylistsError::InvalidInput(_) => {
            error(error_code::INVALID_INPUT, e.to_string())
        }
        PlaylistsError::ReadOnly => error(error_code::PLAYLIST_READ_ONLY, e.to_string()),
        PlaylistsError::AdminRequired => error(error_code::FORBIDDEN, e.to_string()),
        PlaylistsError::KindNotAllowed(kind) => error(
            error_code::KIND_NOT_ALLOWED,
            format!("this playlist does not take {}", kind_name(kind)),
        ),
        PlaylistsError::Conflict { revision } => ErrorObjectOwned::owned(
            error_code::PLAYLIST_CONFLICT,
            e.to_string(),
            Some(serde_json::json!({ "revision": revision })),
        ),
        PlaylistsError::Catalog(e) => super::catalog::catalog_error(e),
        PlaylistsError::Store(e) => internal(e),
    }
}

fn kind_name(kind: spoolframe_types::MediaKind) -> &'static str {
    use spoolframe_types::MediaKind as K;
    match kind {
        K::Movie => "movies",
        K::Show | K::Season | K::Episode => "episodes",
        K::Artist | K::Album | K::Track => "music",
    }
}
