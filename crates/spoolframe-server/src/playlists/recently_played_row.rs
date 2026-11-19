//! Home's "Recently played" row (`RecentlyPlayedRow`, feature
//! `spoolframe:recently-played-row`): a user's plays of every kind as
//! cards, each kind as their preferences say. Here, not in
//! `crate::watch`, because a playlist's card is the playlist as the user
//! sees it.

use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, TryStreamExt, stream};
use spoolframe_protocol::recently_played::RecentCard;
use spoolframe_types::{Item, MediaId, RecentMusicMode, RecentShowsMode};

use super::people::Names;
use super::{CATALOG_FETCHES, Playlists, PlaylistsError};
use crate::principal::Actor;
use crate::store::playlists::PlaylistRow;
use crate::store::plays::{RowBy, RowCard};

/// The most season-less episodes one read of the row looks up the season
/// of: a user's plays from before plays kept it, latest first. Each is
/// looked up once, then kept.
const SEASON_BACKFILL: u32 = 200;

/// A card as found, before its items are filled. A row's worth, short
/// lived: an item unboxed keeps the fill a plain borrow.
#[allow(clippy::large_enum_variant)]
enum Found {
    Playlist(String),
    Item(Item),
}

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
        let principal = actor.principal.to_string();
        let preferences = self
            .store
            .recently_played_preferences(actor.principal)
            .await?;
        let by_source = matches!(
            preferences.music,
            RecentMusicMode::Sources | RecentMusicMode::Unknown
        );
        let seen: HashMap<String, PlaylistRow> = if by_source {
            self.store
                .playlists_seen_by(&principal)
                .await?
                .into_iter()
                .map(|row| (row.id.clone(), row))
                .collect()
        } else {
            HashMap::new()
        };
        if preferences.shows == RecentShowsMode::Season {
            self.backfill_seasons(actor).await?;
        }
        let seen_ids: Vec<String> = seen.keys().cloned().collect();
        let by = RowBy {
            preferences,
            playlists: &seen_ids,
        };
        let cards = self
            .store
            .recently_played_row(actor.principal, by, limit, federated)
            .await?;
        let found: Vec<Option<Found>> = stream::iter(cards)
            .map(|card| self.find(actor, card, &seen))
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        // A season looked up from an older play may be one listed already,
        // at a newer play: it shows there.
        let mut seasons = HashSet::new();
        let mut found: Vec<Found> = found
            .into_iter()
            .flatten()
            .filter(|found| match found {
                Found::Item(Item::Season(season)) => seasons.insert(season.id.clone()),
                _ => true,
            })
            .collect();
        // Every item's state and plays in one read each.
        let mut items: Vec<&mut Item> = found
            .iter_mut()
            .filter_map(|found| match found {
                Found::Item(item) => Some(item),
                Found::Playlist(_) => None,
            })
            .collect();
        self.watch.fill(actor.principal, &mut items).await?;
        self.watch.fill_plays(actor.principal, &mut items).await?;
        // The pins and names a playlist's card needs, only if one shows.
        let (pins, names) = if found.iter().any(|f| matches!(f, Found::Playlist(_))) {
            let (pins, names) = tokio::join!(self.store.pins(&principal), Names::of(&self.store));
            (pins?, names?)
        } else {
            (Vec::new(), Names::default())
        };
        let mut cards = Vec::with_capacity(found.len());
        for found in found {
            cards.push(match found {
                Found::Playlist(id) => {
                    let row = &seen[&id];
                    let pinned = pins.iter().position(|p| *p == id).map(|n| n as u32);
                    let playlist = self.summarized(actor, row, pinned, &names).await?;
                    RecentCard::Playlist { playlist }
                }
                Found::Item(Item::Movie(movie)) => RecentCard::Movie { movie },
                Found::Item(Item::Show(show)) => RecentCard::Show { show },
                Found::Item(Item::Season(season)) => RecentCard::Season { season },
                Found::Item(Item::Episode(episode)) => RecentCard::Episode { episode },
                Found::Item(Item::Album(album)) => RecentCard::Album { album },
                Found::Item(Item::Track(track)) => RecentCard::Track { track },
                Found::Item(Item::Artist(_)) => continue,
            });
        }
        Ok(cards)
    }

    /// Keep the season of `actor`'s plays of episodes that kept none, at
    /// most [`SEASON_BACKFILL`] of them, each looked up once, in one
    /// write: so the row groups them by season, and its limit counts
    /// cards, not plays of one season. An episode not served now stays
    /// as it is, and shows its own season if it is ever found.
    async fn backfill_seasons(&self, actor: &Actor) -> Result<(), PlaylistsError> {
        let episodes = self
            .store
            .episodes_without_season(actor.principal, SEASON_BACKFILL)
            .await?;
        if episodes.is_empty() {
            return Ok(());
        }
        let found: Vec<Option<(String, String)>> = stream::iter(episodes)
            .map(|episode| async move {
                let Ok(id) = episode.parse::<MediaId>() else {
                    return Ok(None);
                };
                match self.catalog.item(&id).await {
                    Ok(found) => Ok(match found.item {
                        Item::Episode(e) => Some((episode, e.season_id.to_string())),
                        _ => None,
                    }),
                    Err(e) if crate::client::unserved(&e) => Ok(None),
                    Err(e) => Err(PlaylistsError::from(e)),
                }
            })
            .buffered(CATALOG_FETCHES)
            .try_collect()
            .await?;
        let seasons: Vec<(String, String)> = found.into_iter().flatten().collect();
        self.store
            .note_play_seasons(actor.principal, &seasons)
            .await?;
        Ok(())
    }

    /// What `card` is as `actor` sees it; `None` when it is not served
    /// them now. A season a play did not keep, and the backfill could not
    /// find, is looked up from its episode.
    async fn find(
        &self,
        actor: &Actor,
        card: RowCard,
        seen: &HashMap<String, PlaylistRow>,
    ) -> Result<Option<Found>, PlaylistsError> {
        let id = match card {
            RowCard::Playlist(id) => {
                return Ok(seen.contains_key(&id).then_some(Found::Playlist(id)));
            }
            RowCard::SeasonOf(episode) => {
                let Some(Item::Episode(e)) = self.served_item(actor, &episode).await? else {
                    return Ok(None);
                };
                e.season_id.to_string()
            }
            RowCard::Movie(id)
            | RowCard::Show(id)
            | RowCard::Season(id)
            | RowCard::Episode(id)
            | RowCard::Album(id)
            | RowCard::Track(id) => id,
        };
        Ok(self.served_item(actor, &id).await?.map(Found::Item))
    }

    /// Item `id` (as stored) as the catalog serves it to `actor` now, if
    /// it does.
    async fn served_item(&self, actor: &Actor, id: &str) -> Result<Option<Item>, PlaylistsError> {
        let Ok(id) = id.parse::<MediaId>() else {
            return Ok(None);
        };
        Ok(self.served(actor, &id).await?.map(|found| found.item))
    }
}
