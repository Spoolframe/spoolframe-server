//! Users and devices › Federation (P8-01), apart from the page: the
//! servers paired with this one in words, and what a refused pairing
//! says. Experimental: the page is in the sidebar only while the server
//! names `federation` among its experimental features. Host-compiled, so
//! `cargo test` covers it.

use serde_json::Value;
use spoolframe_admin_api::{PeerEntry, PeerKey, error_code};

use crate::logic::ApiError;
use crate::portable::{days_ago, short_date};

/// The setting that turns it on (Settings › Experimental).
pub const SETTING: &str = "experimental.federation";

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
        "{online} · {synced} · {} here from its users · {} there by users here",
        plays(peer.plays_here),
        plays(peer.plays_there)
    );
    if peer.refused_here > 0 || peer.refused_there > 0 {
        line.push_str(&format!(
            " · {} refused here · {} refused there",
            peer.refused_here, peer.refused_there
        ));
    }
    line
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

/// The question before unpairing `peer`: its title and its text.
pub fn unpair_question(peer: &PeerEntry) -> (String, String) {
    (
        format!("Unpair {}?", peer.name),
        format!(
            "{} stops being paired with this server, and is told so if it can be reached. \
             Pairing again takes a new invite.",
            peer.name
        ),
    )
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
        "invite_expired" => "That invite has expired: ask for a new one.".into(),
        "invite_exhausted" => "That invite has been used: ask for a new one.".into(),
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
    fn the_question_names_the_server() {
        let (title, text) = unpair_question(&peer());
        assert_eq!(title, "Unpair Alice's server?");
        assert!(text.starts_with("Alice's server stops"), "{text}");
    }
}
