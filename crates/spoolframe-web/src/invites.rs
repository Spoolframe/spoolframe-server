//! Users and devices › Invites (P6-01): what the list, the create form,
//! the result and the revoke question say, and the QR code a phone scans.
//! The design's WebInvites, WebInviteCreate, WebInviteResult and
//! WebInvitesPhone boards.

use spoolframe_admin_api::{InviteEntry, InviteKind, NewInvite, Role, error_code, invite};

use crate::logic::ApiError;

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

/// "Expires after": what the select offers, an hour to 30 days, seven
/// days first chosen.
pub const EXPIRY_CHOICES: [(u64, &str); 7] = [
    (HOUR, "1 hour"),
    (12 * HOUR, "12 hours"),
    (DAY, "1 day"),
    (3 * DAY, "3 days"),
    (7 * DAY, "7 days"),
    (14 * DAY, "14 days"),
    (30 * DAY, "30 days"),
];

/// The create form as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteForm {
    pub note: String,
    pub uses: String,
    /// Seconds, one of [`EXPIRY_CHOICES`].
    pub expires: u64,
    /// For an account here; a portable guest's is always a consumer's.
    pub role: Role,
    /// An account here, a guest with their own identity (P7-05), or a
    /// server to pair (P8-01).
    pub kind: InviteKind,
}

impl Default for InviteForm {
    fn default() -> Self {
        Self {
            note: String::new(),
            uses: invite::DEFAULT_USES.to_string(),
            expires: invite::DEFAULT_EXPIRES_SECS,
            role: Role::Consumer,
            kind: InviteKind::CreateUser,
        }
    }
}

/// The invite `form` asks for, or why it cannot be one, in the form's
/// words. The server checks the same again.
pub fn new_invite(form: &InviteForm) -> Result<NewInvite, String> {
    let uses = form
        .uses
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=invite::MAX_USES).contains(n))
        .ok_or_else(|| format!("Uses is a number from 1 to {}.", invite::MAX_USES))?;
    let note = form.note.trim();
    if note.chars().count() > invite::MAX_NOTE_CHARS {
        return Err(format!(
            "A note is at most {} characters.",
            invite::MAX_NOTE_CHARS
        ));
    }
    // Only an invite that makes an account here takes a role: not a
    // portable user's, nor a server's.
    let takes_role = form.kind == InviteKind::CreateUser;
    Ok(NewInvite {
        kind: form.kind,
        note: (!note.is_empty()).then(|| note.to_owned()),
        uses: Some(uses),
        expires: Some(form.expires),
        role: takes_role.then_some(form.role),
    })
}

/// The result's title: "Invite for Mum's iPad", or "New invite" without
/// a note.
pub fn title(note: Option<&str>) -> String {
    match note {
        Some(note) => format!("Invite for {note}"),
        None => "New invite".into(),
    }
}

