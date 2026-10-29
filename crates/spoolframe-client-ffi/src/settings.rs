//! What an app's settings and controls read from the SDK: the quality
//! picker and its kept form, whether this device is at home, the screen's
//! HDR, the device's probe, the subtitle offset's steps and the longest
//! name a user types.

use std::net::IpAddr;
use std::sync::Arc;

use spoolframe_client_sdk::types::{
    self, AudioCodec, Container, Delivery, Platform, SubtitleFormat, SubtitleTiming, VideoCodec,
};
use spoolframe_client_sdk::{self as sdk, PlaybackQuality};

use crate::error::{SpoolError, invalid};
use crate::session::SpoolSession;
use crate::values::{DeviceCapabilities, ScreenCapabilities};

/// The quality picker's choices, in its order: Auto, 20, 8, 4, 2 and 1
/// Mbit/s, Original.
#[uniffi::export]
pub fn quality_choices() -> Vec<PlaybackQuality> {
    PlaybackQuality::choices().to_vec()
}

/// `quality`'s place in [`quality_choices`]; a bitrate not offered is
/// Auto's, 0.
#[uniffi::export]
pub fn quality_choice_index(quality: PlaybackQuality) -> u32 {
    quality.choice_index() as u32
}

/// `quality` as the app's settings keep it: `auto`, `original` or bits
/// per second.
#[uniffi::export]
pub fn quality_setting(quality: PlaybackQuality) -> String {
    quality.to_string()
}

/// A kept quality read back; also `Auto`, `Original` and `{"Bitrate": n}`
/// as apps kept them before. None when it is no quality.
#[uniffi::export]
pub fn quality_from_setting(kept: String) -> Option<PlaybackQuality> {
    kept.parse()
        .ok()
        .or_else(|| serde_json::from_str(&kept).ok())
}

/// The quality a playback starts with when the viewer picked none: the
/// device's `limit` for this network, else Original on tvOS or at home,
/// else Auto.
#[uniffi::export]
pub fn default_quality_for(
    limit: Option<u64>,
    platform: Platform,
    at_home: bool,
) -> PlaybackQuality {
    PlaybackQuality::default_for(limit, platform, at_home)
}

fn ips(text: &[String]) -> Result<Vec<IpAddr>, SpoolError> {
    text.iter()
        .map(|a| a.parse::<IpAddr>().map_err(|e| invalid(a, e)))
        .collect()
}

#[uniffi::export]
impl SpoolSession {
    /// Whether this device is on the server's home network now:
    /// `local_addrs` are this device's own addresses, as the app reads
    /// them with its platform's API.
    pub fn at_home(&self, local_addrs: Vec<String>) -> Result<bool, SpoolError> {
        Ok(self.session.at_home(&ips(&local_addrs)?))
    }

    /// The quality a playback starts with when the viewer picked none,
    /// for this session's server: `limit` is the device's own for this
    /// network, `local_addrs` this device's addresses.
    pub fn default_quality(
        &self,
        limit: Option<u64>,
        local_addrs: Vec<String>,
    ) -> Result<PlaybackQuality, SpoolError> {
        Ok(self.session.default_quality(limit, &ips(&local_addrs)?))
    }
}

/// The screen as it is now: with `show` (the device shows HDR and "Show
/// HDR" is on) HDR10 and HLG, and Dolby Vision only with `dolby_vision`;
/// off, none.
#[uniffi::export]
pub fn screen_for_hdr(show: bool, dolby_vision: bool) -> ScreenCapabilities {
    types::DisplayCapabilities::for_hdr(show, dolby_vision).into()
}

/// The device's player, asked whether it plays MP4 video of an RFC 6381
/// codec string: VideoToolbox, MediaCodecList, GStreamer.
#[uniffi::export(with_foreign)]
pub trait CodecPlayer: Send + Sync {
    fn plays(&self, codec: String) -> bool;
}

/// `Decoders`: what the player decodes by its own fixed lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct PlayerDecoders {
    pub video: Vec<VideoCodec>,
    /// Those of `video` decoded in hardware; `None`: it doesn't say.
    pub hardware_video: Option<Vec<VideoCodec>>,
    pub audio: Vec<AudioCodec>,
    pub containers: Vec<Container>,
    pub subtitles: Vec<SubtitleFormat>,
    /// Preferred first; empty is range reads, then a progressive stream.
    pub delivery: Vec<Delivery>,
    pub max_audio_channels: Option<u32>,
    pub video_codecs: Option<Vec<String>>,
}

impl From<PlayerDecoders> for types::Decoders {
    fn from(d: PlayerDecoders) -> Self {
        Self {
            video: d.video.into_iter().collect(),
            hardware_video: d.hardware_video.map(|h| h.into_iter().collect()),
            audio: d.audio.into_iter().collect(),
            containers: d.containers.into_iter().collect(),
            subtitles: d.subtitles.into_iter().collect(),
            delivery: d.delivery,
            max_audio_channels: d.max_audio_channels,
            video_codecs: d.video_codecs.map(|v| v.into_iter().collect()),
        }
    }
}

/// What this device plays, probed as the iOS app does on every platform:
/// each candidate codec string asked of `player`, the `hardware` decoders
/// the platform names, 10-bit where HEVC decodes in hardware, on
/// `display`, over `decoders`' fixed lists.
#[uniffi::export]
pub fn probe_capabilities(
    player: Arc<dyn CodecPlayer>,
    hardware: Vec<VideoCodec>,
    display: ScreenCapabilities,
    decoders: PlayerDecoders,
) -> DeviceCapabilities {
    let plays = |codec: &str| player.plays(codec.to_owned());
    types::ClientCapabilities::probe(&plays, &hardware, &display.into(), decoders.into()).into()
}

/// The offset `timing` moves to after `steps` presses of the ± control
/// (0.1 s each; negative: earlier), held within ten minutes either way.
#[uniffi::export]
pub fn subtitle_offset_stepped(timing: SubtitleTiming, steps: i64) -> i64 {
    timing.offset_stepped(steps)
}

/// The most characters a name typed for a device has: a passkey's, an
/// identity's, a pairing's label.
#[uniffi::export]
pub fn max_name_chars() -> u32 {
    sdk::MAX_NAME_CHARS as u32
}

/// `name` as a server takes it: trimmed and cut to [`max_name_chars`];
/// none when nothing is left.
#[uniffi::export]
pub fn fit_name(name: String) -> Option<String> {
    sdk::fit_name(&name)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
