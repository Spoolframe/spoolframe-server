//! Changing a playlist's entries the way every app did (the Linux app's
//! core/playlists.rs): an edit retried once on a conflict, an add that
//! says which entries it made (for Undo), a random entry that can play,
//! and the entries read a page at a time.

use std::sync::Arc;

use futures_util::Stream;
use spoolframe_protocol::playlists::{EntryId, PlaylistEdit, PlaylistEntry};
use spoolframe_types::MediaId;

use crate::playlist_cursor::PlaylistSource;
use crate::{SdkError, Session};

/// The most entries a page of a playlist holds.
pub const PLAYLIST_PAGE: u32 = 200;

/// How many entries from a random place [`Session::random_playlist_entry`]
/// looks through for one that can play, before it reads the whole list.
const RANDOM_LOOK: u32 = 20;

/// What an edit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub revision: u64,
    /// The entries its adds made, when the server says (its
    /// `PlaylistEdited.added`); `None` from a server that does not, and
    /// [`Session::add_to_playlist`] reads the playlist again instead.
    pub added: Option<Vec<EntryId>>,
}

/// `edits` on `revision`; when someone changed the same entries since,
/// once more on the revision the refusal names, so an edit the other did
/// not undo still lands. A second refusal is the user's to see.
async fn edit_merging(
    source: &dyn PlaylistSource,
    id: &str,
    revision: u64,
    edits: Vec<PlaylistEdit>,
) -> Result<Edited, SdkError> {
    match source.edit(id, revision, edits.clone()).await {
        Err(SdkError::PlaylistConflict { revision }) => source.edit(id, revision, edits).await,
        done => done,
    }
}

/// Adds `items` to the end of playlist `id`, merging on a conflict: the
/// entries the add made, in order. Told apart from those before by id,
/// so an item already in the playlist, or one added twice, counts right.
async fn add_items(
    source: &dyn PlaylistSource,
    id: &str,
    items: &[MediaId],
) -> Result<Vec<EntryId>, SdkError> {
    let head = source.page(id, 0, 1).await?;
    // The last page before, and everything from it on after: an add
    // appends, so its entries are past what was there.
    let from = head.total.saturating_sub(PLAYLIST_PAGE);
    let before = match head.total {
        0 => Vec::new(),
        _ => entry_ids(&source.page(id, from, PLAYLIST_PAGE).await?.entries),
    };
    let adds = items
        .iter()
        .map(|item| PlaylistEdit::Add {
            id: item.clone(),
            before: None,
        })
        .collect();
    let edited = edit_merging(source, id, head.playlist.revision, adds).await?;
    if let Some(added) = edited.added {
        return Ok(added);
    }
    let mut after = Vec::new();
    all_entries(source, id, from, PLAYLIST_PAGE, |entries| {
        after.extend(entry_ids(entries));
        true
    })
    .await?;
    Ok(after.into_iter().filter(|e| !before.contains(e)).collect())
}

/// Playlist `id`'s entries from `from` to `visit`, a page of `size` at a
/// time, until its end or until `visit` says to stop (`false`).
pub(crate) async fn all_entries(
    source: &dyn PlaylistSource,
    id: &str,
    from: u32,
    size: u32,
    mut visit: impl FnMut(&[PlaylistEntry]) -> bool,
) -> Result<(), SdkError> {
    let mut offset = from;
    loop {
        let page = source.page(id, offset, size).await?;
        offset = page.offset + page.entries.len() as u32;
        if !visit(&page.entries) || page.entries.is_empty() || offset >= page.total {
            return Ok(());
        }
    }
}

