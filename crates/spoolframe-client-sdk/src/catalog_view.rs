//! Catalog reads an app makes the same way everywhere: a library read a
//! page at a time ([`LibraryPager`]), a show with the season it opens on
//! ([`Session::show`]), and the tracks an item stands for
//! ([`Session::tracks_of`]).

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use spoolframe_types::{Item, ItemFilter, ItemSort, LibraryId, MediaId, MediaKind, Season};

use crate::{ItemDetail, ItemsPage, SdkError, Session};

/// Where a [`LibraryPager`] reads its pages from.
#[async_trait]
pub trait ItemsSource: Send + Sync {
    async fn items_page(
        &self,
        library: &LibraryId,
        offset: u32,
        limit: u32,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> Result<ItemsPage, SdkError>;
}

#[async_trait]
impl ItemsSource for Session {
    async fn items_page(
        &self,
        library: &LibraryId,
        offset: u32,
        limit: u32,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> Result<ItemsPage, SdkError> {
        self.items_sorted(library, offset, limit, sort, filter)
            .await
    }
}

/// A library's items, read a page at a time as the view scrolls, in one
/// order and filter. A new order or filter starts it again.
///
/// One page is read at a time: a `next_page` while one is in flight reads
/// nothing. A page that comes back after [`Self::reset`] is of the old
/// order and is dropped (the revision guard), so a fast scroll and a
/// change of sort never mix two orders in one grid.
pub struct LibraryPager {
    source: Arc<dyn ItemsSource>,
    library: LibraryId,
    page: u32,
    state: Mutex<PagerState>,
}

struct PagerState {
    sort: Option<ItemSort>,
    filter: Option<ItemFilter>,
    items: Vec<Item>,
    total: Option<u32>,
    /// One more for each reset.
    revision: u64,
    /// A page is being read.
    reading: bool,
}

impl LibraryPager {
    /// The items a page holds: a screenful or two of posters.
    pub const PAGE: u32 = 120;

    pub fn new(
        source: Arc<dyn ItemsSource>,
        library: LibraryId,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> Self {
        Self::with_page(source, library, sort, filter, Self::PAGE)
    }

    /// Pages of `page` items rather than [`Self::PAGE`], at least one.
    pub fn with_page(
        source: Arc<dyn ItemsSource>,
        library: LibraryId,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
        page: u32,
    ) -> Self {
        Self {
            source,
            library,
            page: page.max(1),
            state: Mutex::new(PagerState {
                sort,
                filter,
                items: Vec::new(),
                total: None,
                revision: 0,
                reading: false,
            }),
        }
    }

    /// The items read so far, in order.
    pub fn items(&self) -> Vec<Item> {
        self.state.lock().items.clone()
    }

    /// How many the library has in this filter; `None` before the first
    /// page.
    pub fn total(&self) -> Option<u32> {
        self.state.lock().total
    }

    /// Whether a page is left to read.
    pub fn has_more(&self) -> bool {
        let state = self.state.lock();
        state
            .total
            .is_none_or(|total| (state.items.len() as u32) < total)
    }

    /// Read the next page, added to [`Self::items`]: how many it added.
    /// 0 once all are read, while another page is being read, and for a
    /// page of an order since reset. A failed read changes nothing; ask
    /// again.
    pub async fn next_page(&self) -> Result<usize, SdkError> {
        let (offset, sort, filter, revision) = {
            let mut state = self.state.lock();
            let more = state
                .total
                .is_none_or(|total| (state.items.len() as u32) < total);
            if !more || state.reading {
                return Ok(0);
            }
            state.reading = true;
            (
                state.items.len() as u32,
                state.sort,
                state.filter.clone(),
                state.revision,
            )
        };
        let read = self
            .source
            .items_page(&self.library, offset, self.page, sort, filter)
            .await;
        let mut state = self.state.lock();
        if state.revision != revision {
            return Ok(0);
        }
        state.reading = false;
        let page = read?;
        state.total = Some(page.total);
        let added = page.items.len();
        state.items.extend(page.items);
        Ok(added)
    }

    /// Start again in another order or filter: nothing read, and a page
    /// in flight dropped.
    pub fn reset(&self, sort: Option<ItemSort>, filter: Option<ItemFilter>) {
        let mut state = self.state.lock();
        state.sort = sort;
        state.filter = filter;
        state.items.clear();
        state.total = None;
        state.revision += 1;
        state.reading = false;
    }
}

/// A show, with its seasons and the one its page opens on
/// ([`Session::show`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowDetail {
    pub detail: ItemDetail,
    /// The season the page opens on: the first numbered one, in the
    /// order the server lists them, skipping Specials (season 0), which
    /// come after the story; a show of Specials alone opens on them.
    /// `None` for a show with no seasons.
    pub default_season: Option<MediaId>,
    /// How many numbered seasons it has, Specials not counted: a page
    /// shows season tabs only for more than one.
    pub season_count: usize,
}

impl ShowDetail {
    pub fn of(detail: ItemDetail) -> Self {
        let seasons = || {
            detail.children.iter().filter_map(|i| match i {
                Item::Season(s) => Some(s),
                _ => None,
            })
        };
        let numbered = || seasons().filter(|s| s.season > 0);
        let default_season = numbered()
            .next()
            .or_else(|| seasons().next())
            .map(|s| s.id.clone());
        let season_count = numbered().count();
        Self {
            detail,
            default_season,
            season_count,
        }
    }
}

/// A season and its episodes, as a show page lists them
/// ([`ShowTree`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonEpisodes {
    pub season: Season,
    pub episodes: Vec<Item>,
}

/// A show with every season's episodes ([`Session::show_tree`]): what a
/// show page draws its tabs and lists from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowTree {
    pub show: ItemDetail,
    /// In the order the server lists them, Specials among them.
    pub seasons: Vec<SeasonEpisodes>,
}

