//! A local account's ties to portable identities (P7-04, P7-09), in
//! words: the list on the account, and the question before severing one.
//! Only an identity's app ties one; the web UI never holds the root, so it
//! shows and severs only. The words are canvas v109's WebTies board.

use spoolframe_admin_api::PortableTie;

use crate::logic::date;

/// Whose ties a page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The signed-in user's own, in My profile.
    Me,
    /// Another user's, on their page in Users: `name` as shown.
    User { name: String },
}

impl Owner {
    /// "your account", "Alice’s account".
    fn account(&self) -> String {
        match self {
            Owner::Me => "your account".into(),
            Owner::User { name } => format!("{name}’s account"),
        }
    }
}

/// The fingerprint's hover text: the whole id, which it is short for.
pub fn badge_title(tie: &PortableTie) -> String {
    format!("Portable identity {}", tie.id)
}

/// The words under the section's heading, for an account with `ties`.
pub fn heading_line(owner: &Owner, ties: usize) -> String {
    match (owner, ties) {
        (_, 0) => "Not tied.".into(),
        (Owner::Me, 1) => "The identity that opens your account.".into(),
        (Owner::Me, _) => {
            "Identities that open your account. Severing one leaves the others.".into()
        }
        (Owner::User { name }, 1) => format!("1 identity opens {name}’s account."),
        (Owner::User { name }, n) => format!(
            "{n} identities open {name}’s account. Severing one leaves the others; \
             the account keeps everything."
        ),
    }
}

/// The badge beside a user's status on their page: how many identities
/// open the account; none for an account with no ties.
pub fn count_badge(ties: usize) -> Option<String> {
    match ties {
        0 => None,
        1 => Some("1 identity".into()),
        n => Some(format!("{n} identities")),
    }
}

/// What the section says while the account is tied to nothing: where a
/// tie is made, which is never here.
pub fn untied_note(owner: &Owner) -> String {
    match owner {
        Owner::Me => "Have an identity in the Spoolframe app? Tie it to this account from the \
                      app, and devices that sign in with it open your account here: your \
                      history, your libraries. Your password keeps working."
            .into(),
        Owner::User { name } => {
            format!("Only {name} can tie one, from the Spoolframe app.")
        }
    }
}

/// A tie's line: "Tied Oct 2 · last used today", seen at `now`.
pub fn tie_line(now: i64, tie: &PortableTie) -> String {
    let used = match tie.last_used_at {
        None => "Not used yet".to_owned(),
        Some(at) => format!("last used {}", days_ago(now, at)),
    };
    format!("Tied {} · {used}", short_date(now, tie.tied_at))
}

/// `at` by the day, seen at `now`: today, yesterday, so many days ago in
/// the last month, else its date.
pub(crate) fn days_ago(now: i64, at: i64) -> String {
    match now.div_euclid(86_400) - at.div_euclid(86_400) {
        ..=0 => "today".into(),
        1 => "yesterday".into(),
        n @ 2..30 => format!("{n} days ago"),
        _ => short_date(now, at),
    }
}

/// "Oct 2" (UTC), with the year when it is not `now`'s: "Oct 2, 2025".
pub(crate) fn short_date(now: i64, at: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let full = date(at);
    let (year, rest) = full.split_once('-').unwrap_or_default();
    let (month, day) = rest.split_once('-').unwrap_or_default();
    let month = month
        .parse::<usize>()
        .ok()
        .and_then(|m| MONTHS.get(m.wrapping_sub(1)))
        .copied()
        .unwrap_or("?");
    let day = day.trim_start_matches('0');
    if date(now).starts_with(year) {
        format!("{month} {day}")
    } else {
        format!("{month} {day}, {year}")
    }
}

/// The question before severing one tie, with `others` left after it:
/// the title, and the text around the identity's fingerprint.
pub fn sever_question(owner: &Owner, others: usize) -> (String, String, String) {
    let keeps = "keeps its password, watch history, libraries and devices.";
    let rest = match others {
        0 => format!("The account {keeps}"),
        1 => format!("The other identity stays tied, and the account {keeps}"),
        2 => format!("The other two identities stay tied, and the account {keeps}"),
        n => format!("The other {n} identities stay tied, and the account {keeps}"),
    };
    (
        "Sever this identity?".into(),
        "Devices that sign in with ".into(),
        format!(" stop opening {}. {rest}", owner.account()),
    )
}

/// What is said once `tie` is severed.
pub fn severed(tie: &PortableTie) -> String {
    format!("Identity {} severed", tie.fingerprint)
}

