//! Users and devices › Federation (P8-01), apart from the page: the
//! servers paired with this one in words, what a refused pairing says,
//! the check before pairing, and a paired server's own page (the
//! WebFederationPeer, WebFederationPair and WebFederationStates boards). Experimental: the page is in the sidebar only while the server
//! names `federation` among its experimental features. Host-compiled, so
//! `cargo test` covers it.

use serde_json::Value;
use spoolframe_admin_api::{
    LibraryKind, PairLookup, PeerDetail, PeerEntry, PeerKey, PlayDay, ReceivedLibrary, error_code,
};

use crate::catalog::bytes;
use crate::libraries::unit;
use crate::logic::{ApiError, thousands};
use crate::portable::{days_ago, short_date};

/// The setting that turns it on (Settings › Experimental).
pub const SETTING: &str = "experimental.federation";

/// One of `federation { … }`'s settings as Settings › Experimental shows
/// it, with "Show advanced settings" on (P8-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitField {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    /// The default and what it is, under the field.
    pub help: &'static str,
}

const fn field(
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    help: &'static str,
) -> LimitField {
    LimitField {
        key,
        label,
        unit,
        help,
    }
}

/// Federation's settings: the cache, then what a pair may send and ask.
/// Every one is advanced (the 2026-10-23 decision).
pub const LIMITS: &[LimitField] = &[
    field(
        "federation.sync_interval_secs",
        "Sync every",
        "s",
        "Default 3600 s · how often each paired server's libraries are synced; 60 at the least",
    ),
    field(
        "federation.cache_max_mb",
        "Images per server",
        "MiB",
        "Default 2048 MiB · a paired server's artwork kept here",
    ),
    field(
        "federation.cache_total_mb",
        "Images in all",
        "MiB",
        "Default 8192 MiB · every paired server's artwork together",
    ),
    field(
        "federation.sync_max_pages",
        "Pages per sync",
        "",
        "Default 2000 · of one library's sync; 10 at the least",
    ),
    field(
        "federation.sync_max_items",
        "Items per sync",
        "",
        "Default 200000 · of one library's sync; raise it for a very large library",
    ),
    field(
        "federation.max_cached_items",
        "Items per server",
        "",
        "Default 500000 · cached of one paired server, every library together",
    ),
    field(
        "federation.walk_reuse_secs",
        "Walk reused for",
        "s",
        "Default 60 s · one walk of a library answers every pair's sync; 10 at the least",
    ),
    field(
        "federation.max_streams",
        "Streams per connection",
        "",
        "Default 16 · a pair's requests answered at once, 4 to 1024; for connections made after",
    ),
    field(
        "federation.artwork_burst",
        "Images in a burst",
        "",
        "Default 200 · a pair's image requests at once, then 100 a second",
    ),
    field(
        "federation.assertion_burst",
        "Plays in a burst",
        "",
        "Default 30 · plays a pair's users start at once, then 1 a second",
    ),
    field(
        "federation.tokens_per_user",
        "Play tokens per user",
        "",
        "Default 10 · that work at once, for one user of a pair",
    ),
    field(
        "federation.tokens_per_server",
        "Play tokens per server",
        "",
        "Default 500 · that work at once, for a pair",
    ),
];

/// The experimental feature's name, as `serverStatus` lists it.
pub const FEATURE: &str = "federation";

/// Whether the server has federation on, by its experimental features.
pub fn is_on(experimental_features: &[String]) -> bool {
    experimental_features.iter().any(|f| f == FEATURE)
}

/// A pair's line under its name, seen at `now`: when, by whom, and whose
/// invite made it; or when it was unpaired.
pub fn peer_line(now: i64, peer: &PeerEntry) -> String {
    if let Some(at) = peer.revoked_at {
        return format!("Unpaired {}", short_date(now, at));
    }
    let whose = if peer.invite_id.is_some() {
        "with this server's invite"
    } else {
        "with their invite"
    };
    format!(
        "Paired {} by {} {whose}",
        short_date(now, peer.paired_at),
        peer.paired_by
    )
}

