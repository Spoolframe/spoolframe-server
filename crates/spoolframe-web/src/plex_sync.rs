//! The words the pages use for Plex links and watch sync (P3-12): My
//! profile's Plex account card, the user page's Plex account section, and
//! the Libraries page's watch sync column and panel. Host-compiled, so
//! what they say is tested.

use spoolframe_admin_api::{PlexLink, PlexSyncRun, PlexUser, SyncDirection};

use crate::logic::ago;

/// Every direction, in the order the select lists them.
pub const DIRECTIONS: [SyncDirection; 4] = [
    SyncDirection::Off,
    SyncDirection::FromPlex,
    SyncDirection::ToPlex,
    SyncDirection::Both,
];

/// A direction as the select names it.
pub fn direction_name(direction: SyncDirection) -> &'static str {
    match direction {
        SyncDirection::Off => "Off",
        SyncDirection::FromPlex => "From Plex",
        SyncDirection::ToPlex => "To Plex",
        SyncDirection::Both => "Both",
    }
}

/// What a direction does, as WebLibrariesStates 5 explains it.
pub fn direction_meaning(direction: SyncDirection) -> &'static str {
    match direction {
        SyncDirection::Off => "Spoolframe keeps its own watched state.",
        SyncDirection::FromPlex => "Plays in Plex mark things watched here.",
        SyncDirection::ToPlex => "Plays here mark things watched in Plex.",
        SyncDirection::Both => {
            "Both ways; the newer change wins, and nothing bounces back and forth."
        }
    }
}

/// A direction as the select's value carries it, and back.
pub fn direction_value(direction: SyncDirection) -> &'static str {
    direction.config_name()
}

pub fn direction_of(value: &str) -> Option<SyncDirection> {
    SyncDirection::from_config_name(value)
}

/// "every 2 minutes", "every 90 seconds".
fn every(secs: u64) -> String {
    match secs {
        60 => "every minute".into(),
        s if s % 60 == 0 => format!("every {} minutes", s / 60),
        s => format!("every {s} seconds"),
    }
}

/// The sync panel's line about when runs happen.
pub fn schedule_line(interval_secs: u64) -> String {
    format!("Runs {} and soon after a change", every(interval_secs))
}

/// Who a link is to, for "Linked as …": the Plex account's email, else its
/// username; the config's owner link names no account.
pub fn linked_as(link: &PlexLink) -> String {
    match (&link.email, &link.username, link.by_config) {
        (Some(email), _, _) => format!("Linked as {email}"),
        (None, Some(name), _) => format!("Linked as {name}"),
        (None, None, true) => "Linked as the Plex servers' owner, by the server's config".into(),
        (None, None, false) => "Linked to a Plex account".into(),
    }
}

/// A user as the sync panel names them: "Andrey · andrey@example.com".
pub fn panel_name(user: &PlexUser) -> String {
    let name = user
        .display_name
        .clone()
        .unwrap_or_else(|| user.username.clone());
    let account = user
        .link
        .as_ref()
        .and_then(|l| l.email.clone().or_else(|| l.username.clone()));
    match account {
        Some(account) if account != name => format!("{name} · {account}"),
        _ => name,
    }
}

/// When a user last ran, for the sync panel.
pub fn last_run_line(now: i64, user: &PlexUser) -> String {
    match (&user.link, &user.last_run, user.running) {
        (None, _, _) => "Not linked to Plex · not synced".into(),
        (Some(_), _, true) => "Running now".into(),
        (Some(_), Some(run), false) => format!("Last run {}", ago(now, run.at)),
        (Some(_), None, false) => "Not run yet".into(),
    }
}

/// "12 pulled, 3 pushed", and the failures if there were any.
fn counts(run: &PlexSyncRun) -> String {
    let mut line = format!("{} pulled, {} pushed", run.pulled, run.pushed);
    if run.failed > 0 {
        line.push_str(&format!(", {} failed", run.failed));
    }
    line
}

/// "1 library", "3 libraries".
fn libraries(n: u32) -> String {
    if n == 1 {
        "1 library".into()
    } else {
        format!("{n} libraries")
    }
}

/// My profile's line under "Linked as …".
pub fn profile_line(now: i64, run: Option<&PlexSyncRun>, running: bool) -> String {
    match (run, running) {
        (_, true) => "Syncing now".into(),
        (Some(run), false) => format!(
            "Syncing {} · last run {}: {}",
            libraries(run.libraries),
            ago(now, run.at),
            counts(run)
        ),
        (None, false) => "Syncing soon: the first run is on its way".into(),
    }
}

/// The user page's line under "Linked as …".
pub fn user_page_line(now: i64, run: Option<&PlexSyncRun>, running: bool) -> String {
    match (run, running) {
        (_, true) => "Syncing now".into(),
        (Some(run), false) => format!("Last sync {} · {}", ago(now, run.at), counts(run)),
        (None, false) => "Not synced yet".into(),
    }
}