/// What the "Confirm it's you" dialog says the change is.
pub const CONFIRM_ABOUT: &str = "severing a portable identity";

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-09T00:00:00Z.
    const OCT_9: i64 = 1_791_504_000;
    const DAY: i64 = 86_400;

    fn tie(tied_at: i64, last_used_at: Option<i64>) -> PortableTie {
        PortableTie {
            id: "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c".into(),
            fingerprint: "K7QX-3MPA-9TRD-WF2H".into(),
            tied_at,
            last_used_at,
        }
    }

    fn alice() -> Owner {
        Owner::User {
            name: "Alice".into(),
        }
    }

    #[test]
    fn the_fingerprint_shows_the_whole_id_on_hover() {
        assert_eq!(
            badge_title(&tie(OCT_9, None)),
            "Portable identity ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
    }

    #[test]
    fn a_row_says_when_it_was_tied_and_last_used() {
        let now = OCT_9 + 3 * DAY + 3600;
        assert_eq!(
            tie_line(now, &tie(OCT_9 - 7 * DAY, Some(now - 60))),
            "Tied Oct 2 · last used today"
        );
        assert_eq!(
            tie_line(now, &tie(OCT_9, Some(now - DAY))),
            "Tied Oct 9 · last used yesterday"
        );
        assert_eq!(
            tie_line(now, &tie(OCT_9, Some(OCT_9 - 4 * DAY))),
            "Tied Oct 9 · last used 7 days ago"
        );
        assert_eq!(
            tie_line(now, &tie(OCT_9, None)),
            "Tied Oct 9 · Not used yet"
        );
    }

    #[test]
    fn a_date_of_another_year_says_the_year() {
        let now = OCT_9;
        let a_year_ago = OCT_9 - 365 * DAY;
        assert_eq!(
            tie_line(now, &tie(a_year_ago, Some(a_year_ago))),
            "Tied Oct 9, 2025 · last used Oct 9, 2025"
        );
    }

    #[test]
    fn the_heading_counts_the_ties_and_says_whose() {
        assert_eq!(heading_line(&Owner::Me, 0), "Not tied.");
        assert_eq!(
            heading_line(&Owner::Me, 1),
            "The identity that opens your account."
        );
        assert_eq!(
            heading_line(&Owner::Me, 3),
            "Identities that open your account. Severing one leaves the others."
        );
        assert_eq!(heading_line(&alice(), 0), "Not tied.");
        assert_eq!(
            heading_line(&alice(), 1),
            "1 identity opens Alice’s account."
        );
        assert_eq!(
            heading_line(&alice(), 3),
            "3 identities open Alice’s account. Severing one leaves the others; \
             the account keeps everything."
        );
    }

    #[test]
    fn the_admin_s_badge_counts_identities_and_is_not_there_for_none() {
        assert_eq!(count_badge(0), None);
        assert_eq!(count_badge(1).as_deref(), Some("1 identity"));
        assert_eq!(count_badge(3).as_deref(), Some("3 identities"));
    }

    #[test]
    fn tying_is_pointed_to_the_app_never_done_here() {
        assert!(untied_note(&Owner::Me).contains("from the app"));
        assert!(untied_note(&alice()).starts_with("Only Alice can tie one"));
    }

    #[test]
    fn the_question_names_the_identity_whose_account_and_the_others() {
        let (title, before, after) = sever_question(&alice(), 2);
        assert_eq!(title, "Sever this identity?");
        assert_eq!(before, "Devices that sign in with ");
        assert_eq!(
            after,
            " stop opening Alice’s account. The other two identities stay tied, and the \
             account keeps its password, watch history, libraries and devices."
        );
        let (_, _, after) = sever_question(&Owner::Me, 1);
        assert_eq!(
            after,
            " stop opening your account. The other identity stays tied, and the account \
             keeps its password, watch history, libraries and devices."
        );
        let (_, _, after) = sever_question(&Owner::Me, 0);
        assert_eq!(
            after,
            " stop opening your account. The account keeps its password, watch history, \
             libraries and devices."
        );
        let (_, _, after) = sever_question(&alice(), 4);
        assert!(
            after.contains("The other 4 identities stay tied"),
            "{after}"
        );
    }

    #[test]
    fn what_is_said_after_names_the_identity() {
        assert_eq!(
            severed(&tie(OCT_9, None)),
            "Identity K7QX-3MPA-9TRD-WF2H severed"
        );
    }
}