/// An entry of playlist `id` that can play, at random; `random(n)` is a
/// number below `n`. One page from a random place, not the whole list:
/// an entry that cannot play gives the next that can. Only past
/// [`RANDOM_LOOK`] of them in a row is every entry read, to choose among
/// those that can. `None` when none can.
async fn random_entry(
    source: &dyn PlaylistSource,
    id: &str,
    mut random: impl FnMut(u32) -> u32,
) -> Result<Option<EntryId>, SdkError> {
    let head = source.page(id, 0, 1).await?;
    if head.total == 0 {
        return Ok(None);
    }
    let at = random(head.total).min(head.total - 1);
    let near = source.page(id, at, RANDOM_LOOK).await?;
    if let Some(entry) = near.entries.iter().find(|e| !e.unavailable) {
        return Ok(Some(entry.entry));
    }
    let mut playable = Vec::new();
    all_entries(source, id, 0, PLAYLIST_PAGE, |entries| {
        playable.extend(entries.iter().filter(|e| !e.unavailable).map(|e| e.entry));
        true
    })
    .await?;
    if playable.is_empty() {
        return Ok(None);
    }
    let pick = (random(playable.len() as u32) as usize).min(playable.len() - 1);
    Ok(Some(playable[pick]))
}

/// Every entry of playlist `id`, from `source`, a page at a time.
fn entries_of(
    source: Arc<dyn PlaylistSource>,
    id: String,
) -> impl Stream<Item = Result<PlaylistEntry, SdkError>> + Send {
    let pages = futures_util::stream::try_unfold(Some(0u32), move |offset| {
        let (source, id) = (source.clone(), id.clone());
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let page = source.page(&id, offset, PLAYLIST_PAGE).await?;
            let read = page.offset + page.entries.len() as u32;
            let next = (!page.entries.is_empty() && read < page.total).then_some(read);
            let entries = page.entries.into_iter().map(Ok::<_, SdkError>);
            Ok::<_, SdkError>(Some((futures_util::stream::iter(entries), next)))
        }
    });
    futures_util::TryStreamExt::try_flatten(pages)
}

fn entry_ids(entries: &[PlaylistEntry]) -> Vec<EntryId> {
    entries.iter().map(|e| e.entry).collect()
}

impl Session {
    /// [`Session::edit_playlist`], retried once on
    /// [`SdkError::PlaylistConflict`] with the revision the refusal names:
    /// an edit the other change did not undo still lands. A second
    /// conflict is returned, for "Someone changed this playlist; reload".
    pub async fn edit_playlist_merging(
        &self,
        id: &str,
        revision: u64,
        edits: Vec<PlaylistEdit>,
    ) -> Result<Edited, SdkError> {
        edit_merging(self, id, revision, edits).await
    }

    /// Adds `items` to the end of playlist `id` (a season its episodes, an
    /// album its tracks), merging on a conflict: the entries it made, for
    /// Undo ([`Session::remove_playlist_entries`]).
    pub async fn add_to_playlist(
        &self,
        id: &str,
        items: &[MediaId],
    ) -> Result<Vec<EntryId>, SdkError> {
        add_items(self, id, items).await
    }

    /// Takes `entries` out of playlist `id`, merging on a conflict: Undo
    /// of an add. Its new revision.
    pub async fn remove_playlist_entries(
        &self,
        id: &str,
        entries: &[EntryId],
    ) -> Result<u64, SdkError> {
        let revision = self.playlist(id, 0, 1).await?.playlist.revision;
        let edits = entries
            .iter()
            .map(|&entry| PlaylistEdit::Remove { entry })
            .collect();
        Ok(edit_merging(self, id, revision, edits).await?.revision)
    }

    /// An entry of playlist `id` that can play, at random (Play random);
    /// `None` when none can.
    pub async fn random_playlist_entry(&self, id: &str) -> Result<Option<EntryId>, SdkError> {
        random_entry(self, id, crate::playlist_cursor::random_below).await
    }