/// What a Mark watched or Mark unwatched on a show page covers
/// ([`Session::mark_watched_in`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    /// Every episode.
    Show,
    /// One season's episodes, by its id.
    Season(MediaId),
    Episode(MediaId),
}

impl ShowTree {
    fn episodes(&self) -> impl Iterator<Item = &Item> {
        self.seasons.iter().flat_map(|s| &s.episodes)
    }

    /// The episode to watch next: the first part-way through, else the
    /// first not watched; `None` once all are watched.
    pub fn up_next(&self) -> Option<&Item> {
        let started = |e: &Item| e.resume_position().is_some_and(|p| !p.is_zero());
        self.episodes()
            .find(|e| !e.is_watched() && started(e))
            .or_else(|| self.episodes().find(|e| !e.is_watched()))
    }

    /// The season tab the page opens on, by its place: the season
    /// `opened` names (a season the user came to), else the one holding
    /// the episode up next, else the first. `None` with no seasons.
    pub fn first_tab(&self, opened: Option<&MediaId>) -> Option<usize> {
        if self.seasons.is_empty() {
            return None;
        }
        let picked = opened
            .and_then(|id| self.seasons.iter().position(|s| &s.season.id == id))
            .or_else(|| {
                let next = self.up_next()?.id();
                self.seasons
                    .iter()
                    .position(|s| s.episodes.iter().any(|e| e.id() == next))
            });
        Some(picked.unwrap_or(0))
    }

    /// The id one Mark watched for `scope` is sent with.
    pub fn scope_id(&self, scope: &WatchScope) -> MediaId {
        match scope {
            WatchScope::Show => self.show.item.id().clone(),
            WatchScope::Season(id) | WatchScope::Episode(id) => id.clone(),
        }
    }

    /// Marks `scope` watched or not in this copy, as the server does:
    /// each episode it covers, its position cleared either way.
    pub fn mark(&mut self, scope: &WatchScope, watched: bool) {
        for season in &mut self.seasons {
            let all = match scope {
                WatchScope::Show => true,
                WatchScope::Season(id) => &season.season.id == id,
                WatchScope::Episode(_) => false,
            };
            for item in &mut season.episodes {
                if let Item::Episode(e) = item
                    && (all || matches!(scope, WatchScope::Episode(id) if *id == e.id))
                {
                    e.playback.watched = watched;
                    e.playback.position = None;
                }
            }
        }
    }
}

/// Tracks of one album, as a queue takes them ([`Session::tracks_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTracks {
    /// The album; `None` when it could not be read for a track alone.
    pub album: Option<Item>,
    pub tracks: Vec<Item>,
}

