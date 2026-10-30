//! What an encode's `CODECS` says: the string its own record gives.
//!
//! The multivariant playlist is asked for before a run has written its
//! init segment, and of a ladder's rungs only one runs. So the string comes
//! from a probe, one frame of a test pattern through the same encoder at
//! the same size, rate and depth, read from the MP4 it writes. Measured on
//! nimble's 7900 XTX: its string is the film's, for HEVC, H.264 and AV1.
//! Kept for the process, by [`Key`]; a run's own init replaces it.
//!
//! A probe takes no place in the budget: a real encode must never be turned
//! away for one. A card at its limit is not probed, and that answer guesses
//! ([`super::hls`]'s bucket).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use spoolframe_types::CodecString;

/// How long a failed probe is not tried again.
const FAILED_FOR: Duration = Duration::from_secs(60);

/// What an encode's record depends on. Its rate is in here because a level
/// has a maximum rate: 20 Mbit/s of 1080p is not the level 8 Mbit/s is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    /// [`super::pick::RunOn::key`]: the card, or software.
    pub(crate) card: String,
    pub(crate) encoder: String,
    pub(crate) size: (u32, u32),
    /// Frames a second, rounded up: 23.976 and 24 are one.
    pub(crate) frame_rate: u32,
    pub(crate) ten_bits: bool,
    pub(crate) max_bitrate: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    Probing,
    Known(CodecString),
    Failed(Instant),
}

/// The strings read, for the process.
#[derive(Debug, Default)]
pub(crate) struct CodecStrings(Mutex<HashMap<Key, Entry>>);

impl CodecStrings {
    pub(crate) fn known(&self, key: &Key) -> Option<CodecString> {
        match self.0.lock().get(key) {
            Some(Entry::Known(string)) => Some(*string),
            _ => None,
        }
    }

    /// Whether the caller is to probe `key`: not one known, being probed,
    /// or failed in the last minute. It then owes [`Self::found`] or
    /// [`Self::failed`].
    pub(crate) fn claim(&self, key: &Key, now: Instant) -> bool {
        let mut entries = self.0.lock();
        match entries.get(key) {
            Some(Entry::Known(_) | Entry::Probing) => false,
            Some(Entry::Failed(at)) if now.duration_since(*at) < FAILED_FOR => false,
            _ => {
                entries.insert(key.clone(), Entry::Probing);
                true
            }
        }
    }

    /// What was read for `key`, by a probe or from a run's init, which
    /// replaces anything before it. The string it replaced, if another.
    pub(crate) fn found(&self, key: Key, string: CodecString) -> Option<CodecString> {
        match self.0.lock().insert(key, Entry::Known(string)) {
            Some(Entry::Known(old)) if old != string => Some(old),
            _ => None,
        }
    }

    pub(crate) fn failed(&self, key: Key, now: Instant) {
        self.0.lock().insert(key, Entry::Failed(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key {
            card: "vaapi:0000:03:00.0".into(),
            encoder: "hevc_vaapi".into(),
            size: (2592, 1080),
            frame_rate: 24,
            ten_bits: false,
            max_bitrate: Some(8_000_000),
        }
    }

    fn string(s: &str) -> CodecString {
        s.parse().unwrap()
    }

    #[test]
    fn a_key_is_probed_once() {
        let (strings, now) = (CodecStrings::default(), Instant::now());
        assert!(strings.claim(&key(), now));
        assert!(!strings.claim(&key(), now), "being probed");
        strings.found(key(), string("hvc1.1.6.L150.B0"));
        assert!(!strings.claim(&key(), now), "known");
        assert_eq!(strings.known(&key()), Some(string("hvc1.1.6.L150.B0")));
    }

    #[test]
    fn a_failed_probe_waits_its_minute() {
        let (strings, now) = (CodecStrings::default(), Instant::now());
        assert!(strings.claim(&key(), now));
        strings.failed(key(), now);
        assert_eq!(strings.known(&key()), None);
        assert!(!strings.claim(&key(), now + Duration::from_secs(59)));
        assert!(strings.claim(&key(), now + Duration::from_secs(60)));
    }

    #[test]
    fn a_run_s_init_replaces_the_probe_s_string() {
        let strings = CodecStrings::default();
        strings.found(key(), string("hvc1.1.6.L150.B0"));
        let old = strings.found(key(), string("hvc1.1.6.L153.B0"));
        assert_eq!(old, Some(string("hvc1.1.6.L150.B0")));
        assert_eq!(strings.known(&key()), Some(string("hvc1.1.6.L153.B0")));
    }

    #[test]
    fn the_same_string_replaces_nothing() {
        let strings = CodecStrings::default();
        strings.found(key(), string("hvc1.1.6.L150.B0"));
        assert_eq!(strings.found(key(), string("hvc1.1.6.L150.B0")), None);
    }

    #[test]
    fn another_rate_is_another_key() {
        let (strings, now) = (CodecStrings::default(), Instant::now());
        strings.found(key(), string("hvc1.1.6.L120.B0"));
        let faster = Key {
            max_bitrate: Some(20_000_000),
            ..key()
        };
        assert_eq!(strings.known(&faster), None);
        assert!(strings.claim(&faster, now));
    }
}
