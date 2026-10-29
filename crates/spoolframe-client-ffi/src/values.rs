//! The few SDK values UniFFI can't take as they are, as records of their
//! own: sets as lists (a client's capabilities), and generic ones once
//! per type they're used with (search results, a filter's counted
//! values). Everything else crosses as the SDK's own type.

use std::collections::BTreeSet;

use spoolframe_client_sdk::types::{
    self, AudioCodec, Container, Delivery, DynamicRange, HdrFormat, Item, Person, Resolution,
    SearchKind, SortKey, SubtitleFormat, VideoCodec,
};
use spoolframe_client_sdk::{self as sdk, SearchCounts, SearchHit};

/// What this device plays: `ClientCapabilities`, each set a list.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct DeviceCapabilities {
    pub video: Vec<VideoCodec>,
    pub audio: Vec<AudioCodec>,
    pub containers: Vec<Container>,
    pub subtitles: Vec<SubtitleFormat>,
    /// How it takes media in, preferred first. Empty: range reads only.
    pub delivery: Vec<Delivery>,
    pub max_video: Option<DeviceVideoLimits>,
    pub max_audio_channels: Option<u32>,
    /// Bits per second, from the app's settings. `None`: no limit.
    pub max_bitrate: Option<u64>,
    /// The video codecs it decodes in hardware; `None`: it doesn't say.
    pub hardware_video: Option<Vec<VideoCodec>>,
    /// The RFC 6381 codec strings it plays (`hvc1.2.4.L153.B0`); `None`:
    /// it doesn't say.
    pub video_codecs: Option<Vec<String>>,
}

/// `VideoLimits`: the most video it shows. Each `None` is no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct DeviceVideoLimits {
    pub height: Option<u32>,
    pub frame_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    /// The HDR formats it shows as HDR; `None`: all of them.
    pub hdr: Option<Vec<HdrFormat>>,
}

/// `DisplayCapabilities`: what the screen shows now, as a playback
/// starts, over what the device said at start.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct ScreenCapabilities {
    pub bit_depth: Option<u32>,
    pub hdr: Option<Vec<HdrFormat>>,
    /// `None` keeps the device's.
    pub video_codecs: Option<Vec<String>>,
}

fn set<T: Ord>(list: Vec<T>) -> BTreeSet<T> {
    list.into_iter().collect()
}

impl From<DeviceVideoLimits> for types::VideoLimits {
    fn from(v: DeviceVideoLimits) -> Self {
        Self {
            height: v.height,
            frame_rate: v.frame_rate,
            bit_depth: v.bit_depth,
            hdr: v.hdr.map(set),
        }
    }
}

impl From<DeviceCapabilities> for types::ClientCapabilities {
    fn from(c: DeviceCapabilities) -> Self {
        Self {
            video: set(c.video),
            audio: set(c.audio),
            containers: set(c.containers),
            subtitles: set(c.subtitles),
            delivery: c.delivery,
            max_video: c.max_video.map(Into::into),
            max_audio_channels: c.max_audio_channels,
            max_bitrate: c.max_bitrate,
            hardware_video: c.hardware_video.map(set),
            video_codecs: c.video_codecs.map(set),
        }
    }
}

impl From<ScreenCapabilities> for types::DisplayCapabilities {
    fn from(d: ScreenCapabilities) -> Self {
        Self {
            bit_depth: d.bit_depth,
            hdr: d.hdr.map(set),
            video_codecs: d.video_codecs.map(set),
        }
    }
}

fn list<T>(set: BTreeSet<T>) -> Vec<T> {
    set.into_iter().collect()
}

impl From<types::VideoLimits> for DeviceVideoLimits {
    fn from(v: types::VideoLimits) -> Self {
        Self {
            height: v.height,
            frame_rate: v.frame_rate,
            bit_depth: v.bit_depth,
            hdr: v.hdr.map(list),
        }
    }
}

impl From<types::ClientCapabilities> for DeviceCapabilities {
    fn from(c: types::ClientCapabilities) -> Self {
        Self {
            video: list(c.video),
            audio: list(c.audio),
            containers: list(c.containers),
            subtitles: list(c.subtitles),
            delivery: c.delivery,
            max_video: c.max_video.map(Into::into),
            max_audio_channels: c.max_audio_channels,
            max_bitrate: c.max_bitrate,
            hardware_video: c.hardware_video.map(list),
            video_codecs: c.video_codecs.map(list),
        }
    }
}

impl From<types::DisplayCapabilities> for ScreenCapabilities {
    fn from(d: types::DisplayCapabilities) -> Self {
        Self {
            bit_depth: d.bit_depth,
            hdr: d.hdr.map(list),
            video_codecs: d.video_codecs.map(list),
        }
    }
}

