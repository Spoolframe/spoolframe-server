//! The simple enums' names on the wire, both ways, and every value an app
//! offers: for an app that keeps a choice in its own settings, or reads
//! one from a link, with the same names the server and the SDK use. No
//! English labels: the apps word them.
//!
//! Each enum's list names every value but `Unknown` (a name this build
//! does not know, never one to offer); a match over the list and
//! `Unknown` stops it compiling when a value is added and not listed.

use serde::de::value::StrDeserializer;
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    AudioCodec, AudioRole, BitmapPreference, Container, Delivery, DynamicRange, FilterPart,
    ForcedPreference, HdrFormat, ItemFilter, ItemSort, Resolution, SdhPreference, SearchKind,
    SearchScope, SortKey, SubtitleFormat, SubtitleMode, VideoCodec, WatchedState,
};

/// `value`'s name on the wire: its serde name.
fn name_of(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        other => unreachable!("a simple enum serializes as its name, not {other:?}"),
    }
}

/// The value of `all` serde reads from `name`; `None` for a name it reads
/// as none of them. A type's `serde(other)` value reads from any name,
/// so only that one is checked against its own name, which allocates;
/// every other name is read without.
fn named<T: Serialize + DeserializeOwned + PartialEq + Copy>(name: &str, all: &[T]) -> Option<T> {
    let read = |name: &str| T::deserialize(StrDeserializer::<serde::de::value::Error>::new(name));
    let value = read(name).ok()?;
    // What a name nothing has reads as: the `serde(other)` value.
    let other = read("\0").ok();
    if other == Some(value) && name_of(&value) != name {
        return None;
    }
    all.contains(&value).then_some(value)
}

/// How a type's wire name is had: its own `as_str`, or its serde name.
macro_rules! wire_name_by {
    (as_str, $value:expr) => {
        $value.as_str().to_owned()
    };
    (serde, $value:expr) => {
        name_of($value)
    };
    (as_str, $name:expr, $all:expr) => {
        $all.iter().copied().find(|v| v.as_str() == $name)
    };
    (serde, $name:expr, $all:expr) => {
        named($name, $all)
    };
}

/// `value` as a settings string: its JSON.
fn settings_of(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a sort or filter serializes")
}

/// The value a settings string holds; `None` for one that isn't one.
fn from_settings<T: DeserializeOwned>(settings: &str) -> Option<T> {
    serde_json::from_str(settings).ok()
}

macro_rules! wire_names {
    ($(
        $ty:ident ($by:ident), $from:ident, $all:ident, [$($v:ident),* $(,)?] $(, $unknown:ident)?;
    )*) => {$(
        impl $ty {
            /// Every value an app offers, in the order a client lists
            /// them: all but `Unknown`.
            pub const ALL: &'static [$ty] = &[$($ty::$v),*];

            /// The value named `name` on the wire; `None` for a name
            /// this build doesn't know.
            pub fn from_wire_name(name: &str) -> Option<Self> {
                wire_name_by!($by, name, Self::ALL)
            }
        }

        #[cfg_attr(feature = "uniffi", uniffi::export)]
        impl $ty {
            /// Its name on the wire, as the server and the SDK say it.
            pub fn wire_name(&self) -> String {
                wire_name_by!($by, self)
            }
        }

        /// The value named `name` on the wire; `None` for a name this
        /// build doesn't know.
        #[cfg_attr(feature = "uniffi", uniffi::export)]
        pub fn $from(name: String) -> Option<$ty> {
            $ty::from_wire_name(&name)
        }

        /// Every value an app offers: all but `Unknown`, in order.
        #[cfg_attr(feature = "uniffi", uniffi::export)]
        pub fn $all() -> Vec<$ty> {
            $ty::ALL.to_vec()
        }

        const _: fn($ty) = |value| match value {
            $($ty::$v)|* $(| $ty::$unknown)? => {}
        };
    )*};
}