/// A live pair's state line, seen at `now`: online or not, when what it
/// shares was last synced here, and its plays, one number each way, with
/// the plays refused when there were any (P8-02, P8-03). No user of it is
/// named.
pub fn state_line(now: i64, peer: &PeerEntry) -> String {
    let online = if peer.connected { "Online" } else { "Offline" };
    let synced = match peer.last_sync {
        Some(at) => format!("synced {}", ago(now, at)),
        None => "nothing synced yet".to_owned(),
    };
    let mut line = format!(
        "{online} · {synced} · {} here from its users{} · {} there by users here{}",
        plays(peer.plays_here),
        modes_here(peer),
        plays(peer.plays_there),
        through_here(peer)
    );
    if peer.refused_here > 0 || peer.refused_there > 0 {
        line.push_str(&format!(
            " · {} refused here · {} refused there",
            peer.refused_here, peer.refused_there
        ));
    }
    line
}

/// How the plays served here went (P8-04): from here to the device, or
/// through the paired server when the device could not reach this one.
/// Nothing when none were served.
fn modes_here(peer: &PeerEntry) -> String {
    if peer.direct_here == 0 && peer.proxied_here == 0 {
        return String::new();
    }
    format!(
        " ({} direct, {} through it)",
        peer.direct_here, peer.proxied_here
    )
}

/// How many of this server's users' plays it played on to their devices
/// itself (P8-04): its own uplink carried them.
fn through_here(peer: &PeerEntry) -> String {
    match peer.proxied_there {
        0 => String::new(),
        n => format!(" ({n} through this server)"),
    }
}

fn plays(n: u64) -> String {
    match n {
        1 => "1 play".to_owned(),
        n => format!("{n} plays"),
    }
}

/// How long before `now` `at` was: by the minute within the hour, by the
/// hour within the day, then by the day.
fn ago(now: i64, at: i64) -> String {
    match now.saturating_sub(at) {
        ..60 => "just now".to_owned(),
        s @ 60..3_600 => format!("{} min ago", s / 60),
        s @ 3_600..86_400 => format!("{} h ago", s / 3_600),
        _ => days_ago(now, at),
    }
}

/// What the page says once `library` is shared with `peer`, or no longer.
pub fn shared(peer: &PeerEntry, library: &str, now_shared: bool) -> String {
    if now_shared {
        format!("{library} is shared with {}", peer.name)
    } else {
        format!("{library} is no longer shared with {}", peer.name)
    }
}

/// What the "Confirm it's you" dialog says sharing is.
pub const CONFIRM_SHARE: &str = "changing what this server shares with a paired server";

/// What a held key's row says, seen at `now`.
pub fn new_key_line(now: i64, key: &PeerKey) -> String {
    match key.rejected_at {
        Some(at) => format!(
            "A new key, rejected {}: refused for a day after.",
            days_ago(now, at)
        ),
        None => format!(
            "Presents a new key, first seen {}, last seen {}. Confirm only the key whose \
             fingerprint its admin reads you.",
            days_ago(now, key.first_seen),
            days_ago(now, key.last_seen)
        ),
    }
}

/// A key change that failed, in the page's words: the held keys having
/// changed since the page listed them is said as that.
pub fn key_change_error(e: &ApiError) -> String {
    if e.code == error_code::KEY_NOT_HELD {
        "The keys held for that server changed since this page listed them: check them again."
            .into()
    } else {
        e.explain()
    }
}

/// The question before unpairing `peer`, which this server shares
/// `shared` (the libraries' names) with: its title and its text. Where
/// the page does not know what is shared, `None`.
pub fn unpair_question(peer: &PeerEntry, shared: Option<&[String]>) -> (String, String) {
    let loses = match shared {
        Some([]) => String::new(),
        Some(titles) => format!("{} loses {}, ", peer.name, and_list(titles)),
        None => format!("{} loses what you share, ", peer.name),
    };
    (
        format!("Unpair {}?", peer.name),
        format!(
            "Sharing ends both ways now: {loses}your users lose {}'s libraries, and every \
             play token stops. Your users' progress stays. Pairing again takes a new invite.",
            peer.name
        ),
    )
}

