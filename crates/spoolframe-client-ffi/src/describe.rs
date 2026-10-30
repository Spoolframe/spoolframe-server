//! What the apps say about media and connections, as the SDK's
//! `describe` module and the types' caption parts give it: parts each
//! platform words and localises itself, as the SDK's own types.

use std::time::{Duration, SystemTime};

use spoolframe_client_sdk::describe::{
    self, Ago, AudioFormat, HoursMinutes, Language, PathChip, PictureClass, Played, TrackLabel,
    TrackMenu, UpdateNeeded, YearRange,
};
use spoolframe_client_sdk::types::{
    AudioCodec, AudioTrack, CalendarDate, Caption, Container, Item, ItemFilter, MediaTracks,
    PlaybackState, Show, SubtitleTrack, VideoTrack, YearsAired,
};
use spoolframe_client_sdk::{ConnectionPath, PlaybackPlan, Reachability, Selection, ServerEntry};

use crate::error::SpoolError;

/// The language `code` names: ISO 639-2 (`eng`) or two letters with or
/// without a region (`pt-BR`).
#[uniffi::export]
pub fn language(code: String) -> Language {
    describe::language(&code)
}

#[uniffi::export]
pub fn audio_track_label(track: AudioTrack) -> TrackLabel {
    describe::audio_track(&track)
}

#[uniffi::export]
pub fn subtitle_track_label(track: SubtitleTrack) -> TrackLabel {
    describe::subtitle_track(&track)
}

/// The menus for `tracks`, `selection` ticked; with none, the file's
/// default audio and no subtitles.
#[uniffi::export]
pub fn track_menu(tracks: MediaTracks, selection: Option<Selection>) -> TrackMenu {
    describe::track_menu(&tracks, selection.as_ref())
}

/// "HEVC 2160p HDR10"; `None` with nothing known.
#[uniffi::export]
pub fn picture_class(
    container: Option<Container>,
    video: Option<VideoTrack>,
) -> Option<PictureClass> {
    describe::picture(container, video.as_ref())
}

#[uniffi::export]
pub fn audio_format(
    codec: Option<AudioCodec>,
    bit_depth: Option<u32>,
    sample_rate: Option<u32>,
) -> Option<AudioFormat> {
    describe::audio_format(codec, bit_depth, sample_rate)
}

/// How long is left, rounded up to the minute.
#[uniffi::export]
pub fn remaining(length: Duration, position: Duration) -> HoursMinutes {
    describe::remaining(length, position)
}

/// How long before `now` `at` was.
#[uniffi::export]
pub fn ago(at: SystemTime, now: SystemTime) -> Ago {
    describe::ago(at, now)
}

/// "Played 3 times · 2 days ago"; `None` for never played.
#[uniffi::export]
pub fn played(state: PlaybackState, now: SystemTime) -> Option<Played> {
    describe::played(&state, now)
}

/// The chip beside a server's name; `None` with no path yet.
#[uniffi::export]
pub fn path_chip(path: ConnectionPath) -> Option<PathChip> {
    describe::path_chip(path)
}

/// The years a filter asks for.
#[uniffi::export]
pub fn filter_year_range(filter: ItemFilter) -> YearRange {
    describe::year_range(&filter)
}

/// The years `show` ran, on `today`.
#[uniffi::export]
pub fn show_years(show: Show, today: CalendarDate) -> Option<YearsAired> {
    show.year_range(today)
}

/// "2019", "2019–", "2019–2023".
#[uniffi::export]
pub fn years_aired_text(years: YearsAired) -> String {
    years.to_string()
}

/// What a card says under its title on `today`.
#[uniffi::export]
pub fn caption_parts(item: Item, today: CalendarDate) -> Option<Caption> {
    item.caption_parts(today)
}

/// The facts of a reach report, a line each.
#[uniffi::export]
pub fn reach_facts(reachability: Reachability) -> String {
    reachability.to_string()
}

/// The whole report for Copy report: a header with when it failed (`now`
/// when it has not), `build` and `device`, then the facts.
#[uniffi::export]
pub fn reach_report(
    reachability: Reachability,
    now: SystemTime,
    build: String,
    device: String,
) -> String {
    reachability.report(now, &build, &device)
}

/// The address to show for a server: a home-network IPv4 where there is
/// one, without its port.
#[uniffi::export]
pub fn server_display_host(entry: ServerEntry) -> Option<String> {
    entry.display_host()
}

/// The lines a conversion encodes to, for "Auto (720p)"; `None` where
/// the picture is the file's own.
#[uniffi::export]
pub fn plan_encoded_height(plan: PlaybackPlan) -> Option<u32> {
    plan.encoded_height()
}

#[uniffi::export]
impl SpoolError {
    /// Which side an `UpdateRequired` asks to update; `None` for any
    /// other error.
    pub fn who_updates(&self) -> Option<UpdateNeeded> {
        self.classified()?.who_updates()
    }
}

#[cfg(test)]
#[path = "describe_tests.rs"]
mod tests;
