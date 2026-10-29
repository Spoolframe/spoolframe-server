//! What the apps say about media and connections, as parts each platform
//! words and localises itself (the Linux app's format.rs, the iOS
//! labels): a track's language, role and layout; a file's picture class
//! ("HEVC 2160p HDR10"); music's lossless format; how long is left and
//! how long ago; which side needs an update; how a server is reached.
//! A card's caption is [`spoolframe_types::Item::caption_parts`].
//! Nothing here is English but [`Language::english`], a fallback.

use std::time::Duration;

use spoolframe_protocol::Selection;
use spoolframe_types::{
    AudioCodec, AudioRole, AudioTrack, Container, HdrFormat, ItemFilter, MediaTracks,
    PlaybackState, Resolution, SubtitleRole, SubtitleTrack, VideoCodec, VideoTrack,
};

use crate::{ConnectionPath, SdkError};

/// A language a track or a subtitle site names: an ISO 639-2 code
/// (`eng`), or a two-letter one with or without its region (`pt-BR`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Language {
    /// As given.
    pub code: String,
    /// Its ISO 639-1 code, for the platform's own names (`Locale`); `None`
    /// for one this build does not know.
    pub iso639_1: Option<String>,
    /// The region, upper-cased (`BR`), when given.
    pub region: Option<String>,
    /// The English name, where no localised one is to hand.
    pub english: Option<String>,
    /// `und`: the file does not say.
    pub undetermined: bool,
}

/// English names, by ISO 639-1 code: the fallback where the platform
/// has no name of its own. The codes are `spoolframe_types::language`'s.
const ENGLISH: [(&str, &str); 17] = [
    ("en", "English"),
    ("fr", "French"),
    ("de", "German"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("pl", "Polish"),
    ("nl", "Dutch"),
    ("sv", "Swedish"),
    ("no", "Norwegian"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
];

/// [`Language`] of `code`.
pub fn language(code: &str) -> Language {
    use spoolframe_types::language::{bcp47, iso639_2};
    let mut parts = code.split(['-', '_']);
    let primary = parts.next().unwrap_or(code).to_ascii_lowercase();
    let region = parts.next().map(str::to_ascii_uppercase);
    // Known: its two- and three-letter codes differ. Bokmål is
    // Norwegian's here, as a picker shows it.
    let known = bcp47(&primary) != iso639_2(&primary);
    let iso639_1 = known.then(|| match bcp47(&primary).as_str() {
        "nb" => "no".to_owned(),
        two => two.to_owned(),
    });
    let english = iso639_1.as_deref().and_then(|two| {
        ENGLISH
            .iter()
            .find(|(code, _)| *code == two)
            .map(|(_, name)| (*name).to_owned())
    });
    Language {
        code: code.to_owned(),
        iso639_1,
        region,
        english,
        undetermined: primary == "und",
    }
}

/// An audio layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum Channels {
    Mono,
    Stereo,
    Surround51,
    Surround71,
    Other(u32),
}

impl Channels {
    pub fn of(n: u32) -> Self {
        match n {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            6 => Channels::Surround51,
            8 => Channels::Surround71,
            n => Channels::Other(n),
        }
    }
}

/// What a track is for, when it is not the programme's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TrackRole {
    AudioDescription,
    Commentary,
    /// Subtitles for the deaf and hard of hearing.
    Sdh,
    Forced,
}

/// Where a subtitle comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TrackOrigin {
    /// In the file, or the audio.
    InFile,
    /// A file beside it.
    ExternalFile,
    /// Downloaded from a subtitle site, by its name.
    Searched { site: String },
}

/// A track as a picker shows it: "English · Commentary · stereo". An
/// app shows the title where there is one, else the role.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TrackLabel {
    pub language: Option<Language>,
    pub role: Option<TrackRole>,
    /// The file's own title, when it says more than the language and the
    /// role.
    pub title: Option<String>,
    pub channels: Option<Channels>,
    /// A subtitle of only the lines in another language.
    pub forced: bool,
    pub origin: TrackOrigin,
}

/// A title that only repeats the language (its name or code) or the role
/// is left out, as is an empty one.
fn title_beyond(
    title: Option<&String>,
    language: Option<&Language>,
    role: Option<TrackRole>,
) -> Option<String> {
    let named = |role: TrackRole| match role {
        TrackRole::AudioDescription => "audio description",
        TrackRole::Commentary => "commentary",
        TrackRole::Sdh => "sdh",
        TrackRole::Forced => "forced",
    };
    let repeats_language = |t: &str| {
        language.is_some_and(|l| {
            t.eq_ignore_ascii_case(&l.code)
                || l.english
                    .as_deref()
                    .is_some_and(|e| t.eq_ignore_ascii_case(e))
        })
    };
    title
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && !repeats_language(t))
        .filter(|t| role.is_none_or(|r| !t.eq_ignore_ascii_case(named(r))))
        .map(str::to_owned)
}