wire_names! {
    SortKey (as_str), sort_key_from_wire_name, all_sort_keys,
        [Title, Released, Added, LastPlayed, Rating, AudienceRating, Duration, Size], Unknown;
    WatchedState (serde), watched_state_from_wire_name, all_watched_states,
        [Unwatched, InProgress, Watched], Unknown;
    Resolution (as_str), resolution_from_wire_name, all_resolutions, [Uhd, FullHd, Hd, Sd], Unknown;
    DynamicRange (serde), dynamic_range_from_wire_name, all_dynamic_ranges,
        [Sdr, Hdr, DolbyVision], Unknown;
    FilterPart (as_str), filter_part_from_wire_name, all_filter_parts,
        [Watched, Genres, Years, ContentRatings, Resolutions, DynamicRanges], Unknown;
    SearchScope (serde), search_scope_from_wire_name, all_search_scopes,
        [All, Movies, Shows, Episodes, Music];
    SearchKind (serde), search_kind_from_wire_name, all_search_kinds,
        [Shows, Movies, People, Artists, Albums, Tracks, Episodes];
    VideoCodec (as_str), video_codec_from_wire_name, all_video_codecs,
        [H264, Hevc, Av1, Vp9, Vp8, Mpeg2, Mpeg4, Vc1], Unknown;
    AudioCodec (as_str), audio_codec_from_wire_name, all_audio_codecs,
        [Aac, Ac3, Eac3, Dts, TrueHd, Flac, Alac, Opus, Vorbis, Mp3, Pcm], Unknown;
    Container (as_str), container_from_wire_name, all_containers,
        [Matroska, Mp4, MpegTs, Avi, Flac, Mp3, Ogg], Unknown;
    SubtitleFormat (as_str), subtitle_format_from_wire_name, all_subtitle_formats,
        [Srt, Ass, WebVtt, MovText, Pgs, VobSub, DvbSub], Unknown;
    Delivery (serde), delivery_from_wire_name, all_deliveries, [Range, Hls, Progressive], Unknown;
    HdrFormat (serde), hdr_format_from_wire_name, all_hdr_formats, [Hdr10, Hlg, DolbyVision], Unknown;
    SubtitleMode (serde), subtitle_mode_from_wire_name, all_subtitle_modes,
        [Always, ForeignOnly, Off, ForcedOnly];
    SdhPreference (serde), sdh_preference_from_wire_name, all_sdh_preferences,
        [Prefer, Avoid, Only, Accept];
    ForcedPreference (serde), forced_preference_from_wire_name, all_forced_preferences,
        [Prefer, Avoid, Only, Accept];
    AudioRole (serde), audio_role_from_wire_name, all_audio_roles,
        [Main, AudioDescription, Commentary, Other];
    BitmapPreference (serde), bitmap_preference_from_wire_name, all_bitmap_preferences,
        [Pictures, Text];
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl ItemSort {
    /// It as a string to keep in an app's settings:
    /// [`item_sort_from_settings`] reads it back.
    pub fn settings_string(&self) -> String {
        settings_of(self)
    }
}

/// The sort `settings` holds ([`ItemSort::settings_string`]); `None` for
/// a string that isn't one.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn item_sort_from_settings(settings: String) -> Option<ItemSort> {
    from_settings(&settings)
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl ItemFilter {
    /// It as a string to keep in an app's settings:
    /// [`item_filter_from_settings`] reads it back.
    pub fn settings_string(&self) -> String {
        settings_of(self)
    }
}

/// The filter `settings` holds ([`ItemFilter::settings_string`]); `None`
/// for a string that isn't one.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn item_filter_from_settings(settings: String) -> Option<ItemFilter> {
    from_settings(&settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every value of `T` named on the wire reads back as itself, no two
    /// share a name, and `Unknown`'s name reads as nothing.
    fn round_trips<T: Copy + PartialEq + std::fmt::Debug + Serialize>(
        all: &[T],
        from: fn(&str) -> Option<T>,
    ) {
        let names: Vec<String> = all.iter().map(name_of).collect();
        let back: Vec<Option<T>> = names.iter().map(|n| from(n)).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            (back, unique.len(), from("unknown"), from("no-such-name")),
            (
                all.iter().copied().map(Some).collect(),
                names.len(),
                None,
                None
            ),
            "{names:?}"
        );
    }

    #[test]
    fn every_value_offered_reads_back_from_its_wire_name() {
        round_trips(SortKey::ALL, SortKey::from_wire_name);
        round_trips(WatchedState::ALL, WatchedState::from_wire_name);
        round_trips(Resolution::ALL, Resolution::from_wire_name);
        round_trips(DynamicRange::ALL, DynamicRange::from_wire_name);
        round_trips(FilterPart::ALL, FilterPart::from_wire_name);
        round_trips(SearchScope::ALL, SearchScope::from_wire_name);
        round_trips(SearchKind::ALL, SearchKind::from_wire_name);
        round_trips(VideoCodec::ALL, VideoCodec::from_wire_name);
        round_trips(AudioCodec::ALL, AudioCodec::from_wire_name);
        round_trips(Container::ALL, Container::from_wire_name);
        round_trips(SubtitleFormat::ALL, SubtitleFormat::from_wire_name);
        round_trips(Delivery::ALL, Delivery::from_wire_name);
        round_trips(HdrFormat::ALL, HdrFormat::from_wire_name);
        round_trips(SubtitleMode::ALL, SubtitleMode::from_wire_name);
        round_trips(SdhPreference::ALL, SdhPreference::from_wire_name);
        round_trips(ForcedPreference::ALL, ForcedPreference::from_wire_name);
        round_trips(AudioRole::ALL, AudioRole::from_wire_name);
        round_trips(BitmapPreference::ALL, BitmapPreference::from_wire_name);
    }

    /// The vocabularies' own names are the wire names.
    #[test]
    fn a_codec_s_wire_name_is_its_vocabulary_name() {
        assert_eq!(
            (
                VideoCodec::Hevc.wire_name(),
                AudioCodec::TrueHd.wire_name(),
                Resolution::Uhd.wire_name(),
                Container::MpegTs.wire_name(),
            ),
            (
                VideoCodec::Hevc.as_str().to_owned(),
                AudioCodec::TrueHd.as_str().to_owned(),
                "4k".to_owned(),
                Container::MpegTs.as_str().to_owned(),
            )
        );
    }

    #[test]
    fn a_sort_and_a_filter_read_back_from_their_settings_strings() {
        let sort = ItemSort {
            key: SortKey::LastPlayed,
            descending: true,
        };
        let filter = ItemFilter {
            watched: vec![WatchedState::InProgress],
            year_from: Some(1990),
            resolutions: vec![Resolution::Uhd],
            ..ItemFilter::default()
        };
        assert_eq!(
            (
                item_sort_from_settings(sort.settings_string()),
                item_filter_from_settings(filter.settings_string()),
                item_sort_from_settings("not json".into()),
            ),
            (Some(sort), Some(filter), None)
        );
    }
}