/// `items` as a sentence lists them: "A", "A and B", "A, B and C".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The question before stopping sharing library `library` (by its name)
/// with `peer` (P8-03): its title and text.
pub fn revoke_question(peer: &PeerEntry, library: &str) -> (String, String) {
    (
        format!("Stop sharing {library} with {}?", peer.name),
        format!(
            "{}'s users can no longer browse or play it. Their plays of it stop now, and \
             what {} cached of it goes at its next sync.",
            peer.name, peer.name
        ),
    )
}

/// What is said once `peer` is unpaired.
pub fn unpaired(peer: &PeerEntry) -> String {
    format!("Unpaired from {}", peer.name)
}

/// What the "Confirm it's you" dialog says the change is.
pub const CONFIRM_ABOUT: &str = "changing which servers are paired with this one";

/// What an invite used or expired says, under "Invite can't be
/// redeemed".
pub const INVITE_SPENT: &str =
    "This invite was already used by another server, or has expired. Ask its admin for a new one.";

/// Whether a refused pairing is an invite used or expired: the dialog
/// titles it "Invite can't be redeemed".
pub fn is_spent(e: &ApiError) -> bool {
    e.code == error_code::PAIRING_REFUSED
        && e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .is_some_and(|r| matches!(r, "invite_expired" | "invite_exhausted"))
}

/// The check before pairing (WebFederationPair step 3), of what `lookUp`
/// found: whom to ask to read their fingerprint.
pub fn ask_to_compare(found: &PairLookup) -> String {
    match &found.inviter {
        Some(admin) => format!(
            "Ask {admin}, who made the invite, to read {}'s from their Federation page. \
             They must match.",
            found.name
        ),
        None => format!(
            "Ask {}'s admin to read theirs from their Federation page. They must match.",
            found.name
        ),
    }
}

/// What pairing does and does not do, under the fingerprint.
pub fn pairing_means(found: &PairLookup) -> [String; 3] {
    [
        format!(
            "You pair servers, not people. {}'s admin decides who there can watch what you share.",
            found.name
        ),
        "Nothing is shared yet. You choose libraries next; so does the other admin.".into(),
        "Media goes straight between server and app. If that path is closed, the other server \
         passes it on."
            .into(),
    ]
}

/// A library's movies, shows or artists, as Libraries counts them:
/// "1,204 movies", "1 show".
fn items_of(kind: LibraryKind, n: u64) -> String {
    format!("{} {}", thousands(n), unit(kind, n))
}

/// A received library's kind, as the API names it: `None` for one this
/// build does not know.
fn received_kind(library: &ReceivedLibrary) -> Option<LibraryKind> {
    match library.kind.as_str() {
        "movies" => Some(LibraryKind::Movies),
        "shows" => Some(LibraryKind::Shows),
        "music" => Some(LibraryKind::Music),
        _ => None,
    }
}

/// A received library's line: its items, the server's artwork kept here
/// (`artwork`, on its first row), and its revision.
pub fn received_line(library: &ReceivedLibrary, artwork: Option<u64>) -> String {
    let items = match received_kind(library) {
        Some(kind) => items_of(kind, library.items),
        None => format!("{} items", thousands(library.items)),
    };
    let mut line = items;
    if let Some(cached) = artwork {
        line.push_str(&format!(" · artwork {} cached", bytes(cached)));
    }
    line.push_str(&format!(" · revision {}", thousands(library.revision)));
    line
}

