//! Playlist rules (PX-40): who may do what with a playlist, and how an
//! edit made on an old revision is redone on the newer one.
//!
//! Pure: the server's store gathers the facts, these judge them.

use std::collections::HashSet;

use spoolframe_protocol::playlists::EntryId;

/// What one viewer may do with one playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// They do not see it: it is not listed, and asking for it is
    /// "not found".
    None,
    /// They see it and play it.
    Read,
    /// They change its entries too.
    Edit,
    /// They rename it, change its kinds and sharing, and delete it.
    Own,
}

impl Access {
    pub fn can_read(self) -> bool {
        self >= Access::Read
    }

    pub fn can_edit(self) -> bool {
        self >= Access::Edit
    }

    pub fn can_manage(self) -> bool {
        self == Access::Own
    }
}

/// Where a viewer stands with a playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Standing {
    /// The viewer made it.
    pub owner: bool,
    /// The viewer is an admin here.
    pub admin: bool,
    /// It is seen by every user of the server.
    pub server_wide: bool,
    /// It is shared with the viewer: `Some(true)` to edit, `Some(false)`
    /// read only.
    pub share: Option<bool>,
}

/// The access `standing` gives. The owner owns it; a server-wide one is
/// every admin's too, and seen by everyone else (portable guests
/// included); a share gives what it says; anything else is not seen.
pub fn access(standing: Standing) -> Access {
    if standing.owner || (standing.server_wide && standing.admin) {
        return Access::Own;
    }
    match standing.share {
        Some(true) => Access::Edit,
        _ if standing.server_wide => Access::Read,
        Some(false) => Access::Read,
        None => Access::None,
    }
}

/// An edit was made on a revision whose entries someone else has changed
/// since in a way it cannot be redone over: "Someone changed this
/// playlist; reload".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict;

/// What has happened to a playlist's entries since an editor's revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Since {
    /// The revision the editor saw.
    pub base: u64,
    /// The playlist's revision now.
    pub current: u64,
    /// The oldest revision whose changes are still recorded: an editor
    /// that saw an older one cannot be told what changed.
    pub recorded_from: u64,
    /// The entries removed or moved by revisions after `base`.
    pub touched: HashSet<EntryId>,
}

/// Whether edits made on `since.base` that remove or move `touches` may
/// be redone on `since.current`. On the same revision they may. On an
/// older one an add is always redone; a remove or move is redone unless
/// its entry was removed or moved since, which is a [`Conflict`]. So is a
/// revision from the future, or one older than what is recorded.
pub fn rebase(since: &Since, touches: impl IntoIterator<Item = EntryId>) -> Result<(), Conflict> {
    if since.base == since.current {
        return Ok(());
    }
    if since.base > since.current || since.base < since.recorded_from {
        return Err(Conflict);
    }
    if touches
        .into_iter()
        .any(|entry| since.touched.contains(&entry))
    {
        return Err(Conflict);
    }
    Ok(())
}

/// One edit, its items resolved to new entries, as [`apply`] takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// New entries, in their order, before `before` (at the end when
    /// `None` or not there).
    Insert {
        entries: Vec<EntryId>,
        before: Option<EntryId>,
    },
    Remove(EntryId),
    Move {
        entry: EntryId,
        before: Option<EntryId>,
    },
}

/// Apply `step` to `order`, a playlist's entries in order. A remove or
/// move of an entry not in it is a [`Conflict`]: someone took it out.
pub fn apply(order: &mut Vec<EntryId>, step: Step) -> Result<(), Conflict> {
    match step {
        Step::Insert { entries, before } => {
            let at = position(order, before);
            order.splice(at..at, entries);
        }
        Step::Remove(entry) => {
            let at = order.iter().position(|e| *e == entry).ok_or(Conflict)?;
            order.remove(at);
        }
        Step::Move { entry, before } => {
            let at = order.iter().position(|e| *e == entry).ok_or(Conflict)?;
            order.remove(at);
            let to = position(order, before.filter(|b| *b != entry));
            order.insert(to, entry);
        }
    }
    Ok(())
}

/// Where "before `before`" is in `order`: the end when `None` or gone.
fn position(order: &[EntryId], before: Option<EntryId>) -> usize {
    before
        .and_then(|b| order.iter().position(|e| *e == b))
        .unwrap_or(order.len())
}

/// The entries to try, in turn, for what plays after `after` in `order`
/// (the playlist's order, or a shuffled round's), this round only: the
/// first that can be played plays. After nothing, the round from its
/// start. With `repeat_one`, `after` itself first. An `after` no longer in
/// `order` goes on from the start.
pub fn candidates(order: &[EntryId], after: Option<EntryId>, repeat_one: bool) -> Vec<EntryId> {
    let Some(after) = after else {
        return order.to_vec();
    };
    let Some(at) = order.iter().position(|e| *e == after) else {
        return order.to_vec();
    };
    let mut tried = Vec::with_capacity(order.len() - at);
    if repeat_one {
        tried.push(after);
    }
    tried.extend_from_slice(&order[at + 1..]);
    tried
}

