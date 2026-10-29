//! Following a playlist the server plays ([`Session::follow_playlist`]):
//! the server keeps the user's place and its shuffled round, and a
//! [`PlaylistCursor`] keeps the lineup an app plays from, in the
//! server's order, as the iOS and Linux players did.
//!
//! - The lineup is the entry playing and what the server says comes up
//!   (`UpcomingInPlaylist`), topped up when fewer than [`TOP_UP_BELOW`]
//!   are ahead.
//! - Moving on tells the server the place (`NextInPlaylist` after the
//!   entry before), unless it was told it already, or repeat one plays the
//!   entry again.
//! - A change of shuffle or repeat is saved and what is to come is read
//!   anew from the place.
//! - The server's calls run one at a time, in the order asked, each
//!   reading the lineup as it runs: a late call never moves the place
//!   back, and of the saves waiting only the latest is sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_protocol::playlists::{
    EntryId, PlaySettings, PlaylistEdit, PlaylistEntry, PlaylistNext, PlaylistPage, Repeat,
};

use crate::playlist_edits::Edited;
use crate::{SdkError, Session};

/// Fewer entries ahead than this, and the lineup asks for more.
pub const TOP_UP_BELOW: usize = 20;

/// How many entries a top-up asks the server for.
const UPCOMING: u32 = 100;

/// How many entries a preview read asks for at a time.
const PREVIEW_PAGE: u32 = 50;

/// Picks a fresh shuffle tries before it starts from the place.
const RANDOM_PICKS: usize = 3;

/// What a [`PlaylistCursor`] asks the server. [`Session`] is one; a test
/// brings its own.
#[async_trait]
pub trait PlaylistSource: Send + Sync {
    async fn page(&self, id: &str, offset: u32, limit: u32) -> Result<PlaylistPage, SdkError>;
    async fn play(
        &self,
        id: &str,
        from: Option<EntryId>,
        settings: PlaySettings,
    ) -> Result<Option<PlaylistNext>, SdkError>;
    async fn next_after(&self, id: &str, after: EntryId) -> Result<Option<PlaylistNext>, SdkError>;
    async fn upcoming(&self, id: &str, limit: u32) -> Result<Vec<PlaylistEntry>, SdkError>;
    async fn save_settings(&self, id: &str, settings: PlaySettings) -> Result<(), SdkError>;
    async fn edit(
        &self,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<Edited, SdkError>;
}

#[async_trait]
impl PlaylistSource for Session {
    async fn page(&self, id: &str, offset: u32, limit: u32) -> Result<PlaylistPage, SdkError> {
        self.playlist(id, offset, limit).await
    }

    async fn play(
        &self,
        id: &str,
        from: Option<EntryId>,
        settings: PlaySettings,
    ) -> Result<Option<PlaylistNext>, SdkError> {
        self.play_playlist(id, from, settings).await
    }

    async fn next_after(&self, id: &str, after: EntryId) -> Result<Option<PlaylistNext>, SdkError> {
        self.next_in_playlist(id, after).await
    }

    async fn upcoming(&self, id: &str, limit: u32) -> Result<Vec<PlaylistEntry>, SdkError> {
        self.upcoming_in_playlist(id, limit).await
    }

    async fn save_settings(&self, id: &str, settings: PlaySettings) -> Result<(), SdkError> {
        self.set_playlist_settings(id, settings).await
    }

    async fn edit(
        &self,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<Edited, SdkError> {
        let edited = self.edit_playlist(id, revision, edits).await?;
        Ok(Edited {
            revision: edited.revision,
            added: edited.added,
        })
    }
}

/// Where [`Session::follow_playlist`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayFrom {
    /// A fresh shuffle: an entry picked at random starts a new round. One
    /// unavailable is picked again, [`RANDOM_PICKS`] times, then the
    /// user's place.
    Random,
    Entry(EntryId),
    /// Where the user left it.
    Place,
}

/// What Up next says under the entries it lists ([`PlaylistCursor::up_next`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpNextNote {
    /// Nothing to say: the entries are what plays.
    None,
    /// Repeat one: the entry playing plays again.
    PlaysAgain,
    /// Nothing follows: the playlist ends after the entry playing.
    Ends,
    /// Shuffled on a server that cannot say its order: no entries named.
    Shuffled,
}

/// What plays after the entry playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpNext {
    pub entries: Vec<PlaylistEntry>,
    pub note: UpNextNote,
}