/// "14:20 left" of a PIN that expires at `expires_at`.
pub fn time_left(now: i64, expires_at: i64) -> String {
    let s = (expires_at - now).max(0);
    format!("{}:{:02} left", s / 60, s % 60)
}

/// The toast after a library's direction changes.
pub fn direction_changed(library: &str, direction: SyncDirection) -> String {
    let way = match direction {
        SyncDirection::Off => "off",
        SyncDirection::FromPlex => "from Plex",
        SyncDirection::ToPlex => "to Plex",
        SyncDirection::Both => "both ways",
    };
    format!("{library}: watch sync {way}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(email: Option<&str>, username: Option<&str>, by_config: bool) -> PlexLink {
        PlexLink {
            username: username.map(Into::into),
            email: email.map(Into::into),
            by_config,
            linked_at: (!by_config).then_some(0),
        }
    }

    fn user(name: &str, link: Option<PlexLink>, run: Option<PlexSyncRun>) -> PlexUser {
        PlexUser {
            username: name.to_lowercase(),
            display_name: Some(name.into()),
            link,
            last_run: run,
            running: false,
        }
    }

    fn run(at: i64) -> PlexSyncRun {
        PlexSyncRun {
            at,
            libraries: 3,
            pulled: 12,
            pushed: 3,
            skipped: 210,
            failed: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_select_offers_every_direction_and_reads_back_what_it_sent() {
        let names: Vec<_> = DIRECTIONS.iter().map(|&d| direction_name(d)).collect();
        assert_eq!(names, ["Off", "From Plex", "To Plex", "Both"]);
        for d in DIRECTIONS {
            assert_eq!(direction_of(direction_value(d)), Some(d));
        }
        assert_eq!(direction_of("sideways"), None);
    }

    #[test]
    fn the_panel_says_how_often_it_runs() {
        assert_eq!(
            schedule_line(120),
            "Runs every 2 minutes and soon after a change"
        );
        assert_eq!(
            schedule_line(60),
            "Runs every minute and soon after a change"
        );
        assert_eq!(
            schedule_line(90),
            "Runs every 90 seconds and soon after a change"
        );
    }

    #[test]
    fn a_link_is_named_by_its_email_else_its_username_else_as_the_config_s_owner() {
        assert_eq!(
            linked_as(&link(Some("andrey@example.com"), Some("andrey"), false)),
            "Linked as andrey@example.com"
        );
        assert_eq!(
            linked_as(&link(None, Some("andrey"), false)),
            "Linked as andrey"
        );
        assert!(linked_as(&link(None, None, true)).contains("server's config"));
    }

    #[test]
    fn each_user_in_the_panel_says_whether_and_when_they_ran() {
        let now = 1_000_000;
        let andrey = user(
            "Andrey",
            Some(link(Some("andrey@example.com"), None, false)),
            Some(run(now - 70)),
        );
        assert_eq!(panel_name(&andrey), "Andrey · andrey@example.com");
        assert_eq!(last_run_line(now, &andrey), "Last run 1 minute ago");
        let sam = user("Sam", None, None);
        assert_eq!(panel_name(&sam), "Sam");
        assert_eq!(last_run_line(now, &sam), "Not linked to Plex · not synced");
        let fresh = user("Alice", Some(link(None, Some("alice"), false)), None);
        assert_eq!(last_run_line(now, &fresh), "Not run yet");
    }

    #[test]
    fn my_profile_and_the_user_page_count_the_last_run() {
        let now = 1_000_000;
        let last = run(now - 70);
        assert_eq!(
            profile_line(now, Some(&last), false),
            "Syncing 3 libraries · last run 1 minute ago: 12 pulled, 3 pushed"
        );
        assert_eq!(
            user_page_line(now, Some(&last), false),
            "Last sync 1 minute ago · 12 pulled, 3 pushed"
        );
        let failing = PlexSyncRun {
            failed: 2,
            libraries: 1,
            ..last
        };
        assert_eq!(
            profile_line(now, Some(&failing), false),
            "Syncing 1 library · last run 1 minute ago: 12 pulled, 3 pushed, 2 failed"
        );
        assert_eq!(profile_line(now, None, true), "Syncing now");
    }

    #[test]
    fn a_pin_counts_down_in_minutes_and_seconds() {
        assert_eq!(time_left(100, 100 + 860), "14:20 left");
        assert_eq!(time_left(100, 50), "0:00 left");
    }

    #[test]
    fn a_direction_change_is_said_in_words() {
        assert_eq!(
            direction_changed("Movies", SyncDirection::FromPlex),
            "Movies: watch sync from Plex"
        );
        assert_eq!(
            direction_changed("Kids", SyncDirection::Off),
            "Kids: watch sync off"
        );
    }
}