impl Session {
    /// A library read a page at a time ([`LibraryPager`]).
    pub fn library_pager(
        &self,
        library: LibraryId,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> LibraryPager {
        LibraryPager::new(Arc::new(self.clone()), library, sort, filter)
    }

    /// Show `id`, with the season its page opens on.
    pub async fn show(&self, id: &MediaId) -> Result<ShowDetail, SdkError> {
        self.item(id).await.map(ShowDetail::of)
    }

    /// Show `id` with each season's episodes: the show, then its seasons
    /// read a few at once, in order. A season that cannot be read fails
    /// the whole.
    pub async fn show_tree(&self, id: &MediaId) -> Result<ShowTree, SdkError> {
        show_tree(id, |id| async move { self.item(&id).await }).await
    }

    /// Mark `scope` of `tree` watched or not: one request, with the
    /// season's or the show's id for more than one episode, then `tree`
    /// marked to match. A failed request leaves `tree` as it was.
    pub async fn mark_watched_in(
        &self,
        tree: &mut ShowTree,
        scope: WatchScope,
        watched: bool,
    ) -> Result<(), SdkError> {
        let id = tree.scope_id(&scope);
        if watched {
            self.mark_watched(&id).await?;
        } else {
            self.mark_unwatched(&id).await?;
        }
        tree.mark(&scope, watched);
        Ok(())
    }

    /// The tracks `id` stands for, by album, for Play next and Add to
    /// queue: a track with its album, an album's tracks, or each of an
    /// artist's albums in turn. Anything else stands for none.
    pub async fn tracks_of(&self, id: &MediaId) -> Result<Vec<AlbumTracks>, SdkError> {
        tracks_of(id, |id| async move { self.item(&id).await }).await
    }
}

/// The albums of an artist, or the seasons of a show, read at once.
const ALBUMS_AT_ONCE: usize = 4;

async fn show_tree<F, Fut>(id: &MediaId, item: F) -> Result<ShowTree, SdkError>
where
    F: Fn(MediaId) -> Fut,
    Fut: Future<Output = Result<ItemDetail, SdkError>>,
{
    let show = item(id.clone()).await?;
    let seasons = show.children.iter().filter_map(|i| match i {
        Item::Season(s) => Some(s.clone()),
        _ => None,
    });
    let seasons = futures_util::stream::iter(seasons)
        .map(|season| {
            let read = item(season.id.clone());
            async move {
                Ok::<_, SdkError>(SeasonEpisodes {
                    season,
                    episodes: read.await?.children,
                })
            }
        })
        .buffered(ALBUMS_AT_ONCE)
        .try_collect()
        .await?;
    Ok(ShowTree { show, seasons })
}

async fn tracks_of<F, Fut>(id: &MediaId, item: F) -> Result<Vec<AlbumTracks>, SdkError>
where
    F: Fn(MediaId) -> Fut,
    Fut: Future<Output = Result<ItemDetail, SdkError>>,
{
    match id.kind() {
        MediaKind::Track => {
            let track = item(id.clone()).await?.item;
            let album = match &track {
                // A queue plays a track without its album's details.
                Item::Track(t) => item(t.album_id.clone()).await.ok().map(|d| d.item),
                _ => None,
            };
            Ok(vec![AlbumTracks {
                album,
                tracks: vec![track],
            }])
        }
        MediaKind::Album => {
            let album = item(id.clone()).await?;
            Ok(vec![AlbumTracks {
                album: Some(album.item),
                tracks: album.children,
            }])
        }
        MediaKind::Artist => {
            // An album at a time would be one round trip each; a few at
            // once, kept in the artist's order.
            futures_util::stream::iter(item(id.clone()).await?.children)
                .filter(|album| std::future::ready(matches!(album, Item::Album(_))))
                .map(|album| {
                    let read = item(album.id().clone());
                    async move {
                        let detail = read.await?;
                        Ok(AlbumTracks {
                            album: Some(detail.item),
                            tracks: detail.children,
                        })
                    }
                })
                .buffered(ALBUMS_AT_ONCE)
                .try_collect()
                .await
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Album, Artist, Episode, Movie, Namespace, PlaybackState, Track};
    use std::collections::HashMap;

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn movie(key: &str) -> Item {
        Item::Movie(Movie {
            id: id(MediaKind::Movie, key),
            title: key.into(),
            year: None,
            release_date: None,
            summary: None,
            content_rating: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn detail(item: Item, children: Vec<Item>) -> ItemDetail {
        ItemDetail {
            item,
            children,
            markers: Vec::new(),
            playlists: Vec::new(),
            liked: None,
            liked_children: Vec::new(),
            copies: None,
        }
    }

    /// A library of `n` films, each page read noted.
    struct Library {
        n: u32,
        asked: Mutex<Vec<(u32, u32)>>,
    }

    #[async_trait]
    impl ItemsSource for Library {
        async fn items_page(
            &self,
            _: &LibraryId,
            offset: u32,
            limit: u32,
            _: Option<ItemSort>,
            _: Option<ItemFilter>,
        ) -> Result<ItemsPage, SdkError> {
            self.asked.lock().push((offset, limit));
            let end = (offset + limit).min(self.n);
            Ok(ItemsPage {
                items: (offset..end).map(|i| movie(&i.to_string())).collect(),
                total: self.n,
                unavailable: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn a_library_is_read_a_page_at_a_time_to_its_end() {
        let library = Arc::new(Library {
            n: 5,
            asked: Mutex::new(Vec::new()),
        });
        let pager = LibraryPager::with_page(library.clone(), library_id(), None, None, 2);
        assert!(pager.has_more());
        assert_eq!(pager.next_page().await.unwrap(), 2);
        assert_eq!(pager.next_page().await.unwrap(), 2);
        assert_eq!(pager.next_page().await.unwrap(), 1);
        assert!(!pager.has_more());
        assert_eq!(pager.next_page().await.unwrap(), 0);
        assert_eq!((pager.items().len(), pager.total()), (5, Some(5)));
        assert_eq!(*library.asked.lock(), [(0, 2), (2, 2), (4, 2)]);
        pager.reset(None, None);
        assert!(pager.items().is_empty() && pager.has_more());
        pager.next_page().await.unwrap();
        assert_eq!(library.asked.lock().last(), Some(&(0, 2)));
    }

    fn library_id() -> LibraryId {
        LibraryId::new(Namespace::Plex, "1").unwrap()
    }

    /// A library whose pages wait to be let go, one at a time.
    struct Held {
        release: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<()>>,
        asked: Mutex<Vec<Option<ItemSort>>>,
    }

    #[async_trait]
    impl ItemsSource for Held {
        async fn items_page(
            &self,
            _: &LibraryId,
            offset: u32,
            limit: u32,
            sort: Option<ItemSort>,
            _: Option<ItemFilter>,
        ) -> Result<ItemsPage, SdkError> {
            self.asked.lock().push(sort);
            self.release.lock().await.recv().await;
            Ok(ItemsPage {
                items: (offset..offset + limit)
                    .map(|i| movie(&i.to_string()))
                    .collect(),
                total: 100,
                unavailable: Vec::new(),
            })
        }
    }

    fn held() -> (Arc<Held>, tokio::sync::mpsc::Sender<()>) {
        let (release, held) = tokio::sync::mpsc::channel(4);
        let source = Arc::new(Held {
            release: tokio::sync::Mutex::new(held),
            asked: Mutex::new(Vec::new()),
        });
        (source, release)
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_of_the_order_before_a_reset_is_dropped() {
        let (source, release) = held();
        let pager = Arc::new(LibraryPager::with_page(
            source.clone(),
            library_id(),
            None,
            None,
            2,
        ));
        let reading = tokio::spawn({
            let pager = pager.clone();
            async move { pager.next_page().await }
        });
        tokio::task::yield_now().await;
        pager.reset(Some(ItemSort::DEFAULT), None);
        release.send(()).await.unwrap();
        assert_eq!(reading.await.unwrap().unwrap(), 0);
        assert!(pager.items().is_empty() && pager.total().is_none());
        release.send(()).await.unwrap();
        assert_eq!(pager.next_page().await.unwrap(), 2);
        assert_eq!(
            *source.asked.lock(),
            [None, Some(ItemSort::DEFAULT)],
            "the new order is read from its start"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_asked_while_one_is_read_reads_nothing() {
        let (source, release) = held();
        let pager = Arc::new(LibraryPager::with_page(
            source.clone(),
            library_id(),
            None,
            None,
            2,
        ));
        let reading = tokio::spawn({
            let pager = pager.clone();
            async move { pager.next_page().await }
        });
        tokio::task::yield_now().await;
        assert_eq!(pager.next_page().await.unwrap(), 0);
        release.send(()).await.unwrap();
        assert_eq!(reading.await.unwrap().unwrap(), 2);
        assert_eq!(source.asked.lock().len(), 1);
    }

    fn season(n: u32) -> Item {
        Item::Season(Season {
            id: id(MediaKind::Season, &n.to_string()),
            show_id: id(MediaKind::Show, "s"),
            show_title: None,
            season: n,
            title: format!("Season {n}"),
            artwork: Vec::new(),
        })
    }

    #[test]
    fn a_show_opens_on_its_first_numbered_season() {
        let show = ShowDetail::of(detail(movie("s"), vec![season(0), season(1), season(2)]));
        assert_eq!(show.default_season, Some(id(MediaKind::Season, "1")));
        assert_eq!(show.season_count, 2, "Specials are not counted");
        let specials = ShowDetail::of(detail(movie("s"), vec![season(0)]));
        assert_eq!(
            (specials.default_season, specials.season_count),
            (Some(id(MediaKind::Season, "0")), 0)
        );
        let none = ShowDetail::of(detail(movie("s"), Vec::new()));
        assert_eq!((none.default_season, none.season_count), (None, 0));
    }

    fn episode(season: u32, n: u32, position: Option<u64>, watched: bool) -> Item {
        Item::Episode(Episode {
            id: id(MediaKind::Episode, &format!("{season}x{n}")),
            show_id: id(MediaKind::Show, "s"),
            show_title: None,
            season_id: id(MediaKind::Season, &season.to_string()),
            season,
            episode: n,
            title: format!("S{season} E{n}"),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState {
                position: position.map(std::time::Duration::from_secs),
                watched,
                ..PlaybackState::default()
            },
        })
    }

    fn tree(seasons: Vec<(u32, Vec<Item>)>) -> ShowTree {
        ShowTree {
            show: detail(movie("s"), Vec::new()),
            seasons: seasons
                .into_iter()
                .map(|(n, episodes)| SeasonEpisodes {
                    season: match season(n) {
                        Item::Season(s) => s,
                        _ => unreachable!(),
                    },
                    episodes,
                })
                .collect(),
        }
    }

    fn title(item: Option<&Item>) -> Option<&str> {
        item.map(Item::title)
    }

    #[test]
    fn up_next_is_the_episode_part_way_through() {
        let t = tree(vec![
            (
                1,
                vec![episode(1, 1, None, true), episode(1, 2, None, false)],
            ),
            (2, vec![episode(2, 1, Some(600), false)]),
        ]);
        assert_eq!(title(t.up_next()), Some("S2 E1"));
        assert_eq!(t.first_tab(None), Some(1));
    }

    #[test]
    fn without_one_part_way_up_next_is_the_first_not_watched() {
        let t = tree(vec![(
            1,
            vec![episode(1, 1, None, true), episode(1, 2, None, false)],
        )]);
        assert_eq!(title(t.up_next()), Some("S1 E2"));
    }

    #[test]
    fn a_season_opened_is_the_first_tab() {
        let t = tree(vec![
            (1, vec![episode(1, 1, Some(60), false)]),
            (2, vec![episode(2, 1, None, false)]),
        ]);
        assert_eq!(t.first_tab(Some(&id(MediaKind::Season, "2"))), Some(1));
        assert_eq!(tree(Vec::new()).first_tab(None), None);
    }

    #[test]
    fn marking_a_season_marks_its_episodes_only() {
        let mut t = tree(vec![
            (1, vec![episode(1, 1, None, false)]),
            (
                2,
                vec![episode(2, 1, Some(60), false), episode(2, 2, None, false)],
            ),
        ]);
        let scope = WatchScope::Season(id(MediaKind::Season, "2"));
        assert_eq!(t.scope_id(&scope), id(MediaKind::Season, "2"));
        t.mark(&scope, true);
        let watched: Vec<bool> = t.episodes().map(Item::is_watched).collect();
        assert_eq!(watched, [false, true, true]);
        assert_eq!(t.seasons[1].episodes[0].resume_position(), None);
    }

    #[test]
    fn marking_the_show_marks_every_episode() {
        let mut t = tree(vec![
            (1, vec![episode(1, 1, None, false)]),
            (2, vec![episode(2, 1, None, false)]),
        ]);
        assert_eq!(t.scope_id(&WatchScope::Show), id(MediaKind::Movie, "s"));
        t.mark(&WatchScope::Show, true);
        assert_eq!(t.up_next(), None);
    }

    #[tokio::test]
    async fn a_show_tree_reads_the_show_then_each_season_in_order() {
        let show = detail(movie("s"), vec![season(1), season(2)]);
        let seasons: HashMap<MediaId, ItemDetail> = [
            detail(season(1), vec![episode(1, 1, None, false)]),
            detail(
                season(2),
                vec![episode(2, 1, None, false), episode(2, 2, None, false)],
            ),
        ]
        .into_iter()
        .map(|d| (d.item.id().clone(), d))
        .collect();
        let t = show_tree(&id(MediaKind::Movie, "s"), |asked| {
            let found = if asked == *show.item.id() {
                Some(show.clone())
            } else {
                seasons.get(&asked).cloned()
            };
            async move { found.ok_or(SdkError::NotFound(asked.to_string())) }
        })
        .await
        .unwrap();
        let counts: Vec<(u32, usize)> = t
            .seasons
            .iter()
            .map(|s| (s.season.season, s.episodes.len()))
            .collect();
        assert_eq!(counts, [(1, 1), (2, 2)]);
    }

    fn track(key: &str, album: &str) -> Item {
        Item::Track(Track {
            id: id(MediaKind::Track, key),
            album_id: id(MediaKind::Album, album),
            artist_id: id(MediaKind::Artist, "a"),
            artist_name: None,
            album_title: None,
            title: key.into(),
            track: None,
            disc: None,
            album_track_count: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn album(key: &str) -> Item {
        Item::Album(Album {
            id: id(MediaKind::Album, key),
            artist_id: id(MediaKind::Artist, "a"),
            artist_name: None,
            title: key.into(),
            year: None,
            track_count: None,
            artwork: Vec::new(),
        })
    }

    /// An artist "a" of albums "x" (two tracks) and "y" (one).
    fn catalog() -> HashMap<MediaId, ItemDetail> {
        let artist = Item::Artist(Artist {
            id: id(MediaKind::Artist, "a"),
            name: "A".into(),
            summary: None,
            artwork: Vec::new(),
        });
        [
            detail(artist, vec![album("x"), album("y")]),
            detail(album("x"), vec![track("1", "x"), track("2", "x")]),
            detail(album("y"), vec![track("3", "y")]),
            detail(track("1", "x"), Vec::new()),
        ]
        .into_iter()
        .map(|d| (d.item.id().clone(), d))
        .collect()
    }

    async fn of(key: MediaId) -> Vec<(Option<String>, usize)> {
        let catalog = catalog();
        tracks_of(&key, |id| {
            let found = catalog.get(&id).cloned();
            async move { found.ok_or(SdkError::NotFound(id.to_string())) }
        })
        .await
        .unwrap()
        .into_iter()
        .map(|g| (g.album.map(|a| a.title().to_owned()), g.tracks.len()))
        .collect()
    }

    #[tokio::test]
    async fn an_artist_stands_for_each_albums_tracks_in_turn() {
        assert_eq!(
            of(id(MediaKind::Artist, "a")).await,
            [(Some("x".into()), 2), (Some("y".into()), 1)]
        );
    }

    /// Six albums, the earlier ones slower to read: they still come in
    /// the artist's order, and no more than four are read at once.
    #[tokio::test(start_paused = true)]
    async fn an_artists_albums_are_read_a_few_at_once_in_order() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let keys = ["1", "2", "3", "4", "5", "6"];
        let artist = Item::Artist(Artist {
            id: id(MediaKind::Artist, "a"),
            name: "A".into(),
            summary: None,
            artwork: Vec::new(),
        });
        let artist = detail(artist, keys.iter().map(|k| album(k)).collect());
        let (reading, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let groups = tracks_of(&id(MediaKind::Artist, "a"), |asked| {
            let (artist, reading, most) = (artist.clone(), &reading, &most);
            async move {
                if asked.kind() == MediaKind::Artist {
                    return Ok(artist);
                }
                let now = reading.fetch_add(1, SeqCst) + 1;
                most.fetch_max(now, SeqCst);
                let key: u64 = asked.key().parse().unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(100 * (7 - key))).await;
                reading.fetch_sub(1, SeqCst);
                Ok(detail(
                    album(asked.key()),
                    vec![track(asked.key(), asked.key())],
                ))
            }
        })
        .await
        .unwrap();
        let order: Vec<_> = groups
            .iter()
            .map(|g| g.album.as_ref().unwrap().title().to_owned())
            .collect();
        assert_eq!(order, keys);
        assert_eq!(most.load(SeqCst), ALBUMS_AT_ONCE);
    }

    #[tokio::test]
    async fn a_track_stands_for_itself_with_its_album() {
        assert_eq!(of(id(MediaKind::Track, "1")).await, [(Some("x".into()), 1)]);
        assert_eq!(of(id(MediaKind::Album, "y")).await, [(Some("y".into()), 1)]);
        assert!(of(id(MediaKind::Movie, "m")).await.is_empty());
    }
}