pub fn audio_track(track: &AudioTrack) -> TrackLabel {
    let role = match track.role {
        AudioRole::AudioDescription => Some(TrackRole::AudioDescription),
        AudioRole::Commentary => Some(TrackRole::Commentary),
        AudioRole::Main | AudioRole::Other => None,
    };
    let language = track.language.as_deref().map(language);
    TrackLabel {
        title: title_beyond(track.title.as_ref(), language.as_ref(), role),
        language,
        role,
        channels: track.channels.map(Channels::of),
        forced: false,
        origin: TrackOrigin::InFile,
    }
}

pub fn subtitle_track(track: &SubtitleTrack) -> TrackLabel {
    let role = match track.role {
        SubtitleRole::Sdh => Some(TrackRole::Sdh),
        SubtitleRole::Forced => Some(TrackRole::Forced),
        SubtitleRole::Commentary => Some(TrackRole::Commentary),
        SubtitleRole::Full => None,
    };
    let origin = match (&track.download, track.external) {
        (Some(d), _) => TrackOrigin::Searched {
            site: spoolframe_types::SubtitleSource::name_of(&d.source).to_owned(),
        },
        (None, true) => TrackOrigin::ExternalFile,
        (None, false) => TrackOrigin::InFile,
    };
    let language = track.language.as_deref().map(language);
    TrackLabel {
        // A downloaded one's title is its site's name, in `origin`.
        title: title_beyond(track.title.as_ref(), language.as_ref(), role)
            .filter(|_| track.download.is_none()),
        language,
        role,
        channels: None,
        forced: track.forced || track.role == SubtitleRole::Forced,
        origin,
    }
}

/// One row of the audio or subtitles menu.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MenuRow {
    /// The track's index, as a [`Selection`] names it.
    pub index: u32,
    pub label: TrackLabel,
    /// For a label an earlier row already has: this one's number among
    /// them, from 2 ("English (2)"), so a system menu (HLS) tells them
    /// apart. `None` for the first.
    pub ordinal: Option<u32>,
    pub selected: bool,
}

/// Numbers each row whose label an earlier one has.
fn number_repeats(rows: &mut [MenuRow]) {
    for i in 0..rows.len() {
        let before = rows[..i]
            .iter()
            .filter(|r| r.label == rows[i].label)
            .count() as u32;
        rows[i].ordinal = (before > 0).then_some(before + 1);
    }
}

/// The audio and subtitles menus of an item's tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TrackMenu {
    pub audio: Vec<MenuRow>,
    pub subtitles: Vec<MenuRow>,
    /// "Off" is ticked: no subtitle chosen.
    pub subtitles_off: bool,
}

/// The menus for `tracks`, `selection` ticked; with none, the file's
/// default audio and no subtitles.
pub fn track_menu(tracks: &MediaTracks, selection: Option<&Selection>) -> TrackMenu {
    let audio_pick = selection.and_then(|s| s.audio).or_else(|| {
        tracks
            .audio
            .iter()
            .position(|a| a.default)
            .map(|i| i as u32)
    });
    let subtitle_pick = selection.and_then(|s| s.subtitle);
    let mut menu = TrackMenu {
        audio: tracks
            .audio
            .iter()
            .enumerate()
            .map(|(i, t)| MenuRow {
                index: i as u32,
                label: audio_track(t),
                ordinal: None,
                selected: audio_pick == Some(i as u32),
            })
            .collect(),
        subtitles: tracks
            .subtitles
            .iter()
            .enumerate()
            .map(|(i, t)| MenuRow {
                index: i as u32,
                label: subtitle_track(t),
                ordinal: None,
                selected: subtitle_pick == Some(i as u32),
            })
            .collect(),
        subtitles_off: subtitle_pick.is_none(),
    };
    number_repeats(&mut menu.audio);
    number_repeats(&mut menu.subtitles);
    menu
}

/// A file's picture, as "MKV · HEVC 2160p HDR10" says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PictureClass {
    pub container: Option<Container>,
    pub codec: Option<VideoCodec>,
    /// By width and height ([`VideoTrack::resolution`]).
    pub resolution: Option<Resolution>,
    pub hdr: Option<HdrFormat>,
}