/// A filter value and how many items have it, when the server counted.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CountedText {
    pub value: String,
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CountedYear {
    pub value: u16,
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CountedResolution {
    pub value: Resolution,
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CountedDynamicRange {
    pub value: DynamicRange,
    pub count: Option<u32>,
}

/// `ItemFilterValues`: what a library sorts and filters by, and the
/// values to offer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FilterValues {
    pub genres: Vec<CountedText>,
    pub years: Vec<CountedYear>,
    pub content_ratings: Vec<CountedText>,
    pub resolutions: Vec<CountedResolution>,
    pub dynamic_ranges: Vec<CountedDynamicRange>,
    pub sorts: Vec<SortKey>,
    pub filters: Vec<types::FilterPart>,
    /// The decades the years fall in, newest first: 1990 for 1990 to
    /// 1999.
    pub decades: Vec<u16>,
}

macro_rules! counted {
    ($list:expr, $record:ident) => {
        $list
            .into_iter()
            .map(|c| $record {
                value: c.value,
                count: c.count,
            })
            .collect()
    };
}

impl From<types::ItemFilterValues> for FilterValues {
    fn from(v: types::ItemFilterValues) -> Self {
        let decades = v.decades();
        Self {
            genres: counted!(v.genres, CountedText),
            years: counted!(v.years, CountedYear),
            content_ratings: counted!(v.content_ratings, CountedText),
            resolutions: counted!(v.resolutions, CountedResolution),
            dynamic_ranges: counted!(v.dynamic_ranges, CountedDynamicRange),
            sorts: v.sorts,
            filters: v.filters,
            decades,
        }
    }
}

/// A search's top match: an item, or a person.
// No box: UniFFI passes enum fields by value; one per search.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ItemSearchTop {
    Item { item: Item },
    Person { person: Person },
}

/// One kind's group of a search.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemSearchGroup {
    pub kind: SearchKind,
    pub items: Vec<Item>,
    pub people: Vec<Person>,
    /// There are more than shown: "See all".
    pub more: bool,
    pub total: Option<u32>,
    pub hidden: u32,
}

/// `SearchResults`: the top match, a group per kind, the counts per
/// scope, and the year taken off the end of the query.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemSearchResults {
    pub top: Option<ItemSearchTop>,
    pub groups: Vec<ItemSearchGroup>,
    pub counts: SearchCounts,
    pub year: Option<u16>,
    pub without_year: Vec<Item>,
}

/// As [`ItemSearchTop`], over this server and its paired ones.
// No box: UniFFI passes enum fields by value; one per search.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum HitSearchTop {
    Hit { hit: SearchHit },
    Person { person: Person },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HitSearchGroup {
    pub kind: SearchKind,
    pub hits: Vec<SearchHit>,
    pub people: Vec<Person>,
    pub more: bool,
    pub total: Option<u32>,
    pub hidden: u32,
}

/// `SearchResults<SearchHit>`: each item with where it's from, its other
/// copies and the copy that plays.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HitSearchResults {
    pub top: Option<HitSearchTop>,
    pub groups: Vec<HitSearchGroup>,
    pub counts: SearchCounts,
    pub year: Option<u16>,
    pub without_year: Vec<SearchHit>,
}

impl From<sdk::SearchResults> for ItemSearchResults {
    fn from(r: sdk::SearchResults) -> Self {
        Self {
            top: r.top.map(|t| match t {
                sdk::TopResult::Item(item) => ItemSearchTop::Item { item },
                sdk::TopResult::Person(person) => ItemSearchTop::Person { person },
            }),
            groups: r
                .groups
                .into_iter()
                .map(|g| ItemSearchGroup {
                    kind: g.kind,
                    items: g.items,
                    people: g.people,
                    more: g.more,
                    total: g.total,
                    hidden: g.hidden,
                })
                .collect(),
            counts: r.counts,
            year: r.year,
            without_year: r.without_year,
        }
    }
}

impl From<sdk::SearchResults<SearchHit>> for HitSearchResults {
    fn from(r: sdk::SearchResults<SearchHit>) -> Self {
        Self {
            top: r.top.map(|t| match t {
                sdk::TopResult::Item(hit) => HitSearchTop::Hit { hit },
                sdk::TopResult::Person(person) => HitSearchTop::Person { person },
            }),
            groups: r
                .groups
                .into_iter()
                .map(|g| HitSearchGroup {
                    kind: g.kind,
                    hits: g.items,
                    people: g.people,
                    more: g.more,
                    total: g.total,
                    hidden: g.hidden,
                })
                .collect(),
            counts: r.counts,
            year: r.year,
            without_year: r.without_year,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_cross_as_lists_and_reach_the_sdk_as_sets() {
        let caps: types::ClientCapabilities = DeviceCapabilities {
            video: vec![VideoCodec::Hevc, VideoCodec::H264, VideoCodec::H264],
            hardware_video: Some(vec![VideoCodec::Hevc]),
            max_video: Some(DeviceVideoLimits {
                height: Some(2160),
                hdr: Some(vec![HdrFormat::DolbyVision]),
                ..DeviceVideoLimits::default()
            }),
            ..DeviceCapabilities::default()
        }
        .into();
        assert_eq!(caps.video, [VideoCodec::H264, VideoCodec::Hevc].into());
        assert_eq!(caps.hardware_video, Some([VideoCodec::Hevc].into()));
        assert_eq!(
            caps.max_video.and_then(|v| v.hdr),
            Some([HdrFormat::DolbyVision].into())
        );
    }

    #[test]
    fn a_person_on_top_stays_a_person() {
        let person: Person = serde_json::from_value(serde_json::json!({
            "id": "plex:1", "name": "Amy Adams", "role": "actor", "count": 3,
        }))
        .unwrap();
        let results = sdk::SearchResults {
            top: Some(sdk::TopResult::Person(person.clone())),
            groups: Vec::new(),
            counts: SearchCounts::default(),
            year: Some(2016),
            without_year: Vec::new(),
        };
        let crossed = ItemSearchResults::from(results);
        assert_eq!(crossed.top, Some(ItemSearchTop::Person { person }));
        assert_eq!(crossed.year, Some(2016));
    }
}