/// `entries` in a shuffled order, the same for the same `seed`: one
/// round, each entry once.
pub fn shuffled(entries: &[EntryId], seed: u64) -> Vec<EntryId> {
    let mut order = entries.to_vec();
    let mut state = seed;
    // Fisher–Yates, drawn from splitmix64: no generator to depend on, and
    // a test names its seed.
    for i in (1..order.len()).rev() {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let j = (z % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::playlists::PlaylistEdit;
    use spoolframe_types::MediaId;

    fn touches(edits: &[PlaylistEdit]) -> Vec<EntryId> {
        edits.iter().filter_map(PlaylistEdit::touches).collect()
    }

    fn standing(f: impl FnOnce(&mut Standing)) -> Access {
        let mut s = Standing::default();
        f(&mut s);
        access(s)
    }

    #[test]
    fn a_personal_playlist_is_its_owner_s_alone() {
        assert_eq!(standing(|s| s.owner = true), Access::Own);
        assert_eq!(standing(|_| {}), Access::None);
        // Being an admin gives nothing over someone's own playlist.
        assert_eq!(standing(|s| s.admin = true), Access::None);
    }

    #[test]
    fn a_share_gives_read_or_edit_and_never_ownership() {
        assert_eq!(standing(|s| s.share = Some(false)), Access::Read);
        assert_eq!(standing(|s| s.share = Some(true)), Access::Edit);
        assert!(!standing(|s| s.share = Some(true)).can_manage());
        assert!(!standing(|s| s.share = Some(false)).can_edit());
    }

    #[test]
    fn a_server_wide_playlist_is_seen_by_all_and_managed_by_admins() {
        assert_eq!(standing(|s| s.server_wide = true), Access::Read);
        assert_eq!(
            standing(|s| {
                s.server_wide = true;
                s.admin = true;
            }),
            Access::Own
        );
        // An editor share still edits it.
        assert_eq!(
            standing(|s| {
                s.server_wide = true;
                s.share = Some(true);
            }),
            Access::Edit
        );
    }

    fn add(id: &str) -> PlaylistEdit {
        PlaylistEdit::Add {
            id: id.parse::<MediaId>().unwrap(),
            before: None,
        }
    }

    fn since(base: u64, current: u64, touched: &[EntryId]) -> Since {
        Since {
            base,
            current,
            recorded_from: 0,
            touched: touched.iter().copied().collect(),
        }
    }

    /// Two editors at revision 3: the first removed entry 10 (now 4).
    #[test]
    fn an_add_on_an_old_revision_is_redone() {
        let edits = vec![add("plex:movie:1")];
        assert_eq!(rebase(&since(3, 4, &[10]), touches(&edits)), Ok(()));
    }

    #[test]
    fn a_move_of_an_entry_the_other_removed_is_refused() {
        let edits = vec![PlaylistEdit::Move {
            entry: 10,
            before: None,
        }];
        assert_eq!(rebase(&since(3, 4, &[10]), touches(&edits)), Err(Conflict));
    }

    #[test]
    fn a_move_of_an_entry_the_other_did_not_touch_is_redone() {
        let edits = vec![PlaylistEdit::Move {
            entry: 11,
            before: Some(10),
        }];
        assert_eq!(rebase(&since(3, 4, &[10]), touches(&edits)), Ok(()));
    }

    #[test]
    fn a_revision_ahead_or_past_the_record_is_refused() {
        assert_eq!(rebase(&since(5, 4, &[]), []), Err(Conflict));
        let forgotten = Since {
            recorded_from: 3,
            ..since(2, 4, &[])
        };
        assert_eq!(
            rebase(&forgotten, touches(&[add("plex:movie:1")])),
            Err(Conflict)
        );
    }

    #[test]
    fn steps_insert_remove_and_move_in_order() {
        let mut order = vec![1, 2, 3];
        apply(
            &mut order,
            Step::Insert {
                entries: vec![7, 8],
                before: Some(2),
            },
        )
        .unwrap();
        assert_eq!(order, [1, 7, 8, 2, 3]);
        apply(&mut order, Step::Remove(7)).unwrap();
        apply(
            &mut order,
            Step::Move {
                entry: 3,
                before: Some(1),
            },
        )
        .unwrap();
        assert_eq!(order, [3, 1, 8, 2]);
        apply(
            &mut order,
            Step::Move {
                entry: 3,
                before: None,
            },
        )
        .unwrap();
        assert_eq!(order, [1, 8, 2, 3]);
    }

    #[test]
    fn an_anchor_that_is_gone_puts_the_entry_at_the_end() {
        let mut order = vec![1, 2];
        apply(
            &mut order,
            Step::Insert {
                entries: vec![5],
                before: Some(99),
            },
        )
        .unwrap();
        assert_eq!(order, [1, 2, 5]);
    }

    #[test]
    fn a_step_on_an_entry_not_there_is_a_conflict() {
        let mut order = vec![1, 2];
        assert_eq!(apply(&mut order, Step::Remove(9)), Err(Conflict));
        assert_eq!(
            apply(
                &mut order,
                Step::Move {
                    entry: 9,
                    before: None
                }
            ),
            Err(Conflict)
        );
        assert_eq!(order, [1, 2]);
    }

    #[test]
    fn what_plays_next_is_the_rest_of_the_round() {
        let order = [1, 2, 3, 4];
        assert_eq!(candidates(&order, None, false), [1, 2, 3, 4]);
        assert_eq!(candidates(&order, Some(2), false), [3, 4]);
        assert!(candidates(&order, Some(4), false).is_empty());
        // Repeat one: the same again first.
        assert_eq!(candidates(&order, Some(4), true), [4]);
        assert_eq!(candidates(&order, Some(2), true), [2, 3, 4]);
        // An entry taken out since: from the start.
        assert_eq!(candidates(&order, Some(9), false), [1, 2, 3, 4]);
    }

    #[test]
    fn a_shuffled_round_has_every_entry_once() {
        let entries: Vec<EntryId> = (1..=20).collect();
        let round = shuffled(&entries, 7);
        assert_ne!(round, entries);
        let mut sorted = round.clone();
        sorted.sort();
        assert_eq!(sorted, entries);
        assert_eq!(shuffled(&entries, 7), round);
        assert_ne!(shuffled(&entries, 8), round);
    }
}