    /// Every entry of playlist `id`, read [`PLAYLIST_PAGE`] at a time as
    /// the stream is polled. A failed read ends it with the error.
    pub fn playlist_entries(
        &self,
        id: &str,
    ) -> impl Stream<Item = Result<PlaylistEntry, SdkError>> + Send + use<> {
        entries_of(Arc::new(self.clone()), id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist_cursor::tests::{Server, entry};
    use futures_util::TryStreamExt;
    use spoolframe_types::{MediaKind, Namespace};

    fn track(n: u64) -> MediaId {
        MediaId::new(Namespace::Plex, MediaKind::Track, n.to_string()).unwrap()
    }

    #[tokio::test]
    async fn an_edit_refused_for_a_conflict_is_sent_once_more_on_the_new_revision() {
        let server = Server::new(3);
        server.conflicts.lock().push(5);
        let edits = vec![PlaylistEdit::Remove { entry: 2 }];
        let edited = edit_merging(server.as_ref(), "p", 1, edits).await.unwrap();
        assert_eq!(edited.revision, 6);
        assert_eq!(server.calls(), ["edit 1", "edit 5"]);
    }

    #[tokio::test]
    async fn a_second_conflict_is_the_users_to_see() {
        let server = Server::new(3);
        server.conflicts.lock().extend([5, 9]);
        let edits = vec![PlaylistEdit::Remove { entry: 2 }];
        let refused = edit_merging(server.as_ref(), "p", 1, edits).await;
        assert_eq!(refused, Err(SdkError::PlaylistConflict { revision: 9 }));
    }

    #[tokio::test]
    async fn an_add_says_each_entry_it_made_even_of_an_item_already_there() {
        let server = Server::new(250);
        let made = add_items(server.as_ref(), "p", &[track(1), track(2)])
            .await
            .unwrap();
        assert_eq!(made, [251, 252]);
    }

    #[tokio::test]
    async fn an_add_takes_the_entries_the_server_names_when_it_does() {
        let server = Server::new(3);
        *server.names_added.lock() = true;
        let made = add_items(server.as_ref(), "p", &[track(1)]).await.unwrap();
        assert_eq!(made, [4]);
        let pages = server
            .calls()
            .iter()
            .filter(|c| c.starts_with("page"))
            .count();
        assert_eq!(pages, 2, "nothing read again");
    }

    #[tokio::test]
    async fn a_random_pick_is_the_entry_at_a_random_place() {
        let server = Server::new(30);
        let picked = random_entry(server.as_ref(), "p", |_| 7).await.unwrap();
        assert_eq!(picked, Some(8));
    }

    #[tokio::test]
    async fn a_random_pick_of_an_entry_that_cannot_play_is_the_next_that_can() {
        let mut server = Server::new(30);
        Arc::get_mut(&mut server).unwrap().entries[7].unavailable = true;
        let picked = random_entry(server.as_ref(), "p", |_| 7).await.unwrap();
        assert_eq!(picked, Some(9));
    }

    #[tokio::test]
    async fn a_random_pick_past_the_last_that_can_play_chooses_among_them_all() {
        let mut server = Server::new(30);
        for e in &mut Arc::get_mut(&mut server).unwrap().entries[5..] {
            e.unavailable = true;
        }
        let mut picks = [10, 2].into_iter();
        let picked = random_entry(server.as_ref(), "p", move |_| picks.next().unwrap())
            .await
            .unwrap();
        assert_eq!(picked, Some(3));
    }

    #[tokio::test]
    async fn a_random_pick_where_nothing_can_play_is_none() {
        let mut server = Server::new(3);
        for e in &mut Arc::get_mut(&mut server).unwrap().entries {
            e.unavailable = true;
        }
        assert_eq!(random_entry(server.as_ref(), "p", |_| 0).await, Ok(None));
    }

    #[tokio::test]
    async fn every_entry_streams_a_page_at_a_time() {
        let server = Server::new(450);
        let all: Vec<PlaylistEntry> = entries_of(server.clone(), "p".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(all.len(), 450);
        assert_eq!(all.last(), Some(&entry(450)));
        let pages = server
            .calls()
            .iter()
            .filter(|c| c.starts_with("page"))
            .count();
        assert_eq!(pages, 3);
    }
}