/// Whether a received library is fresh, seen at `now`: up to date while
/// its server is online, paused while it is not; and when it last synced.
/// The bool: whether it reads as good news.
pub fn freshness(now: i64, library: &ReceivedLibrary, online: bool) -> (String, bool) {
    match (library.synced_at, online) {
        (None, _) => ("Not synced yet".into(), false),
        (Some(at), true) => (format!("Up to date · {}", ago(now, at)), true),
        (Some(at), false) => (format!("Paused · synced {}", ago(now, at)), false),
    }
}

/// A pair's header line: when and by whom, or when it was unpaired.
pub fn paired_line(now: i64, peer: &PeerEntry) -> String {
    match peer.revoked_at {
        Some(at) => format!("Unpaired {}", short_date(now, at)),
        None => format!(
            "Paired {} by {}",
            short_date(now, peer.paired_at),
            peer.paired_by
        ),
    }
}

/// Online, or offline since when it was last seen.
pub fn presence(now: i64, detail: &PeerDetail) -> String {
    match (detail.peer.connected, detail.last_seen) {
        (true, _) => "Online".into(),
        (false, Some(at)) => format!("Offline since {}", short_date(now, at)),
        (false, None) => "Offline".into(),
    }
}

/// The week's plays across a pair, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Week {
    /// Of this server's items by the pair's users, and of them those its
    /// server passed on.
    pub here: u64,
    pub passed_on: u64,
    /// Of the pair's items by this server's users.
    pub there: u64,
}

pub fn week(days: &[PlayDay]) -> Week {
    days.iter().fold(Week::default(), |week, day| Week {
        here: week.here + day.plays_here,
        passed_on: week.passed_on + day.proxied_here,
        there: week.there + day.plays_there,
    })
}

/// The "From `name`" tile's line: direct, and passed on by its server.
pub fn from_line(peer: &PeerEntry, week: Week) -> String {
    format!(
        "{} direct · {} passed on by {}",
        week.here.saturating_sub(week.passed_on),
        week.passed_on,
        peer.name
    )
}

/// What a paired server's state says, over its page (WebFederationStates):
/// a title and a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub title: String,
    pub text: String,
    /// Red, not yellow: something refused, not something paused.
    pub bad: bool,
}

/// Each state `detail` is in, seen at `now`, on server `me` (this one's
/// name): its identity changed, it is offline, it stopped sharing a
/// library, its users' plays went through it. A held new key has its own
/// row.
pub fn states(now: i64, me: &str, detail: &PeerDetail) -> Vec<State> {
    let name = &detail.peer.name;
    let mut states = Vec::new();
    if detail.peer.revoked_at.is_some() {
        return states;
    }
    if detail.identity_refused_at.is_some() {
        states.push(State {
            title: format!("{name}'s identity changed"),
            text: format!(
                "A server answers with {name}'s key but a different server ID. It was refused, \
                 as it will be every time. If its admin rebuilt it, unpair and pair again."
            ),
            bad: true,
        });
    }
    if !detail.peer.connected {
        let titles: Vec<String> = detail.received.iter().map(|l| l.title.clone()).collect();
        let browse = if titles.is_empty() {
            String::new()
        } else {
            format!(
                " Your users can still browse its {} from the copy kept here;",
                and_list(&titles)
            )
        };
        states.push(State {
            title: presence(now, detail),
            text: if browse.is_empty() {
                "Updates are paused, and plays wait until it is back.".into()
            } else {
                format!("Updates are paused.{browse} plays wait until it is back.")
            },
            bad: false,
        });
    }
    for gone in &detail.withdrawn {
        states.push(State {
            title: format!("{name} stopped sharing {}", gone.title),
            text: format!(
                "It was removed from your users' apps {}; their progress on it stays on {me}.",
                days_ago(now, gone.withdrawn_at)
            ),
            bad: false,
        });
    }
    let passed = week(&detail.week).passed_on;
    if passed > 0 {
        states.push(State {
            title: format!(
                "{} went through {name}",
                if passed == 1 {
                    "1 play".to_owned()
                } else {
                    format!("{passed} plays")
                }
            ),
            text: format!(
                "Apps on {name} couldn't reach {me} directly, so {name} fetched the media and \
                 passed it on. Turning on remote access (Settings › Remote access) lets them \
                 play directly."
            ),
            bad: false,
        });
    }
    states
}