/// The picture of a file of `container` with `video`; `None` when the
/// server probed nothing.
pub fn picture(container: Option<Container>, video: Option<&VideoTrack>) -> Option<PictureClass> {
    let container = container.filter(|c| *c != Container::Unknown);
    if container.is_none() && video.is_none() {
        return None;
    }
    Some(PictureClass {
        container,
        codec: video.map(|v| v.codec).filter(|c| *c != VideoCodec::Unknown),
        resolution: video.and_then(VideoTrack::resolution),
        hdr: video.and_then(|v| v.hdr),
    })
}

/// Music's sound as an album shows it: "FLAC 24/96" for lossless with
/// its depth and rate, the codec alone otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub lossless: bool,
    /// Said for lossless only, with `sample_rate`.
    pub bit_depth: Option<u32>,
    /// Hz.
    pub sample_rate: Option<u32>,
}

/// Whether `codec` keeps every bit: FLAC, ALAC and PCM.
pub fn lossless(codec: AudioCodec) -> bool {
    matches!(codec, AudioCodec::Flac | AudioCodec::Alac | AudioCodec::Pcm)
}

pub fn audio_format(
    codec: Option<AudioCodec>,
    bit_depth: Option<u32>,
    sample_rate: Option<u32>,
) -> Option<AudioFormat> {
    let codec = codec.filter(|c| *c != AudioCodec::Unknown)?;
    let lossless = lossless(codec);
    let (bit_depth, sample_rate) = bit_depth.zip(sample_rate).filter(|_| lossless).unzip();
    Some(AudioFormat {
        codec,
        lossless,
        bit_depth,
        sample_rate,
    })
}

/// A span in hours and minutes, rounded up: an item is never "0 min"
/// from its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct HoursMinutes {
    pub hours: u64,
    pub minutes: u64,
}

impl HoursMinutes {
    pub fn of(span: Duration) -> Self {
        let minutes = span.as_secs().div_ceil(60);
        Self {
            hours: minutes / 60,
            minutes: minutes % 60,
        }
    }
}

/// How much of an item `length` long is left at `position`.
pub fn remaining(length: Duration, position: Duration) -> HoursMinutes {
    HoursMinutes::of(length.saturating_sub(position))
}

/// How long ago something was, as "just now", "5 minutes ago".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum Ago {
    JustNow,
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

/// How long before `now` `at` was, both Unix seconds.
pub fn ago(at: u64, now: u64) -> Ago {
    let secs = now.saturating_sub(at);
    match secs {
        0..60 => Ago::JustNow,
        60..3600 => Ago::Minutes(secs / 60),
        3600..86_400 => Ago::Hours(secs / 3600),
        _ => Ago::Days(secs / 86_400),
    }
}

/// "Played 3 times · 2 days ago".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Played {
    pub count: u32,
    /// When it was last played, Unix seconds, for a date of the
    /// platform's own; and how long before now that was.
    pub last_at: Option<i64>,
    pub last: Option<Ago>,
}

/// What `state` says of plays at `now`; `None` for never played.
pub fn played(state: &PlaybackState, now: u64) -> Option<Played> {
    (state.play_count > 0).then(|| Played {
        count: state.play_count,
        last_at: state.last_played_at,
        last: state.last_played_at.map(|at| ago(at.max(0) as u64, now)),
    })
}

/// Which side an [`SdkError::UpdateRequired`] asks to update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum UpdateNeeded {
    /// The server speaks a newer revision: update the app.
    App,
    /// The server is older: update the server.
    Server,
    /// The server did not say: either.
    Either,
}

impl SdkError {
    /// Which side an [`SdkError::UpdateRequired`] asks to update; `None`
    /// for any other error.
    pub fn who_updates(&self) -> Option<UpdateNeeded> {
        let SdkError::UpdateRequired { server_protocol } = self else {
            return None;
        };
        let ours = spoolframe_protocol::PROTOCOL_VERSION;
        Some(match server_protocol {
            Some(theirs) if *theirs > ours => UpdateNeeded::App,
            Some(_) => UpdateNeeded::Server,
            None => UpdateNeeded::Either,
        })
    }
}

/// How the device reaches a server, as the chip beside its name says it:
/// "Direct · LAN", "Direct · internet", "Relayed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PathChip {
    Lan,
    Internet,
    Relayed,
}

/// The chip for `path`; `None` while not connected.
pub fn path_chip(path: ConnectionPath) -> Option<PathChip> {
    match path {
        ConnectionPath::Lan { .. } => Some(PathChip::Lan),
        ConnectionPath::Direct { .. } => Some(PathChip::Internet),
        ConnectionPath::Relay => Some(PathChip::Relayed),
        ConnectionPath::Unknown => None,
    }
}

