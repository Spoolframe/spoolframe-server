//! A local account's tie to a portable identity (P7-04), in words: the
//! badge on the account, and the question before severing it. Only the
//! identity's phone app ties one; the web UI never holds the root, so it
//! shows and severs only.

use spoolframe_admin_api::PortableTie;

use crate::logic::date;

/// Whose tie a page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The signed-in user's own, in My profile.
    Me,
    /// Another user's, on their page in Users: `name` as shown.
    User { name: String },
}

/// The badge's hover text: the whole id, which the fingerprint is short
/// for.
pub fn badge_title(tie: &PortableTie) -> String {
    format!("Portable identity {}", tie.id)
}

/// The line under the badge: "Tied on 2026-10-09".
pub fn tie_line(tie: &PortableTie) -> String {
    format!("Tied on {}", date(tie.tied_at))
}

/// What the card says while the account is tied to nothing.
pub fn untied_line(owner: &Owner) -> String {
    match owner {
        Owner::Me => "Not tied. Tie this account from the Spoolframe app on the phone \
                      that holds your portable identity."
            .into(),
        Owner::User { name } => {
            format!("Not tied. Only {name} can tie one, from the Spoolframe app on their phone.")
        }
    }
}

/// What the card says beside a tie, of how it is made again.
pub fn tied_note(owner: &Owner) -> String {
    match owner {
        Owner::Me => "Tied from the Spoolframe app on your phone. Sever it here; tie it \
                      again from the app."
            .into(),
        Owner::User { name } => {
            format!("Only {name} can tie one, from the Spoolframe app on their phone.")
        }
    }
}

/// The question before severing, and its button.
pub fn sever_question(owner: &Owner) -> (String, String) {
    let whose = match owner {
        Owner::Me => "your".to_owned(),
        Owner::User { name } => format!("{name}'s"),
    };
    let title = format!("Sever {whose} portable identity?");
    let text = match owner {
        Owner::Me => "Your account stays as it is: its password, passkeys, devices and \
                      watch history. The portable identity no longer opens it until it is \
                      tied again from your phone."
            .to_owned(),
        Owner::User { name } => format!(
            "{name}'s account stays as it is: its password, passkeys, devices and watch \
             history. The portable identity no longer opens it until {name} ties it again \
             from their phone."
        ),
    };
    (title, text)
}

/// What is said once it is severed.
pub fn severed(owner: &Owner) -> String {
    match owner {
        Owner::Me => "Portable identity severed".into(),
        Owner::User { name } => format!("{name}'s portable identity severed"),
    }
}

/// What the "Confirm it's you" dialog says the change is.
pub const CONFIRM_ABOUT: &str = "severing a portable identity";

#[cfg(test)]
mod tests {
    use super::*;

    fn tie() -> PortableTie {
        PortableTie {
            id: "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c".into(),
            fingerprint: "K7QX-3MPA-9TRD-WF2H".into(),
            // 2026-10-09T00:00:00Z.
            tied_at: 1_791_504_000,
        }
    }

    fn bob() -> Owner {
        Owner::User { name: "Bob".into() }
    }

    #[test]
    fn the_badge_shows_the_whole_id_on_hover() {
        assert_eq!(
            badge_title(&tie()),
            "Portable identity ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
    }

    #[test]
    fn the_line_says_when_it_was_tied() {
        assert_eq!(tie_line(&tie()), "Tied on 2026-10-09");
    }

    #[test]
    fn tying_is_pointed_to_the_phone_never_done_here() {
        assert!(untied_line(&Owner::Me).contains("Spoolframe app on the phone"));
        assert!(untied_line(&bob()).starts_with("Not tied. Only Bob can tie one"));
        assert!(tied_note(&Owner::Me).contains("tie it again from the app"));
        assert!(tied_note(&bob()).starts_with("Only Bob can tie one"));
    }

    #[test]
    fn the_question_names_whose_and_says_the_account_stays() {
        let (title, text) = sever_question(&Owner::Me);
        assert_eq!(title, "Sever your portable identity?");
        assert!(text.starts_with("Your account stays as it is"));
        let (title, text) = sever_question(&bob());
        assert_eq!(title, "Sever Bob's portable identity?");
        assert!(text.starts_with("Bob's account stays as it is"));
        assert!(text.contains("until Bob ties it again"));
    }

    #[test]
    fn what_is_said_after_names_whose() {
        assert_eq!(severed(&Owner::Me), "Portable identity severed");
        assert_eq!(severed(&bob()), "Bob's portable identity severed");
    }
}
