//! A Plex account's playlists (PX-40), read once for a user's import:
//! what each holds, as items of this server's catalog. Nothing is kept in
//! sync afterwards.

use spoolframe_catalog::{Catalog, CatalogError};
use spoolframe_types::{Item, LibraryKind};

use crate::{Call, PlexCatalog, convert};

/// One of the account's playlists on this PMS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexPlaylist {
    /// Its ratingKey.
    pub key: String,
    pub title: String,
    /// Made by rules: what it holds is Plex's to decide, now only.
    pub smart: bool,
}

/// One entry of a Plex playlist.
#[derive(Debug, Clone, PartialEq)]
pub enum PlexPlaylistItem {
    /// An item this catalog reads, under its id here.
    Item(Box<Item>),
    /// One it does not (a photo, a clip, a kind Spoolframe does not
    /// model, or one it cannot read): its title, and why.
    Unmapped { title: String, why: String },
}

impl PlexCatalog {
    /// The video and music playlists of the account this catalog's token
    /// is, in Plex's order. Photo playlists are left out.
    pub async fn playlists(&self) -> Result<Vec<PlexPlaylist>, CatalogError> {
        let call = Call::get("/playlists");
        let container = self.fetch(&call).await?;
        Ok(container
            .metadata
            .iter()
            .filter(|m| matches!(m.playlist_type.as_deref(), Some("video" | "audio")))
            .map(|m| PlexPlaylist {
                key: m.rating_key.clone(),
                title: m.title.clone(),
                smart: m.smart.unwrap_or(false),
            })
            .collect())
    }

    /// The tracks the account this catalog's token is rated, in every
    /// music section, the last rated first: what a Plex import likes
    /// here (PX-43). Plex's `userRating>>=0` asks for those rated above
    /// nothing; the rating is checked again here.
    pub async fn rated_tracks(&self) -> Result<Vec<Item>, CatalogError> {
        let mut rated = Vec::new();
        for library in self.libraries().await? {
            if library.kind != LibraryKind::Music {
                continue;
            }
            let section = crate::plex_key(library.id.namespace(), library.id.key(), &library.id)?;
            let listed = self.listed(section, "10", &[("userRating>>", "0")]).await?;
            for m in listed {
                if m.user_rating.unwrap_or(0.0) <= 0.0 {
                    continue;
                }
                if let Ok(item @ Item::Track(_)) = convert::item(&m) {
                    rated.push((m.last_rated_at, item));
                }
            }
        }
        // Stable: equal or unknown times keep Plex's order.
        rated.sort_by_key(|(at, _)| std::cmp::Reverse(at.unwrap_or(i64::MIN)));
        Ok(rated.into_iter().map(|(_, item)| item).collect())
    }

    /// What playlist `key` holds, in its order.
    pub async fn playlist_items(&self, key: &str) -> Result<Vec<PlexPlaylistItem>, CatalogError> {
        let key = convert::local_key(key).map_err(|e| CatalogError::NotFound(e.to_string()))?;
        let call = Call::get(format!("/playlists/{key}/items"));
        let container = self.fetch(&call).await?;
        Ok(container
            .metadata
            .iter()
            .map(|m| match convert::media_kind(&m.kind) {
                Some(_) => match convert::item(m) {
                    Ok(item) => PlexPlaylistItem::Item(Box::new(item)),
                    Err(e) => PlexPlaylistItem::Unmapped {
                        title: m.title.clone(),
                        why: format!("{e:?}"),
                    },
                },
                None => PlexPlaylistItem::Unmapped {
                    title: m.title.clone(),
                    why: format!("a {} is not something Spoolframe plays", m.kind),
                },
            })
            .collect())
    }
}