/// A filter's years, as its chip says them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum YearRange {
    /// "1990".
    One(u16),
    /// "1990–1999".
    Between(u16, u16),
    /// "Since 2010".
    Since(u16),
    /// "Until 1980".
    Until(u16),
    /// No year filter.
    Any,
}

pub fn year_range(filter: &ItemFilter) -> YearRange {
    match (filter.year_from, filter.year_to) {
        (Some(from), Some(to)) if from == to => YearRange::One(from),
        (Some(from), Some(to)) => YearRange::Between(from, to),
        (Some(from), None) => YearRange::Since(from),
        (None, Some(to)) => YearRange::Until(to),
        (None, None) => YearRange::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_known_by_either_code_and_keep_their_region() {
        let pt = language("pt-BR");
        assert_eq!(
            (pt.iso639_1.as_deref(), pt.region.as_deref()),
            (Some("pt"), Some("BR"))
        );
        assert_eq!(language("ger").english.as_deref(), Some("German"));
        assert_eq!(language("nb").iso639_1.as_deref(), Some("no"));
        let unknown = language("tlh");
        assert_eq!((unknown.iso639_1, unknown.undetermined), (None, false));
        assert!(language("und").undetermined);
    }

    fn audio(role: AudioRole, title: Option<&str>) -> AudioTrack {
        AudioTrack {
            language: Some("eng".into()),
            title: title.map(Into::into),
            channels: Some(2),
            default: false,
            role,
        }
    }

    #[test]
    fn a_title_that_repeats_the_role_is_left_out() {
        let label = audio_track(&audio(AudioRole::Commentary, Some("Commentary")));
        assert_eq!(label.role, Some(TrackRole::Commentary));
        assert_eq!(label.title, None);
        assert_eq!(label.channels, Some(Channels::Stereo));
        let label = audio_track(&audio(AudioRole::Commentary, Some("Director")));
        assert_eq!(label.title.as_deref(), Some("Director"));
    }

    #[test]
    fn a_title_that_is_only_the_language_is_left_out() {
        for title in ["English", "eng", "ENGLISH", " "] {
            let label = audio_track(&audio(AudioRole::Main, Some(title)));
            assert_eq!(label.title, None, "{title:?}");
        }
        let label = audio_track(&audio(AudioRole::Main, Some("English (US)")));
        assert_eq!(label.title.as_deref(), Some("English (US)"));
    }

    #[test]
    fn a_forced_subtitle_says_so() {
        let mut track = subtitle(false);
        assert!(!subtitle_track(&track).forced);
        track.forced = true;
        assert!(subtitle_track(&track).forced);
    }

    #[test]
    fn the_menu_numbers_a_label_an_earlier_row_has() {
        let tracks = MediaTracks {
            subtitles: vec![
                subtitle(false),
                subtitle(true),
                subtitle(false),
                subtitle(false),
            ],
            ..MediaTracks::default()
        };
        let menu = track_menu(&tracks, None);
        assert_eq!(
            menu.subtitles.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            [None, None, Some(2), Some(3)]
        );
    }

    fn subtitle(external: bool) -> SubtitleTrack {
        SubtitleTrack {
            language: Some("en".into()),
            title: Some("SDH".into()),
            forced: false,
            default: false,
            external,
            role: SubtitleRole::Sdh,
            timing: None,
            download: None,
        }
    }

    #[test]
    fn a_subtitle_says_where_it_is_from() {
        assert_eq!(subtitle_track(&subtitle(false)).origin, TrackOrigin::InFile);
        let external = subtitle_track(&subtitle(true));
        assert_eq!(
            (external.origin, external.title),
            (TrackOrigin::ExternalFile, None)
        );
    }

    #[test]
    fn the_menu_ticks_the_selection_else_the_default_audio_and_no_subtitles() {
        let mut main = audio(AudioRole::Main, None);
        main.default = true;
        let tracks = MediaTracks {
            audio: vec![audio(AudioRole::Commentary, None), main],
            subtitles: vec![subtitle(false)],
            ..MediaTracks::default()
        };
        let menu = track_menu(&tracks, None);
        assert_eq!(
            menu.audio.iter().map(|r| r.selected).collect::<Vec<_>>(),
            [false, true]
        );
        assert!(menu.subtitles_off);
        let picked = Selection {
            audio: Some(0),
            subtitle: Some(0),
        };
        let menu = track_menu(&tracks, Some(&picked));
        assert!(menu.audio[0].selected && menu.subtitles[0].selected && !menu.subtitles_off);
    }

    #[test]
    fn a_scope_film_in_4k_is_2160p_hdr10() {
        let video = VideoTrack {
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 1600,
            bit_depth: 10,
            hdr: Some(HdrFormat::Hdr10),
            bit_rate: None,
        };
        assert_eq!(
            picture(Some(Container::Matroska), Some(&video)),
            Some(PictureClass {
                container: Some(Container::Matroska),
                codec: Some(VideoCodec::Hevc),
                resolution: Some(Resolution::Uhd),
                hdr: Some(HdrFormat::Hdr10),
            })
        );
        assert_eq!(picture(None, None), None);
    }

    #[test]
    fn lossless_music_says_its_depth_and_rate_and_lossy_does_not() {
        assert_eq!(
            audio_format(Some(AudioCodec::Flac), Some(24), Some(96_000)),
            Some(AudioFormat {
                codec: AudioCodec::Flac,
                lossless: true,
                bit_depth: Some(24),
                sample_rate: Some(96_000),
            })
        );
        let aac = audio_format(Some(AudioCodec::Aac), Some(16), Some(44_100)).unwrap();
        assert_eq!(
            (aac.lossless, aac.bit_depth, aac.sample_rate),
            (false, None, None)
        );
    }

    #[test]
    fn what_is_left_rounds_up_and_ago_goes_by_the_largest_unit() {
        assert_eq!(
            remaining(Duration::from_secs(5400), Duration::from_secs(10)),
            HoursMinutes {
                hours: 1,
                minutes: 30
            }
        );
        assert_eq!(
            remaining(Duration::from_secs(60), Duration::from_secs(59)).minutes,
            1
        );
        assert_eq!(ago(100, 130), Ago::JustNow);
        assert_eq!(ago(0, 7200), Ago::Hours(2));
        assert_eq!(ago(0, 3 * 86_400), Ago::Days(3));
    }

    #[test]
    fn played_says_how_often_and_when() {
        let state = PlaybackState {
            play_count: 3,
            last_played_at: Some(1000),
            ..PlaybackState::default()
        };
        assert_eq!(
            played(&state, 1000 + 120),
            Some(Played {
                count: 3,
                last_at: Some(1000),
                last: Some(Ago::Minutes(2))
            })
        );
        assert_eq!(played(&PlaybackState::default(), 0), None);
    }

    #[test]
    fn an_update_names_the_older_side() {
        let ours = spoolframe_protocol::PROTOCOL_VERSION;
        let refused = |server_protocol| SdkError::UpdateRequired { server_protocol };
        assert_eq!(
            refused(Some(ours + 1)).who_updates(),
            Some(UpdateNeeded::App)
        );
        assert_eq!(
            refused(Some(ours.saturating_sub(1))).who_updates(),
            Some(UpdateNeeded::Server)
        );
        assert_eq!(refused(None).who_updates(), Some(UpdateNeeded::Either));
        assert_eq!(SdkError::NotAllowed.who_updates(), None);
    }

    #[test]
    fn a_filters_years_read_as_one_a_span_or_open() {
        let years = |from, to| {
            year_range(&ItemFilter {
                year_from: from,
                year_to: to,
                ..ItemFilter::default()
            })
        };
        assert_eq!(years(Some(1990), Some(1990)), YearRange::One(1990));
        assert_eq!(
            years(Some(1990), Some(1999)),
            YearRange::Between(1990, 1999)
        );
        assert_eq!(years(Some(2010), None), YearRange::Since(2010));
        assert_eq!(years(None, None), YearRange::Any);
    }

    #[test]
    fn a_card_caption_is_the_items_own() {
        let today = spoolframe_types::CalendarDate::new(2026, 10, 14).unwrap();
        let track = spoolframe_types::Item::Track(spoolframe_types::Track {
            id: "plex:track:1".parse().unwrap(),
            album_id: "plex:album:1".parse().unwrap(),
            artist_id: "plex:artist:1".parse().unwrap(),
            title: "t".into(),
            track: None,
            disc: None,
            duration: Some(Duration::from_secs(200)),
            playback: PlaybackState::default(),
            codec: None,
            bit_depth: None,
            sample_rate: None,
            album_track_count: None,
            artist_name: None,
            album_title: None,
            artwork: Vec::new(),
        });
        assert_eq!(
            track.caption_parts(today),
            Some(spoolframe_types::Caption::Length(Duration::from_secs(200)))
        );
    }

    #[test]
    fn the_path_chip_names_how_the_server_is_reached() {
        assert_eq!(path_chip(ConnectionPath::Relay), Some(PathChip::Relayed));
        assert_eq!(path_chip(ConnectionPath::Unknown), None);
    }
}