struct State {
    settings: PlaySettings,
    /// The entries played, the one playing at `at`, and what comes up.
    lineup: Vec<PlaylistEntry>,
    at: usize,
    /// The place the server was last told, by lineup index.
    told: Option<usize>,
    /// The server cannot say what comes up (older than
    /// `UpcomingInPlaylist`): each entry is asked for as it is needed.
    no_upcoming: bool,
}

impl State {
    fn current(&self) -> Option<&PlaylistEntry> {
        self.lineup.get(self.at)
    }

    fn ahead(&self) -> &[PlaylistEntry] {
        self.lineup.get(self.at + 1..).unwrap_or_default()
    }
}

/// A playlist the server plays, followed ([`Session::follow_playlist`]).
pub struct PlaylistCursor {
    source: Arc<dyn PlaylistSource>,
    id: String,
    state: Mutex<State>,
    /// The server's calls, one at a time in the order asked.
    calls: tokio::sync::Mutex<()>,
    /// The last save of the settings asked for.
    saves: AtomicU64,
}

impl Session {
    /// Play playlist `id` as the server plays it, `from` where with
    /// `settings`: the first entry, and what comes up after it.
    /// [`PlaylistCursor::current`] is `None` when nothing in it plays.
    pub async fn follow_playlist(
        &self,
        id: &str,
        from: PlayFrom,
        settings: PlaySettings,
    ) -> Result<PlaylistCursor, SdkError> {
        PlaylistCursor::start(Arc::new(self.clone()), id, from, settings, random_below).await
    }
}

/// A number below `n`, from the OS.
pub(crate) fn random_below(n: u32) -> u32 {
    let mut bytes = [0; 4];
    getrandom::fill(&mut bytes).expect("the OS gives random bytes");
    u32::from_le_bytes(bytes) % n.max(1)
}