/// What the Unpair section says, on server `me`.
pub fn unpair_means(peer: &PeerEntry, me: &str) -> String {
    format!(
        "Ends sharing both ways at once: grants, play tokens and {0}'s cached libraries go; \
         your own libraries stay. Your users' progress on {0}'s titles stays on {me}, marked \
         no longer available.",
        peer.name
    )
}

/// A refused pairing in the page's words: by the other server's reason,
/// where the admin API names one. The reasons are
/// `spoolframe_protocol::federation::PairingRefused::name()`'s.
pub fn refusal(e: &ApiError) -> String {
    if e.code != error_code::PAIRING_REFUSED {
        return e.explain();
    }
    let reason = e
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match reason {
        "invite_invalid" => "The other server does not know that code: check the link.".into(),
        // One message for both (WebFederationStates): the admin does the
        // same either way.
        "invite_expired" | "invite_exhausted" => INVITE_SPENT.into(),
        "wrong_invite_kind" => "That invite is for an account, not for pairing a server.".into(),
        "already_paired" => "The two servers are paired already.".into(),
        "key_held" => "The other server holds this server's new key: its admin confirms it on \
                       their Federation page."
            .into(),
        "key_refused" => "The other server refuses this server's new key: its admin rejected \
                          it, or holds as many new keys as it keeps."
            .into(),
        "same_server_id" => "The other server has this server's id: they cannot pair.".into(),
        _ => e.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    /// Every federation setting is advanced, on the tab its switch is on.
    #[test]
    fn every_federation_setting_is_advanced_under_experimental() {
        for field in super::LIMITS {
            assert!(crate::advanced::is_advanced(field.key), "{}", field.key);
            assert!(
                crate::advanced::on_tab("experimental").contains(&field.key),
                "{}",
                field.key
            );
        }
        assert_eq!(super::LIMITS.len(), 12);
    }

    use super::*;

    const DAY: i64 = 86_400;
    /// 2026-10-22, noon UTC.
    const NOW: i64 = 1_792_670_400;

    fn peer() -> PeerEntry {
        PeerEntry {
            id: "id".into(),
            name: "Alice's server".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            endpoint_id: "ab".repeat(32),
            fingerprint: "PGJA-BHZ1-F68M-DFSE".into(),
            paired_by: "andrey".into(),
            invite_id: None,
            paired_at: NOW - 20 * DAY,
            revoked_at: None,
            held_keys: Vec::new(),
            connected: false,
            last_sync: None,
            plays_here: 0,
            plays_there: 0,
            refused_here: 0,
            refused_there: 0,
            direct_here: 0,
            proxied_here: 0,
            proxied_there: 0,
        }
    }

    #[test]
    fn stopping_a_share_says_what_the_paired_server_loses() {
        let (title, text) = revoke_question(&peer(), "Movies");
        assert_eq!(title, format!("Stop sharing Movies with {}?", peer().name));
        assert!(text.contains("can no longer browse or play it"), "{text}");
    }

    /// Online or not, the last sync, and one play count each way.
    #[test]
    fn a_live_pair_says_whether_it_is_online_when_it_synced_and_its_plays() {
        assert_eq!(
            state_line(NOW, &peer()),
            "Offline · nothing synced yet · 0 plays here from its users · 0 plays there by users here"
        );
        let busy = PeerEntry {
            connected: true,
            last_sync: Some(NOW - 5 * 60),
            plays_here: 1,
            plays_there: 12,
            ..peer()
        };
        assert_eq!(
            state_line(NOW, &busy),
            "Online · synced 5 min ago · 1 play here from its users · 12 plays there by users here"
        );
        let refused = PeerEntry {
            refused_here: 2,
            ..busy.clone()
        };
        assert!(
            state_line(NOW, &refused).ends_with(" · 2 refused here · 0 refused there"),
            "{}",
            state_line(NOW, &refused)
        );
        let hours = PeerEntry {
            last_sync: Some(NOW - 3 * 3_600),
            ..peer()
        };
        assert!(state_line(NOW, &hours).contains("synced 3 h ago"));
        let days = PeerEntry {
            last_sync: Some(NOW - 2 * DAY),
            ..peer()
        };
        assert!(state_line(NOW, &days).contains("synced 2 days ago"));
        assert!(
            state_line(
                NOW,
                &PeerEntry {
                    last_sync: Some(NOW - 10),
                    ..peer()
                }
            )
            .contains("synced just now")
        );
    }

    /// How the plays went (P8-04): those served here, direct or through
    /// the paired server, and those this server played on to its own
    /// users' devices; nothing said of a way no play went.
    #[test]
    fn a_live_pair_says_how_its_plays_went() {
        let served = PeerEntry {
            plays_here: 5,
            direct_here: 3,
            proxied_here: 2,
            plays_there: 4,
            proxied_there: 1,
            ..peer()
        };
        assert_eq!(
            state_line(NOW, &served),
            "Offline · nothing synced yet · 5 plays here from its users (3 direct, 2 through it) \
             · 4 plays there by users here (1 through this server)"
        );
        let minted_only = PeerEntry {
            plays_there: 4,
            ..peer()
        };
        assert!(!state_line(NOW, &minted_only).contains('('));
    }

    #[test]
    fn a_pair_says_when_by_whom_and_whose_invite() {
        assert_eq!(
            peer_line(NOW, &peer()),
            "Paired Oct 2 by andrey with their invite"
        );
        let ours = PeerEntry {
            invite_id: Some("i".into()),
            ..peer()
        };
        assert_eq!(
            peer_line(NOW, &ours),
            "Paired Oct 2 by andrey with this server's invite"
        );
        let gone = PeerEntry {
            revoked_at: Some(NOW - DAY),
            ..peer()
        };
        assert_eq!(peer_line(NOW, &gone), "Unpaired Oct 21");
    }

    #[test]
    fn a_held_key_says_when_it_was_seen_or_rejected() {
        let key = PeerKey {
            endpoint_id: "cd".repeat(32),
            fingerprint: "QAJ7-XQG9-2MDW-AN9H".into(),
            first_seen: NOW - 3 * DAY,
            last_seen: NOW - 2 * DAY,
            rejected_at: None,
        };
        let line = new_key_line(NOW, &key);
        assert!(line.contains("first seen 3 days ago"), "{line}");
        assert!(line.contains("last seen 2 days ago"), "{line}");
        let rejected = PeerKey {
            rejected_at: Some(NOW - 2 * DAY),
            ..key
        };
        assert!(new_key_line(NOW, &rejected).starts_with("A new key, rejected 2 days ago"));
    }

    #[test]
    fn a_key_that_is_no_longer_held_says_to_look_again() {
        let changed = ApiError {
            code: error_code::KEY_NOT_HELD,
            message: "that key is not held".into(),
            data: None,
        };
        assert!(key_change_error(&changed).contains("check them again"));
        let other = ApiError {
            code: error_code::FORBIDDEN,
            message: "admins only".into(),
            data: None,
        };
        assert_eq!(key_change_error(&other), other.explain());
    }

    #[test]
    fn the_feature_is_on_only_when_named() {
        assert!(is_on(&["http-playback".into(), "federation".into()]));
        assert!(!is_on(&["http-playback".into()]));
    }

    fn refused(reason: &str) -> ApiError {
        ApiError {
            code: error_code::PAIRING_REFUSED,
            message: "the server message".into(),
            data: Some(serde_json::json!({ "reason": reason })),
        }
    }

    #[test]
    fn a_refusal_is_said_by_its_reason() {
        assert!(refusal(&refused("invite_expired")).contains("expired"));
        assert!(refusal(&refused("already_paired")).contains("paired already"));
        assert!(refusal(&refused("key_held")).contains("new key"));
        assert!(refusal(&refused("key_refused")).contains("refuses"));
        assert!(refusal(&refused("same_server_id")).contains("this server's id"));
        // A reason this build does not know: the server's own words.
        assert_eq!(refusal(&refused("busy")), "the server message");
        let other = ApiError {
            code: error_code::FORBIDDEN,
            message: "admins only".into(),
            data: None,
        };
        assert_eq!(refusal(&other), other.explain());
    }

    #[test]
    fn the_question_names_the_server_and_what_it_loses() {
        let (title, text) = unpair_question(&peer(), None);
        assert_eq!(title, "Unpair Alice's server?");
        assert!(
            text.contains("Alice's server loses what you share"),
            "{text}"
        );
        let shared = ["Movies".to_owned(), "TV Shows".to_owned()];
        let (_, text) = unpair_question(&peer(), Some(&shared));
        assert!(
            text.starts_with("Sharing ends both ways now: Alice's server loses Movies and TV Shows, your users lose"),
            "{text}"
        );
        let (_, text) = unpair_question(&peer(), Some(&[]));
        assert!(
            text.starts_with("Sharing ends both ways now: your users lose"),
            "{text}"
        );
    }

    /// Used or expired, the invite says one thing: ask for a new one.
    #[test]
    fn a_spent_invite_says_one_thing() {
        assert_eq!(refusal(&refused("invite_expired")), INVITE_SPENT);
        assert_eq!(refusal(&refused("invite_exhausted")), INVITE_SPENT);
        assert!(is_spent(&refused("invite_exhausted")));
        assert!(!is_spent(&refused("already_paired")));
    }

    fn lookup(inviter: Option<&str>) -> PairLookup {
        PairLookup {
            name: "Bob's Media".into(),
            server_id: "mzxw6ytbojtg633c".into(),
            fingerprint: "M4QX-8KDT-2WNF-7RKD".into(),
            inviter: inviter.map(Into::into),
            expires_at: NOW + DAY,
        }
    }

    #[test]
    fn the_check_before_pairing_says_whom_to_ask() {
        assert!(ask_to_compare(&lookup(Some("bob"))).starts_with("Ask bob, who made the invite"));
        assert!(ask_to_compare(&lookup(None)).starts_with("Ask Bob's Media's admin"));
        let means = pairing_means(&lookup(None));
        assert!(means[0].starts_with("You pair servers, not people."));
        assert!(means[1].starts_with("Nothing is shared yet."));
        assert!(means[2].contains("the other server passes it on"));
    }

    fn received(kind: &str, items: u64, synced_at: Option<i64>) -> ReceivedLibrary {
        ReceivedLibrary {
            id: "plex:1".into(),
            title: "Movies".into(),
            kind: kind.into(),
            items,
            revision: 18_422,
            synced_at,
        }
    }

    #[test]
    fn a_received_library_says_its_items_artwork_and_revision() {
        assert_eq!(
            received_line(&received("movies", 412, None), Some(1_800_000_000)),
            "412 movies · artwork 1.8 GB cached · revision 18,422"
        );
        assert_eq!(
            received_line(&received("shows", 1, None), None),
            "1 show · revision 18,422"
        );
        assert_eq!(items_of(LibraryKind::Music, 3_812), "3,812 artists");
        assert!(received_line(&received("photos", 2, None), None).starts_with("2 items"));
    }

    #[test]
    fn a_received_library_is_up_to_date_only_while_online() {
        let synced = received("movies", 1, Some(NOW - 120));
        assert_eq!(
            freshness(NOW, &synced, true),
            ("Up to date · 2 min ago".into(), true)
        );
        assert_eq!(
            freshness(NOW, &synced, false),
            ("Paused · synced 2 min ago".into(), false)
        );
        assert_eq!(
            freshness(NOW, &received("movies", 1, None), true).0,
            "Not synced yet"
        );
    }

    fn detail() -> PeerDetail {
        PeerDetail {
            peer: PeerEntry {
                name: "Bob's Media".into(),
                connected: true,
                ..peer()
            },
            last_seen: None,
            identity_refused_at: None,
            received: Vec::new(),
            cached_bytes: 0,
            withdrawn: Vec::new(),
            week: Vec::new(),
        }
    }

    fn day(here: u64, passed: u64, there: u64) -> PlayDay {
        PlayDay {
            day: NOW,
            plays_here: here,
            proxied_here: passed,
            plays_there: there,
            proxied_there: 0,
        }
    }

    /// The tiles sum the days: from the pair, direct and passed on; and
    /// this server's users there.
    #[test]
    fn the_week_sums_its_days() {
        let summed = week(&[day(40, 2, 10), day(4, 1, 7)]);
        assert_eq!(
            summed,
            Week {
                here: 44,
                passed_on: 3,
                there: 17
            }
        );
        assert_eq!(
            from_line(&detail().peer, summed),
            "41 direct · 3 passed on by Bob's Media"
        );
    }

    #[test]
    fn an_online_pair_with_nothing_to_say_has_no_state() {
        assert!(states(NOW, "Home Media", &detail()).is_empty());
        assert_eq!(presence(NOW, &detail()), "Online");
        assert_eq!(paired_line(NOW, &detail().peer), "Paired Oct 2 by andrey");
    }

    #[test]
    fn an_offline_pair_says_since_when_and_what_stays_browsable() {
        let offline = PeerDetail {
            peer: PeerEntry {
                connected: false,
                ..detail().peer
            },
            last_seen: Some(NOW - 14 * DAY),
            received: vec![
                received("movies", 1, Some(NOW)),
                ReceivedLibrary {
                    title: "Music".into(),
                    ..received("music", 1, Some(NOW))
                },
            ],
            ..detail()
        };
        let said = states(NOW, "Home Media", &offline);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].title, "Offline since Oct 8");
        assert_eq!(
            said[0].text,
            "Updates are paused. Your users can still browse its Movies and Music from the copy \
             kept here; plays wait until it is back."
        );
        let never = PeerDetail {
            last_seen: None,
            received: Vec::new(),
            ..offline
        };
        assert_eq!(states(NOW, "Home Media", &never)[0].title, "Offline");
    }

    #[test]
    fn a_withdrawn_library_a_changed_identity_and_passed_on_plays_are_said() {
        let busy = PeerDetail {
            identity_refused_at: Some(NOW),
            withdrawn: vec![spoolframe_admin_api::WithdrawnLibrary {
                id: "plex:2".into(),
                title: "TV Shows".into(),
                withdrawn_at: NOW,
            }],
            week: vec![day(10, 3, 0)],
            ..detail()
        };
        let said = states(NOW, "Home Media", &busy);
        let titles: Vec<&str> = said.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Bob's Media's identity changed",
                "Bob's Media stopped sharing TV Shows",
                "3 plays went through Bob's Media"
            ]
        );
        assert!(said[0].bad && !said[1].bad);
        assert!(
            said[1].text.ends_with("stays on Home Media."),
            "{}",
            said[1].text
        );
        assert!(said[2].text.contains("Settings › Remote access"));
        // Unpaired, none of it is news.
        let gone = PeerDetail {
            peer: PeerEntry {
                revoked_at: Some(NOW),
                ..busy.peer.clone()
            },
            ..busy
        };
        assert!(states(NOW, "Home Media", &gone).is_empty());
    }

    #[test]
    fn unpairing_says_what_goes_and_what_stays() {
        let text = unpair_means(&detail().peer, "Home Media");
        assert!(text.contains("Bob's Media's cached libraries go"), "{text}");
        assert!(
            text.contains("stays on Home Media, marked no longer available"),
            "{text}"
        );
    }
}
