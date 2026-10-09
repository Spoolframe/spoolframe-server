//! An invite link (P6-01): text a user pastes or a QR code carries,
//! read as the SDK reads it. What reads as an invite is written back and
//! must read the same: the link an app shows must be the one it read.
#![no_main]

use libfuzzer_sys::fuzz_target;
use spoolframe_protocol::invite::parse_link;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(invite) = parse_link(text) {
        let again = parse_link(&invite.link()).expect("a written link reads back");
        assert_eq!(again, invite);
    }
});
