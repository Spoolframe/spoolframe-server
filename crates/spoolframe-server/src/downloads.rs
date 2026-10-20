//! Downloads for offline viewing (PX-38). This part is what the rest
//! builds on: the copy pool, which downloads use even when optimized
//! versions are off (the user's decision, 2026-10-20), and the device
//! classes a download is written for.

use std::collections::BTreeSet;
use std::sync::Arc;

use spoolframe_types::{AudioCodec, Platform};

use crate::optimized::Pool;

/// What downloads work with. The pool is always here: `optimized.enabled
/// = false` only stops the copies the server makes on its own.
#[derive(Debug, Clone)]
pub struct Downloads {
    pool: Arc<Pool>,
}

impl Downloads {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// The copy pool a download's conversion is made in.
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

/// The kind of device a download is written for: its container, and what
/// it plays as it is. A platform family, from `Hello.platform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// iPhone, iPad and Mac: MP4, text subtitles as `mov_text`.
    Apple,
    /// The Linux app and the SDK's command-line clients: Matroska.
    Linux,
}

impl DeviceClass {
    /// The class of a client on `platform`; `None` for one that has no
    /// downloads (tvOS, the web UI, a platform this build does not know).
    pub fn of(platform: Platform) -> Option<Self> {
        match platform {
            Platform::Ios | Platform::Macos => Some(Self::Apple),
            Platform::Linux | Platform::Cli => Some(Self::Linux),
            Platform::Tvos | Platform::Web | Platform::Unknown => None,
        }
    }

    /// The name in a copy's target, in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Linux => "linux",
        }
    }

    /// The class `s` names, as [`DeviceClass::as_str`] spells it.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apple" => Some(Self::Apple),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }

    /// The audio codecs a copy for this class keeps as they are; any
    /// other is converted. Apple's players take no DTS or TrueHD; the
    /// Linux app plays every codec the server names.
    pub fn keep_audio(self) -> BTreeSet<AudioCodec> {
        let apple = [
            AudioCodec::Aac,
            AudioCodec::Ac3,
            AudioCodec::Eac3,
            AudioCodec::Alac,
            AudioCodec::Flac,
            AudioCodec::Mp3,
        ];
        match self {
            Self::Apple => apple.into(),
            Self::Linux => apple
                .into_iter()
                .chain([
                    AudioCodec::Dts,
                    AudioCodec::TrueHd,
                    AudioCodec::Opus,
                    AudioCodec::Vorbis,
                    AudioCodec::Pcm,
                ])
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_iphone_and_a_mac_download_for_apple() {
        assert_eq!(
            [Platform::Ios, Platform::Macos].map(DeviceClass::of),
            [Some(DeviceClass::Apple); 2]
        );
    }

    #[test]
    fn the_linux_app_and_the_cli_download_for_linux() {
        assert_eq!(
            [Platform::Linux, Platform::Cli].map(DeviceClass::of),
            [Some(DeviceClass::Linux); 2]
        );
    }

    #[test]
    fn an_apple_tv_and_the_web_have_no_downloads() {
        assert_eq!(
            [Platform::Tvos, Platform::Web, Platform::Unknown].map(DeviceClass::of),
            [None; 3]
        );
    }

    #[test]
    fn a_class_s_name_reads_back_as_the_class() {
        for class in [DeviceClass::Apple, DeviceClass::Linux] {
            assert_eq!(DeviceClass::parse(class.as_str()), Some(class));
        }
    }
}
