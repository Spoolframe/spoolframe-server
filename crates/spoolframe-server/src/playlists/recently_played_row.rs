//! Home's "Recently played" row (`RecentlyPlayedRow`, feature
//! `spoolframe:recently-played-row`): a user's plays of every kind as
//! cards, each kind as their preferences say. Here, not in
//! `crate::watch`, because a playlist's card is the playlist as the user
//! sees it.

use std::collections::HashSet;

use futures_util::{StreamExt, TryStreamExt, stream};
use spoolframe_protocol::recently_played::RecentCard;
use spoolframe_types::{Item, MediaId, RecentMusicMode};

use super::people::Names;
use super::{CATALOG_FETCHES, Playlists, PlaylistsError};
use crate::principal::Actor;
use crate::store::playlists::PlaylistRow;
use crate::store::plays::{RowBy, RowCard};

impl Playlists {
    /// `actor`'s "Recently played" row: at most `limit` cards, newest
    /// first, each once at its latest counted play, each kind as their
    /// preferences say; a kind they hid is left out, and with every kind
    /// hidden there is nothing. A playlist they no longer see shows the
    /// play's album. A card the catalog does not serve them now is left
    /// out, so there may be fewer.
    pub async fn recently_played_row(
        &self,
        actor: &Actor,
        limit: u32,
        federated: bool,
    ) -> Result<Vec<RecentCard>, PlaylistsError> {
        let preferences = self
            .store
            .recently_played_preferences(actor.principal)
            .await?;
        let by_source = matches!(
            preferences.music,
            RecentMusicMode::Sources | RecentMusicMode::Unknown
        );
        let seen = if by_source {
            self.store
                .playlists_seen_by(&actor.principal.to_string())
                .await?
        } else {
            Vec::new()
        };
        let seen_ids: Vec<String> = seen.iter().map(|row| row.id.clone()).collect();
        let by = RowBy {
            preferences,
            playlists: &seen_ids,
        };
        let cards = self
            .store
            .recently_played_row(actor.principal, by, limit, federated)
            .await?;
        let pins = self.store.pins(&actor.principal.to_string()).await?;
        let names = Names::of(&self.store).await?;
        let made: Vec<Option<RecentCard>> = stream::iter(cards)
            .map(|card| self.card(actor, card, &seen, &pins, &names))
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        // A season looked up from an older play may be one listed already,
        // at a newer play: it shows there.
        let mut seasons = HashSet::new();
        Ok(made
            .into_iter()
            .flatten()
            .filter(|card| match card {
                RecentCard::Season { season } => seasons.insert(season.id.clone()),
                _ => true,
            })
            .collect())
    }

    /// `card` as `actor` sees it; `None` when it is not served them now.
    async fn card(
        &self,
        actor: &Actor,
        card: RowCard,
        seen: &[PlaylistRow],
        pins: &[String],
        names: &Names,
    ) -> Result<Option<RecentCard>, PlaylistsError> {
        if let RowCard::Playlist(id) = &card {
            let Some(row) = seen.iter().find(|row| row.id == *id) else {
                return Ok(None);
            };
            let pinned = pins.iter().position(|p| p == id).map(|n| n as u32);
            let playlist = self.summarized(actor, row, pinned, names).await?;
            return Ok(Some(RecentCard::Playlist { playlist }));
        }
        let id = match card {
            RowCard::SeasonOf(episode) => match self.served(actor, &episode).await? {
                Some(Item::Episode(e)) => e.season_id.to_string(),
                _ => return Ok(None),
            },
            RowCard::Movie(id)
            | RowCard::Show(id)
            | RowCard::Season(id)
            | RowCard::Episode(id)
            | RowCard::Album(id)
            | RowCard::Track(id)
            | RowCard::Playlist(id) => id,
        };
        let Some(mut item) = self.served(actor, &id).await? else {
            return Ok(None);
        };
        self.watch.fill(actor.principal, &mut [&mut item]).await?;
        self.watch
            .fill_plays(actor.principal, &mut [&mut item])
            .await?;
        Ok(match item {
            Item::Movie(movie) => Some(RecentCard::Movie { movie }),
            Item::Show(show) => Some(RecentCard::Show { show }),
            Item::Season(season) => Some(RecentCard::Season { season }),
            Item::Episode(episode) => Some(RecentCard::Episode { episode }),
            Item::Album(album) => Some(RecentCard::Album { album }),
            Item::Track(track) => Some(RecentCard::Track { track }),
            Item::Artist(_) => None,
        })
    }

    /// Item `id` as the catalog serves it to `actor` now, if it does.
    async fn served(&self, actor: &Actor, id: &str) -> Result<Option<Item>, PlaylistsError> {
        let Ok(id) = id.parse::<MediaId>() else {
            return Ok(None);
        };
        let found = match self.catalog.item(&id).await {
            Ok(found) => found,
            Err(e) if crate::client::unserved(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(self.may_see(actor, &found.item).then_some(found.item))
    }
}