/// When an invite at `at` expires, seen at `now` (Unix seconds): "in 6
/// days", "in 23 hours", "in 40 minutes".
pub fn expires_in(now: i64, at: i64) -> String {
    let secs = (at - now).max(0);
    let (n, unit) = match secs {
        0..60 => return "in under a minute".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    if n == 1 {
        format!("in 1 {unit}")
    } else {
        format!("in {n} {unit}s")
    }
}

/// Under a day left: the list adds "· soon".
pub fn expires_soon(now: i64, at: i64) -> bool {
    at - now < DAY as i64
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The day and month of `local` (Unix seconds moved by the browser's
/// offset from UTC): "14 Oct".
pub fn day_month(local: i64) -> String {
    match spoolframe_types::CalendarDate::of_unix(local) {
        Some(date) => format!("{} {}", date.day(), MONTHS[usize::from(date.month() - 1)]),
        None => "?".to_owned(),
    }
}

/// "14 Oct, 18:40", of `local` as for [`day_month`].
pub fn day_month_time(local: i64) -> String {
    let secs = local.rem_euclid(86_400);
    format!(
        "{}, {:02}:{:02}",
        day_month(local),
        secs / 3600,
        secs / 60 % 60
    )
}

/// The list's "Uses left": "3 of 4".
pub fn uses_left(entry: &InviteEntry) -> String {
    format!("{} of {}", entry.uses_left, entry.uses)
}

/// The result's chip: "1 use", "4 uses".
pub fn uses_chip(uses: u32) -> String {
    if uses == 1 {
        "1 use".into()
    } else {
        format!("{uses} uses")
    }
}

/// A phone's card line: "3 of 4 left · expires in 2 days · by Andrey".
pub fn card_line(entry: &InviteEntry, now: i64) -> String {
    format!(
        "{} left · expires {} · by {}",
        uses_left(entry),
        expires_in(now, entry.expires_at),
        entry.created_by
    )
}

/// What a list row calls the invite: its note, else what kind it is.
pub fn name(entry: &InviteEntry) -> String {
    entry.note.clone().unwrap_or_else(|| {
        if entry.kind == InviteKind::Federation {
            "Server invite".into()
        } else {
            "Invite without a note".into()
        }
    })
}

/// "Revoke the Book club invite?" and what it does.
pub fn revoke_question(entry: &InviteEntry) -> (String, String) {
    let title = match &entry.note {
        Some(note) => format!("Revoke the {note} invite?"),
        None => "Revoke this invite?".into(),
    };
    let unused = match entry.uses_left {
        1 => "Its unused code stops working at once.".to_owned(),
        n => format!("Its {n} unused codes stop working at once."),
    };
    let made = entry.uses - entry.uses_left;
    let body = match made {
        0 => format!("{unused} No account was made with it."),
        1 => format!(
            "{unused} The account already made with it stays; remove it in Users and devices if \
             you need to."
        ),
        n => format!(
            "{unused} The {n} accounts already made with it stay; remove them in Users and \
             devices if you need to."
        ),
    };
    (title, body)
}

/// Why a revoke did not happen, in the list's words.
pub fn revoke_error(e: &ApiError) -> String {
    match e.code {
        error_code::NO_SUCH_INVITE => {
            "That invite can no longer make an account: it was used up, expired or revoked.".into()
        }
        _ => e.explain(),
    }
}

/// A QR code as the page draws it: `size` modules a side, and the dark
/// ones as one SVG path, a run of a row each. The quiet zone is the
/// card's padding, as the design has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qr {
    pub size: usize,
    pub path: String,
}

/// `data` as a QR code, or `None` if it is too long for one. Error
/// correction Q (a quarter may be lost): the mark the design puts in the
/// middle covers a twentieth.
pub fn qr(data: &str) -> Option<Qr> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(data, EcLevel::Q).ok()?;
    let size = code.width();
    let colors = code.to_colors();
    let mut path = String::new();
    for (y, row) in colors.chunks(size).enumerate() {
        let mut x = 0;
        while x < size {
            if row[x] == Color::Dark {
                let start = x;
                while x < size && row[x] == Color::Dark {
                    x += 1;
                }
                let run = x - start;
                path.push_str(&format!("M{start} {y}h{run}v1h-{run}z"));
            } else {
                x += 1;
            }
        }
    }
    Some(Qr { size, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uses: u32, uses_left: u32) -> InviteEntry {
        InviteEntry {
            id: "1".into(),
            kind: InviteKind::CreateUser,
            note: Some("Book club".into()),
            role: Role::Consumer,
            uses,
            uses_left,
            expires_at: 1_000_000 + 2 * DAY as i64,
            created_by: "Andrey".into(),
            created_at: 1_000_000,
        }
    }

    #[test]
    fn the_form_starts_at_one_use_seven_days_and_a_consumer() {
        assert_eq!(
            new_invite(&InviteForm::default()),
            Ok(NewInvite {
                kind: InviteKind::CreateUser,
                note: None,
                uses: Some(1),
                expires: Some(7 * DAY),
                role: Some(Role::Consumer),
            })
        );
    }

    #[test]
    fn a_form_s_note_is_trimmed_and_its_uses_read() {
        let form = InviteForm {
            note: "  Mum's iPad ".into(),
            uses: " 4".into(),
            expires: HOUR,
            role: Role::Admin,
            kind: InviteKind::CreateUser,
        };
        let made = new_invite(&form).unwrap();
        assert_eq!(made.note.as_deref(), Some("Mum's iPad"));
        assert_eq!(
            (made.uses, made.expires, made.role),
            (Some(4), Some(HOUR), Some(Role::Admin))
        );
    }

    /// P7-05: a guest's invite sends no role, whatever the form held.
    #[test]
    fn a_portable_guest_s_invite_has_no_role() {
        let form = InviteForm {
            role: Role::Admin,
            kind: InviteKind::PortableAccount,
            ..InviteForm::default()
        };
        let made = new_invite(&form).unwrap();
        assert_eq!(made.kind, InviteKind::PortableAccount);
        assert_eq!(made.role, None);
    }

    /// P8-01: a server's invite keeps its note, uses and expiry, and
    /// sends no role, as the server refuses one.
    #[test]
    fn a_server_s_invite_keeps_its_note_and_expiry_and_has_no_role() {
        let form = InviteForm {
            note: " Bob's server ".into(),
            uses: "2".into(),
            expires: DAY,
            role: Role::Admin,
            kind: InviteKind::Federation,
        };
        assert_eq!(
            new_invite(&form),
            Ok(NewInvite {
                kind: InviteKind::Federation,
                note: Some("Bob's server".into()),
                uses: Some(2),
                expires: Some(DAY),
                role: None,
            })
        );
    }

    #[test]
    fn uses_out_of_range_or_a_long_note_are_refused_in_the_form() {
        for uses in ["0", "101", "two", ""] {
            let form = InviteForm {
                uses: uses.into(),
                ..InviteForm::default()
            };
            assert!(new_invite(&form).is_err(), "{uses:?}");
        }
        let form = InviteForm {
            note: "x".repeat(65),
            ..InviteForm::default()
        };
        assert!(new_invite(&form).is_err());
    }

    #[test]
    fn the_expiry_choices_run_from_an_hour_to_30_days_with_the_default() {
        assert_eq!(EXPIRY_CHOICES[0].0, invite::MIN_EXPIRES_SECS);
        assert_eq!(EXPIRY_CHOICES[6].0, invite::MAX_EXPIRES_SECS);
        assert!(
            EXPIRY_CHOICES
                .iter()
                .any(|(secs, _)| *secs == invite::DEFAULT_EXPIRES_SECS)
        );
    }

    /// WebInvites' Expires column.
    #[test]
    fn expiry_reads_as_the_list_shows_it() {
        let now = 1_000_000;
        assert_eq!(expires_in(now, now + 6 * DAY as i64 + 5), "in 6 days");
        assert_eq!(expires_in(now, now + 23 * 3600 + 59), "in 23 hours");
        assert_eq!(expires_in(now, now + 3600), "in 1 hour");
        assert_eq!(expires_in(now, now + 40 * 60), "in 40 minutes");
        assert_eq!(expires_in(now, now + 10), "in under a minute");
        assert!(expires_soon(now, now + 23 * 3600));
        assert!(!expires_soon(now, now + 2 * DAY as i64));
    }

    #[test]
    fn dates_read_as_the_design_writes_them() {
        // 2026-10-14 18:40 UTC.
        let at = 1_791_936_000 + 18 * 3600 + 40 * 60;
        assert_eq!(day_month_time(at), "14 Oct, 18:40");
        assert_eq!(day_month(at), "14 Oct");
        assert_eq!(day_month(1_767_225_600), "1 Jan");
    }

    #[test]
    fn rows_and_cards_say_what_is_left_and_who_made_it() {
        let book_club = entry(4, 3);
        assert_eq!(uses_left(&book_club), "3 of 4");
        assert_eq!(
            card_line(&book_club, 1_000_000),
            "3 of 4 left · expires in 2 days · by Andrey"
        );
        assert_eq!(uses_chip(1), "1 use");
        assert_eq!(uses_chip(4), "4 uses");
        assert_eq!(title(Some("Mum's iPad")), "Invite for Mum's iPad");
        assert_eq!(title(None), "New invite");
    }

    #[test]
    fn an_invite_without_a_note_is_named_by_its_kind() {
        let unnamed = InviteEntry {
            note: None,
            ..entry(1, 1)
        };
        assert_eq!(name(&unnamed), "Invite without a note");
        let server = InviteEntry {
            kind: InviteKind::Federation,
            ..unnamed
        };
        assert_eq!(name(&server), "Server invite");
        assert_eq!(name(&entry(1, 1)), "Book club");
    }

    /// WebInvitesPhone 5.
    #[test]
    fn the_revoke_question_counts_the_unused_codes_and_the_accounts_made() {
        let (title, body) = revoke_question(&entry(4, 3));
        assert_eq!(title, "Revoke the Book club invite?");
        assert_eq!(
            body,
            "Its 3 unused codes stop working at once. The account already made with it stays; \
             remove it in Users and devices if you need to."
        );
        let (_, body) = revoke_question(&entry(1, 1));
        assert_eq!(
            body,
            "Its unused code stops working at once. No account was made with it."
        );
        let (_, body) = revoke_question(&entry(5, 2));
        assert!(body.contains("The 3 accounts already made"), "{body}");
    }

    #[test]
    fn a_link_is_a_qr_code_with_a_path_of_runs() {
        let link = "spoolframe://invite?server=3f9a1c7e5b2d8a04e6c1f7b39d52a8e0c4b6f1a2d3e5c7b9\
                    a1f0e2d4c6b8a0f1&name=Home%20Media&addr=203.0.113.24:41641\
                    &exp=1792003200&role=consumer&cap=QW7K-2MFP-XR4T";
        let code = qr(link).unwrap();
        // A version's side is 17 + 4 × its number.
        assert_eq!((code.size - 17) % 4, 0, "{}", code.size);
        // The top-left finder: seven dark modules in the first row.
        assert!(
            code.path.starts_with("M0 0h7v1h-7z"),
            "{}",
            &code.path[..40]
        );
        assert!(qr("x".repeat(4000).as_str()).is_none());
    }
}