impl PlaylistCursor {
    /// [`Session::follow_playlist`] over `source`; `random(n)` is a number
    /// below `n`.
    pub async fn start(
        source: Arc<dyn PlaylistSource>,
        id: &str,
        from: PlayFrom,
        settings: PlaySettings,
        mut random: impl FnMut(u32) -> u32 + Send,
    ) -> Result<Self, SdkError> {
        let from = match from {
            PlayFrom::Entry(entry) => Some(entry),
            PlayFrom::Place => None,
            PlayFrom::Random => {
                let total = source.page(id, 0, 1).await?.total;
                let mut picked = None;
                for _ in 0..RANDOM_PICKS.min(total as usize) {
                    let page = source.page(id, random(total), 1).await?;
                    if let Some(pick) = page.entries.first().filter(|e| !e.unavailable) {
                        picked = Some(pick.entry);
                        break;
                    }
                }
                picked
            }
        };
        let first = source.play(id, from, settings).await?;
        let lineup: Vec<PlaylistEntry> = first.into_iter().map(entry_of).collect();
        let cursor = Self {
            source,
            id: id.to_owned(),
            state: Mutex::new(State {
                settings,
                told: (!lineup.is_empty()).then_some(0),
                lineup,
                at: 0,
                no_upcoming: false,
            }),
            calls: tokio::sync::Mutex::new(()),
            saves: AtomicU64::new(0),
        };
        if cursor.current().is_some() {
            let _turn = cursor.calls.lock().await;
            cursor.top_up().await?;
        }
        Ok(cursor)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The entry playing.
    pub fn current(&self) -> Option<PlaylistEntry> {
        self.state.lock().current().cloned()
    }

    pub fn settings(&self) -> PlaySettings {
        self.state.lock().settings
    }

    /// The lineup's entries after the one playing, in the server's order.
    pub fn ahead(&self) -> Vec<PlaylistEntry> {
        self.state.lock().ahead().to_vec()
    }

    /// Moves on: the next entry, told to the server as the place; with
    /// repeat one, the same entry again. `None` at the playlist's end.
    pub async fn advance(&self) -> Result<Option<PlaylistEntry>, SdkError> {
        let moved = {
            let mut state = self.state.lock();
            if state.settings.repeat == Repeat::One {
                return Ok(state.current().cloned());
            }
            let moved = !state.ahead().is_empty();
            if moved {
                state.at += 1;
            }
            moved
        };
        let _turn = self.calls.lock().await;
        if moved {
            self.tell_place().await?;
            self.top_up().await?;
            return Ok(self.current());
        }
        // Nothing in the lineup: the server says what follows.
        let Some(after) = self.current().map(|e| e.entry) else {
            return Ok(None);
        };
        let Some(next) = self.source.next_after(&self.id, after).await? else {
            return Ok(None);
        };
        let mut state = self.state.lock();
        let keep = state.at + 1;
        state.lineup.truncate(keep);
        state.lineup.push(entry_of(next));
        state.at += 1;
        state.told = Some(state.at);
        Ok(state.current().cloned())
    }

    /// Plays `entry` now: the server's place and round start there, and
    /// what comes up is read anew.
    pub async fn pick(&self, entry: EntryId) -> Result<Option<PlaylistEntry>, SdkError> {
        let _turn = self.calls.lock().await;
        let settings = self.settings();
        let first = self.source.play(&self.id, Some(entry), settings).await?;
        {
            let mut state = self.state.lock();
            let Some(first) = first else {
                return Ok(None);
            };
            let keep = state.at + 1;
            state.lineup.truncate(keep);
            state.lineup.push(entry_of(first));
            state.at = state.lineup.len() - 1;
            state.told = Some(state.at);
        }
        self.read_ahead(settings).await?;
        Ok(self.current())
    }

    /// Shuffle or repeat changed: the server keeps it, and what is to come
    /// is its order from the place. Saves waiting behind a newer one are
    /// not sent.
    pub async fn set_settings(&self, settings: PlaySettings) -> Result<(), SdkError> {
        let was = {
            let mut state = self.state.lock();
            std::mem::replace(&mut state.settings, settings)
        };
        let save = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        let _turn = self.calls.lock().await;
        if self.saves.load(Ordering::SeqCst) != save {
            return Ok(());
        }
        let settings = self.settings();
        // The place first, so shuffle turned on shuffles from it; after,
        // when it waited out repeat one.
        if was.repeat != Repeat::One {
            self.tell_place().await?;
        }
        self.source.save_settings(&self.id, settings).await?;
        if was.repeat == Repeat::One {
            self.tell_place().await?;
        }
        self.read_ahead(settings).await
    }

    /// What plays after the entry playing, at most `limit`: the lineup,
    /// topped up first when short. With repeat one, the entry playing.
    /// From a server that cannot say what comes up, the playlist's own
    /// order is read past the entry playing, [`PREVIEW_PAGE`] at a time.
    pub async fn up_next(&self, limit: usize) -> Result<UpNext, SdkError> {
        let (settings, current, no_upcoming) = {
            let state = self.state.lock();
            (state.settings, state.current().cloned(), state.no_upcoming)
        };
        let Some(current) = current else {
            return Ok(UpNext {
                entries: Vec::new(),
                note: UpNextNote::Ends,
            });
        };
        if settings.repeat == Repeat::One {
            return Ok(UpNext {
                entries: vec![current],
                note: UpNextNote::PlaysAgain,
            });
        }
        if no_upcoming {
            if settings.shuffle {
                return Ok(UpNext {
                    entries: Vec::new(),
                    note: UpNextNote::Shuffled,
                });
            }
            let entries = self.preview(current.entry, settings, limit).await?;
            return Ok(with_note(entries));
        }
        if self.state.lock().ahead().len() < limit {
            let _turn = self.calls.lock().await;
            self.top_up().await?;
        }
        let state = self.state.lock();
        let mut listed = std::collections::HashSet::new();
        let entries = state
            .ahead()
            .iter()
            // Cut where the list comes round again.
            .take_while(|e| e.entry != current.entry && listed.insert(e.entry))
            .take(limit)
            .cloned()
            .collect();
        Ok(with_note(entries))
    }

    /// The server's place to the entry playing: the one after the entry
    /// before it. Not when it is there already, nor with repeat one, which
    /// would give that entry again. Run in turn.
    async fn tell_place(&self) -> Result<(), SdkError> {
        let (before, mine, at) = {
            let state = self.state.lock();
            if state.settings.repeat == Repeat::One || state.told == Some(state.at) || state.at == 0
            {
                return Ok(());
            }
            let Some(mine) = state.current().map(|e| e.entry) else {
                return Ok(());
            };
            (state.lineup[state.at - 1].entry, mine, state.at)
        };
        let told = self.source.next_after(&self.id, before).await?;
        let mut state = self.state.lock();
        // Told where it is unless the server went elsewhere (an edit).
        state.told = (told.is_some_and(|t| t.entry == mine) && state.at == at).then_some(at);
        Ok(())
    }

    /// Fewer than [`TOP_UP_BELOW`] ahead: what the server says comes up,
    /// those already ahead left out. Run in turn.
    async fn top_up(&self) -> Result<(), SdkError> {
        {
            let state = self.state.lock();
            if state.settings.repeat == Repeat::One
                || state.no_upcoming
                || state.ahead().len() >= TOP_UP_BELOW
            {
                return Ok(());
            }
        }
        let upcoming = match self.source.upcoming(&self.id, UPCOMING).await {
            Err(SdkError::NotOnServer { .. }) => {
                self.state.lock().no_upcoming = true;
                return Ok(());
            }
            other => other?,
        };
        let mut state = self.state.lock();
        let mut known: std::collections::HashSet<EntryId> =
            state.ahead().iter().map(|e| e.entry).collect();
        let more: Vec<PlaylistEntry> = upcoming
            .into_iter()
            .filter(|e| known.insert(e.entry))
            .collect();
        state.lineup.extend(more);
        Ok(())
    }

    /// What is to come, anew from the server's place, under `settings`.
    async fn read_ahead(&self, settings: PlaySettings) -> Result<(), SdkError> {
        if settings.repeat == Repeat::One {
            let mut state = self.state.lock();
            let keep = state.at + 1;
            state.lineup.truncate(keep);
            return Ok(());
        }
        let upcoming = match self.source.upcoming(&self.id, UPCOMING).await {
            Err(SdkError::NotOnServer { .. }) => {
                let mut state = self.state.lock();
                state.no_upcoming = true;
                let keep = state.at + 1;
                state.lineup.truncate(keep);
                return Ok(());
            }
            other => other?,
        };
        let mut state = self.state.lock();
        let keep = state.at + 1;
        state.lineup.truncate(keep);
        state.lineup.extend(upcoming);
        Ok(())
    }

    /// The playlist's own order after `entry`, at most `limit`, read page
    /// by page: entries unavailable, and watched ones when skipped, left
    /// out. With repeat all it goes round from the top.
    async fn preview(
        &self,
        entry: EntryId,
        settings: PlaySettings,
        limit: usize,
    ) -> Result<Vec<PlaylistEntry>, SdkError> {
        let plays =
            |e: &PlaylistEntry| !e.unavailable && !(settings.skip_watched && e.item.is_watched());
        let mut found = false;
        let mut before: Vec<PlaylistEntry> = Vec::new();
        let mut after: Vec<PlaylistEntry> = Vec::new();
        crate::playlist_edits::all_entries(&*self.source, &self.id, 0, PREVIEW_PAGE, |entries| {
            for e in entries {
                if e.entry == entry {
                    found = true;
                } else if found {
                    after.extend(plays(e).then(|| e.clone()));
                } else if before.len() < limit {
                    before.extend(plays(e).then(|| e.clone()));
                }
            }
            after.len() < limit
        })
        .await?;
        if !found {
            return Ok(Vec::new());
        }
        if settings.repeat == Repeat::All {
            after.extend(before);
        }
        after.truncate(limit);
        Ok(after)
    }
}

fn with_note(entries: Vec<PlaylistEntry>) -> UpNext {
    let note = match entries.is_empty() {
        true => UpNextNote::Ends,
        false => UpNextNote::None,
    };
    UpNext { entries, note }
}

fn entry_of(next: PlaylistNext) -> PlaylistEntry {
    PlaylistEntry {
        entry: next.entry,
        item: next.item,
        unavailable: false,
        liked: next.liked,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use spoolframe_protocol::playlists::{Kinds, PlaylistOwnerKind, PlaylistSummary, Visibility};
    use spoolframe_types::{Item, MediaId, MediaKind, Namespace, PlaybackState, Track};

    fn track(n: u64) -> Item {
        let id = |kind, key: &str| MediaId::new(Namespace::Plex, kind, key).unwrap();
        Item::Track(Track {
            id: id(MediaKind::Track, &n.to_string()),
            album_id: id(MediaKind::Album, "1"),
            artist_id: id(MediaKind::Artist, "1"),
            title: n.to_string(),
            track: None,
            disc: None,
            duration: None,
            playback: PlaybackState::default(),
            codec: None,
            bit_depth: None,
            sample_rate: None,
            album_track_count: None,
            artist_name: None,
            album_title: None,
            artwork: Vec::new(),
        })
    }

    pub(crate) fn entry(n: u64) -> PlaylistEntry {
        PlaylistEntry {
            entry: n,
            item: track(n),
            unavailable: false,
            liked: false,
        }
    }

    /// A playlist of entries 1..=n played in order, as the server plays
    /// it: a place, repeat all going round. Each call is noted.
    /// Edits add entries at the end; a refusal for a conflict is given
    /// for each revision in `conflicts`, in turn.
    pub(crate) struct Server {
        pub(crate) entries: Vec<PlaylistEntry>,
        added: Mutex<Vec<PlaylistEntry>>,
        state: Mutex<(Option<EntryId>, PlaySettings)>,
        calls: Mutex<Vec<String>>,
        upcoming: bool,
        pub(crate) conflicts: Mutex<Vec<u64>>,
        /// Edits say the entries their adds made.
        pub(crate) names_added: Mutex<bool>,
    }

    impl Server {
        pub(crate) fn new(n: u64) -> Arc<Self> {
            Arc::new(Self {
                entries: (1..=n).map(entry).collect(),
                added: Mutex::new(Vec::new()),
                state: Mutex::new((None, PlaySettings::default())),
                calls: Mutex::new(Vec::new()),
                upcoming: true,
                conflicts: Mutex::new(Vec::new()),
                names_added: Mutex::new(false),
            })
        }

        fn all(&self) -> Vec<PlaylistEntry> {
            let mut all = self.entries.clone();
            all.extend(self.added.lock().iter().cloned());
            all
        }

        fn after(&self, place: Option<EntryId>) -> Option<PlaylistEntry> {
            let settings = self.state.lock().1;
            let all = self.all();
            let at = place.and_then(|p| all.iter().position(|e| e.entry == p));
            let next = at.map_or(0, |at| at + 1);
            match all.get(next) {
                Some(e) => Some(e.clone()),
                None if settings.repeat == Repeat::All => all.first().cloned(),
                None => None,
            }
        }

        fn note(&self, call: String) {
            self.calls.lock().push(call);
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
    }

    fn next(e: PlaylistEntry) -> PlaylistNext {
        PlaylistNext {
            entry: e.entry,
            item: e.item,
            markers: Vec::new(),
            liked: false,
        }
    }

    #[async_trait]
    impl PlaylistSource for Server {
        async fn page(&self, _: &str, offset: u32, limit: u32) -> Result<PlaylistPage, SdkError> {
            self.note(format!("page {offset}"));
            let (place, settings) = *self.state.lock();
            let all = self.all();
            let entries: Vec<_> = all
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect();
            let summary = PlaylistSummary {
                id: "p".into(),
                name: "P".into(),
                kinds: Kinds {
                    tracks: true,
                    ..Kinds::default()
                },
                owner_kind: PlaylistOwnerKind::Mine,
                visibility: Visibility::Personal,
                can_edit: true,
                can_manage: true,
                revision: 1,
                pinned: None,
                builtin: None,
                owner_name: None,
                artwork: None,
                custom_art: false,
            };
            Ok(PlaylistPage {
                playlist: summary,
                entries,
                offset,
                total: all.len() as u32,
                playable: all.len() as u32,
                place,
                settings,
            })
        }

        async fn play(
            &self,
            _: &str,
            from: Option<EntryId>,
            settings: PlaySettings,
        ) -> Result<Option<PlaylistNext>, SdkError> {
            self.note(format!("play {from:?}"));
            self.state.lock().1 = settings;
            let first = match from {
                Some(from) => self.entries.iter().find(|e| e.entry == from).cloned(),
                None => {
                    let place = self.state.lock().0;
                    self.after(place)
                }
            };
            self.state.lock().0 = first.as_ref().map(|e| e.entry);
            Ok(first.map(next))
        }

        async fn next_after(
            &self,
            _: &str,
            after: EntryId,
        ) -> Result<Option<PlaylistNext>, SdkError> {
            self.note(format!("next after {after}"));
            let next_one = self.after(Some(after));
            self.state.lock().0 = next_one.as_ref().map(|e| e.entry);
            Ok(next_one.map(next))
        }

        async fn upcoming(&self, _: &str, limit: u32) -> Result<Vec<PlaylistEntry>, SdkError> {
            if !self.upcoming {
                return Err(SdkError::NotOnServer {
                    request: "UpcomingInPlaylist".into(),
                });
            }
            self.note("upcoming".into());
            let mut place = self.state.lock().0;
            let mut out = Vec::new();
            while out.len() < limit as usize {
                let Some(e) = self.after(place) else { break };
                place = Some(e.entry);
                out.push(e);
            }
            Ok(out)
        }

        async fn save_settings(&self, _: &str, settings: PlaySettings) -> Result<(), SdkError> {
            self.note(format!("save {:?}", settings.repeat));
            self.state.lock().1 = settings;
            Ok(())
        }

        async fn edit(
            &self,
            _: &str,
            revision: u64,
            edits: Vec<PlaylistEdit>,
        ) -> Result<Edited, SdkError> {
            self.note(format!("edit {revision}"));
            let mut conflicts = self.conflicts.lock();
            if !conflicts.is_empty() {
                let now = conflicts.remove(0);
                return Err(SdkError::PlaylistConflict { revision: now });
            }
            drop(conflicts);
            let mut made = Vec::new();
            for edit in edits {
                if let PlaylistEdit::Add { .. } = edit {
                    let n = self.all().len() as u64 + 1;
                    self.added.lock().push(entry(n));
                    made.push(n);
                }
            }
            Ok(Edited {
                revision: revision + 1,
                added: self.names_added.lock().then_some(made),
            })
        }
    }

    async fn follow(
        server: &Arc<Server>,
        from: PlayFrom,
        settings: PlaySettings,
    ) -> PlaylistCursor {
        PlaylistCursor::start(server.clone(), "p", from, settings, |_| 0)
            .await
            .unwrap()
    }

    fn entries(up: &UpNext) -> Vec<EntryId> {
        up.entries.iter().map(|e| e.entry).collect()
    }

    #[tokio::test]
    async fn following_reads_the_first_entry_and_what_comes_up() {
        let server = Server::new(5);
        let cursor = follow(&server, PlayFrom::Place, PlaySettings::default()).await;
        assert_eq!(cursor.current().map(|e| e.entry), Some(1));
        assert_eq!(cursor.ahead().len(), 4);
        assert_eq!(server.calls(), ["play None", "upcoming"]);
    }

    #[tokio::test]
    async fn moving_on_tells_the_place_once() {
        let server = Server::new(30);
        let cursor = follow(&server, PlayFrom::Place, PlaySettings::default()).await;
        assert_eq!(cursor.advance().await.unwrap().map(|e| e.entry), Some(2));
        assert_eq!(server.calls()[2], "next after 1");
        assert_eq!(server.state.lock().0, Some(2));
        // Told already: a settings save does not tell it again.
        cursor
            .set_settings(PlaySettings {
                repeat: Repeat::All,
                ..PlaySettings::default()
            })
            .await
            .unwrap();
        assert!(!server.calls()[3..].contains(&"next after 1".to_owned()));
    }

    #[tokio::test]
    async fn the_lineup_is_topped_up_below_twenty_ahead() {
        let server = Server::new(130);
        let cursor = follow(&server, PlayFrom::Place, PlaySettings::default()).await;
        assert_eq!(cursor.ahead().len(), 100);
        for _ in 0..81 {
            cursor.advance().await.unwrap();
        }
        // 19 ahead after the 82nd: topped up from the server's place.
        let ahead = cursor.ahead();
        assert_eq!(ahead.first().map(|e| e.entry), Some(83));
        assert_eq!(ahead.last().map(|e| e.entry), Some(130));
        let unique: std::collections::HashSet<_> = ahead.iter().map(|e| e.entry).collect();
        assert_eq!(unique.len(), ahead.len(), "no entry twice");
    }

    #[tokio::test]
    async fn repeat_one_plays_the_entry_again_and_tells_nothing() {
        let server = Server::new(3);
        let one = PlaySettings {
            repeat: Repeat::One,
            ..PlaySettings::default()
        };
        let cursor = follow(&server, PlayFrom::Place, one).await;
        assert_eq!(server.calls(), ["play None"], "nothing to top up");
        assert_eq!(cursor.advance().await.unwrap().map(|e| e.entry), Some(1));
        let up = cursor.up_next(10).await.unwrap();
        assert_eq!((entries(&up), up.note), (vec![1], UpNextNote::PlaysAgain));
        assert_eq!(server.calls().len(), 1);
    }

    #[tokio::test]
    async fn the_end_of_the_playlist_ends_it_and_repeat_all_goes_round() {
        let server = Server::new(2);
        let cursor = follow(&server, PlayFrom::Place, PlaySettings::default()).await;
        cursor.advance().await.unwrap();
        let up = cursor.up_next(10).await.unwrap();
        assert_eq!((entries(&up), up.note), (vec![], UpNextNote::Ends));
        assert_eq!(cursor.advance().await.unwrap(), None);

        let all = PlaySettings {
            repeat: Repeat::All,
            ..PlaySettings::default()
        };
        let server = Server::new(3);
        let cursor = follow(&server, PlayFrom::Place, all).await;
        let up = cursor.up_next(10).await.unwrap();
        assert_eq!(entries(&up), [2, 3], "cut where it comes round to 1");
        cursor.advance().await.unwrap();
        cursor.advance().await.unwrap();
        assert_eq!(cursor.advance().await.unwrap().map(|e| e.entry), Some(1));
    }

    #[tokio::test]
    async fn a_change_of_repeat_is_saved_after_the_place_and_reads_anew() {
        let server = Server::new(5);
        let cursor = follow(&server, PlayFrom::Place, PlaySettings::default()).await;
        cursor.advance().await.unwrap(); // 2, told
        let one = PlaySettings {
            repeat: Repeat::One,
            ..PlaySettings::default()
        };
        cursor.set_settings(one).await.unwrap();
        assert!(cursor.ahead().is_empty());
        cursor.set_settings(PlaySettings::default()).await.unwrap();
        assert_eq!(
            cursor.ahead().iter().map(|e| e.entry).collect::<Vec<_>>(),
            [3, 4, 5]
        );
        let calls = server.calls();
        assert_eq!(calls.iter().filter(|c| c.starts_with("save")).count(), 2);
    }

    #[tokio::test]
    async fn of_saves_waiting_only_the_latest_is_sent() {
        let server = Server::new(5);
        let cursor = Arc::new(follow(&server, PlayFrom::Place, PlaySettings::default()).await);
        let turn = cursor.calls.lock().await;
        let saving = |repeat| {
            let cursor = cursor.clone();
            tokio::spawn(async move {
                cursor
                    .set_settings(PlaySettings {
                        repeat,
                        ..PlaySettings::default()
                    })
                    .await
            })
        };
        let first = saving(Repeat::All);
        tokio::task::yield_now().await;
        let second = saving(Repeat::Off);
        tokio::task::yield_now().await;
        drop(turn);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let saves: Vec<_> = server
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("save"))
            .collect();
        assert_eq!(saves, ["save Off"]);
    }

    #[tokio::test]
    async fn a_fresh_shuffle_picks_again_past_an_unavailable_entry() {
        let mut server = Server::new(5);
        Arc::get_mut(&mut server).unwrap().entries[0].unavailable = true;
        let mut picks = [0, 3].into_iter();
        let cursor = PlaylistCursor::start(
            server.clone(),
            "p",
            PlayFrom::Random,
            PlaySettings {
                shuffle: true,
                ..PlaySettings::default()
            },
            move |_| picks.next().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(cursor.current().map(|e| e.entry), Some(4));
    }

    #[tokio::test]
    async fn without_upcoming_up_next_reads_the_playlist_past_the_entry() {
        let mut server = Server::new(120);
        {
            let server = Arc::get_mut(&mut server).unwrap();
            server.upcoming = false;
            server.entries[2].unavailable = true;
        }
        let cursor = follow(&server, PlayFrom::Entry(1), PlaySettings::default()).await;
        let up = cursor.up_next(60).await.unwrap();
        assert_eq!(up.entries.len(), 60);
        assert_eq!(
            up.entries[..2].iter().map(|e| e.entry).collect::<Vec<_>>(),
            [2, 4]
        );
        let pages = server
            .calls()
            .iter()
            .filter(|c| c.starts_with("page"))
            .count();
        assert_eq!(pages, 2, "read 50 at a time, only as far as needed");
        // Moving on asks the server each time.
        assert_eq!(cursor.advance().await.unwrap().map(|e| e.entry), Some(2));
        let shuffled = PlaySettings {
            shuffle: true,
            ..PlaySettings::default()
        };
        cursor.set_settings(shuffled).await.unwrap();
        let up = cursor.up_next(10).await.unwrap();
        assert_eq!(up.note, UpNextNote::Shuffled);
    }
}
