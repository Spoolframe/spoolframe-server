//! Optimized versions (PX-37): storage. The admin's profiles, the pool's
//! copies, and the tracks they are made of: each converted stream a file
//! of its own, shared by every copy that needs it (the user's decision,
//! 2026-10-20). Making, serving and evicting the files is the pool's;
//! this module only keeps their records.

use std::collections::{BTreeSet, HashMap};

use spoolframe_protocol::{ConvertedVideo, PlaybackPlan};
use spoolframe_types::{AudioCodec, VideoCodec};

use super::{Store, StoreError};
use crate::downloads::DeviceClass;

pub mod requests;
use crate::downloads::music::MusicQuality;

/// A profile (migration 0038): what its copies' video is converted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub video_codec: VideoCodec,
    /// Lines in the output.
    pub height: u32,
    /// Bits per second.
    pub max_bitrate: u64,
}

impl Profile {
    /// The longest name the web UI lists without cutting it.
    pub const MAX_NAME: usize = 40;

    /// What is wrong with it, said for the admin; `None` if nothing.
    pub fn problem(&self) -> Option<String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Some("a profile needs a name".into());
        }
        if name != self.name {
            return Some("a profile's name may not start or end with a space".into());
        }
        if name.chars().count() > Self::MAX_NAME {
            return Some(format!(
                "a profile's name is at most {} characters",
                Self::MAX_NAME
            ));
        }
        if !matches!(
            self.video_codec,
            VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1
        ) {
            return Some(format!(
                "{} is not a codec a copy is made in: use h264, hevc or av1",
                self.video_codec
            ));
        }
        if !(144..=4320).contains(&self.height) {
            return Some(format!(
                "a height of {} lines: use 144 to 4320",
                self.height
            ));
        }
        if self.max_bitrate < 100_000 {
            return Some("a bitrate under 100 kbit/s makes nothing watchable".into());
        }
        None
    }

    /// Whether `other` makes a different video, so copies made for one
    /// are not the other's.
    fn video_differs(&self, other: &Profile) -> bool {
        self.video_codec != other.video_codec
            || self.height != other.height
            || self.max_bitrate != other.max_bitrate
    }
}

/// What writing a profile did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileWritten {
    /// It was not there before.
    pub created: bool,
    /// Its video changed: its copies are outdated, and its on-demand
    /// requests' are to be made again.
    pub changed: bool,
    /// Its copies marked outdated.
    pub outdated: u64,
}

/// What a copy is made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyTarget {
    /// A profile, by name.
    Profile(String),
    /// Binge-ahead: the next episode, with a playback's own plan.
    Ahead,
    /// A download in original quality (PX-38) for a device class: only
    /// what the class cannot play is converted, its video copied when it
    /// plays it. Queued and evicted as a profile's copy is.
    Original { class: DeviceClass },
    /// A track's download (PX-51) for a device class, in a quality: its
    /// one audio track converted as [`crate::downloads::music`] says.
    /// Queued and evicted as a profile's copy is.
    Music {
        class: DeviceClass,
        quality: MusicQuality,
    },
}

impl CopyTarget {
    /// The audio codecs a copy for this target keeps as they are; `None`
    /// for PX-37's rule (AAC, AC-3 and E-AC-3).
    pub fn keep_audio(&self) -> Option<BTreeSet<AudioCodec>> {
        match self {
            Self::Original { class } => Some(class.keep_audio()),
            Self::Profile(_) | Self::Ahead | Self::Music { .. } => None,
        }
    }

    /// What the admin's Server › Optimized page calls it.
    pub fn label(&self) -> String {
        match self {
            Self::Profile(name) => name.clone(),
            Self::Ahead => "ahead".to_owned(),
            Self::Original {
                class: DeviceClass::Apple,
            } => "original (Apple)".to_owned(),
            Self::Original {
                class: DeviceClass::Linux,
            } => "original (Linux)".to_owned(),
            Self::Music { class, quality } => {
                let class = match class {
                    DeviceClass::Apple => "Apple",
                    DeviceClass::Linux => "Linux",
                };
                match quality {
                    MusicQuality::Original => format!("music (original, {class})"),
                    MusicQuality::Aac256 => format!("music (AAC 256, {class})"),
                }
            }
        }
    }
}

/// How a copy's video is made: the profile's, or the playback's for an
/// `ahead` copy, or the file's own copied for an `original` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyVideo {
    pub codec: VideoCodec,
    pub height: u32,
    /// Bits per second; `None` for the encoder's own choice.
    pub max_bitrate: Option<u64>,
    /// HDR mapped to SDR.
    pub tone_map: bool,
    /// The file's video copied as it is (PX-38): `codec` and `height` are
    /// the file's, and nothing is encoded.
    pub copied: bool,
}

impl CopyVideo {
    /// The video of `plan`, if it encodes it, as a copy makes it.
    pub fn of_plan(plan: &PlaybackPlan) -> Option<Self> {
        match plan {
            PlaybackPlan::Converted {
                video:
                    Some(ConvertedVideo::Encode {
                        codec,
                        height,
                        max_bitrate,
                        tone_map,
                    }),
                ..
            } => Some(Self {
                codec: *codec,
                height: *height,
                max_bitrate: *max_bitrate,
                tone_map: *tone_map,
                copied: false,
            }),
            _ => None,
        }
    }
}

impl CopyVideo {
    /// Profile `profile`'s video for a file whose video is `height` lines,
    /// `hdr` or not: never taller than the file, and tone mapped where
    /// H.264 carries no HDR a player shows. A download's copy in the
    /// profile and an on-demand one are the same copy.
    pub fn of_profile(profile: &Profile, height: u32, hdr: bool) -> Self {
        Self {
            codec: profile.video_codec,
            height: profile.height.min(height),
            max_bitrate: Some(profile.max_bitrate),
            tone_map: hdr && profile.video_codec == VideoCodec::H264,
            copied: false,
        }
    }
}

/// The plan's video for a copy made as it says.
impl From<CopyVideo> for ConvertedVideo {
    fn from(video: CopyVideo) -> Self {
        let CopyVideo {
            codec,
            height,
            max_bitrate,
            tone_map,
            copied,
        } = video;
        if copied {
            return Self::Copy {
                codec,
                dolby_vision: false,
            };
        }
        Self::Encode {
            codec,
            height,
            max_bitrate,
            tone_map,
        }
    }
}

/// Where a copy, or one of its tracks, is in its making.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyState {
    /// Asked for, waiting for a place in the encode budget.
    Waiting,
    /// Its ffmpeg is running (a copy's: one of its tracks').
    Making,
    /// Its file is whole (a copy's: every one of its tracks').
    Ready,
    /// Its ffmpeg failed; `failure` says why.
    Failed,
}

impl CopyState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Making => "making",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self, sqlx::Error> {
        Ok(match s {
            "waiting" => Self::Waiting,
            "making" => Self::Making,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            other => return Err(decode_error("state", other)),
        })
    }
}

/// A copy asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCopy {
    pub media_id: String,
    pub media_path: String,
    /// The media file's size and modification time now.
    pub source_stamp: String,
    pub target: CopyTarget,
    pub video: CopyVideo,
}

/// A copy's row, with what its tracks say of it: its state, its size
/// and when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRow {
    /// The server's own.
    pub id: String,
    pub media_id: String,
    pub media_path: String,
    pub source_stamp: String,
    pub target: CopyTarget,
    pub video: CopyVideo,
    /// The audio codecs it keeps as they are; `None` for PX-37's rule.
    pub keep_audio: Option<BTreeSet<AudioCodec>>,
    /// Failed if it or a track failed, ready once every track is, making
    /// while one is made, else waiting. A copy with no track to store is
    /// ready at once.
    pub state: CopyState,
    /// Its tracks' files together, once all are made.
    pub size_bytes: Option<u64>,
    pub failure: Option<String>,
    pub outdated: bool,
    pub requested_at: i64,
    /// When its last track was made.
    pub made_at: Option<i64>,
    pub last_used_at: i64,
    /// A pinned on-demand request wants it: never evicted.
    pub pinned: bool,
    /// Its tracks, in the order it asked for them: its video first.
    pub tracks: Vec<TrackRow>,
}

/// What a track is: one converted stream of a media file, in a file of
/// its own (the user's decision, 2026-10-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// The file's video, converted as this says (never `copied`).
    Video(CopyVideo),
    /// One of its audio tracks, converted.
    Audio(AudioRecipe),
}

/// An audio track converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRecipe {
    /// Its stream index in the media file.
    pub stream: u32,
    /// E-AC-3, or AAC where the server has no E-AC-3 encoder.
    pub codec: AudioCodec,
    pub channels: u32,
    /// Bits per second.
    pub bitrate: u64,
}

impl Recipe {
    /// The `recipe` column: one string per track of a file.
    pub fn key(&self) -> String {
        match self {
            Self::Video(v) => format!(
                "video:{}:{}:{}:{}",
                v.codec,
                v.height,
                v.max_bitrate
                    .map_or_else(|| "auto".to_owned(), |b| b.to_string()),
                if v.tone_map { "sdr" } else { "same" }
            ),
            Self::Audio(a) => format!(
                "audio:{}:{}:{}:{}",
                a.stream, a.codec, a.channels, a.bitrate
            ),
        }
    }

    /// The recipe a `recipe` column names.
    fn parse(key: &str) -> Result<Self, sqlx::Error> {
        let bad = || decode_error("recipe", key);
        let parts: Vec<&str> = key.split(':').collect();
        match parts.as_slice() {
            ["video", codec_name, height, bitrate, range] => Ok(Self::Video(CopyVideo {
                codec: codec(codec_name)?,
                height: height.parse().map_err(|_| bad())?,
                max_bitrate: match *bitrate {
                    "auto" => None,
                    b => Some(b.parse().map_err(|_| bad())?),
                },
                tone_map: match *range {
                    "sdr" => true,
                    "same" => false,
                    _ => return Err(bad()),
                },
                copied: false,
            })),
            ["audio", stream, codec_name, channels, bitrate] => Ok(Self::Audio(AudioRecipe {
                stream: stream.parse().map_err(|_| bad())?,
                codec: match serde_json::from_value(serde_json::Value::String(
                    (*codec_name).to_owned(),
                )) {
                    Ok(AudioCodec::Unknown) | Err(_) => return Err(bad()),
                    Ok(c) => c,
                },
                channels: channels.parse().map_err(|_| bad())?,
                bitrate: bitrate.parse().map_err(|_| bad())?,
            })),
            _ => Err(bad()),
        }
    }
}

/// A track's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    /// The server's own; it names the file.
    pub id: String,
    pub media_path: String,
    pub source_stamp: String,
    pub recipe: Recipe,
    pub state: CopyState,
    pub size_bytes: Option<u64>,
    pub failure: Option<String>,
    pub outdated: bool,
    pub requested_at: i64,
    pub made_at: Option<i64>,
    pub last_used_at: i64,
    /// No live copy uses it but binge-ahead ones (or none does): it waits
    /// behind the others, and goes first after the outdated ones.
    pub ahead_only: bool,
    /// Every live copy that uses it, but binge-ahead ones, is an
    /// on-demand request's: it waits behind downloads' tracks.
    pub on_demand_only: bool,
    /// A live copy a pinned on-demand request wants uses it: it is never
    /// evicted, though it goes when outdated.
    pub pinned: bool,
}

/// The `target` column's prefix for a profile's copy, its name after it.
const PROFILE_PREFIX: &str = "profile:";

/// The `target` column's prefix for a binge-ahead copy, its plan after it.
const AHEAD_PREFIX: &str = "ahead:";

/// The `target` column's prefix for a download's copy in original
/// quality: its device class, then its video, after it.
const ORIGINAL_PREFIX: &str = "original:";

/// The `target` column's prefix for a track's download (PX-51): its
/// device class, then its quality, after it.
const MUSIC_PREFIX: &str = "music:";

/// The `target` column: one string per target, so a media file has one
/// live copy for each. An `ahead` copy is unique by its plan; an
/// `original` one by its class and video, `copy` for the file's own, or
/// the codec, height and range it is converted to (`hdr`: the file's own
/// range kept, `sdr`: tone mapped).
fn target_key(target: &CopyTarget, video: &CopyVideo) -> String {
    match target {
        CopyTarget::Original { class } if video.copied => {
            format!("{ORIGINAL_PREFIX}{}:copy", class.as_str())
        }
        CopyTarget::Original { class } => format!(
            "{ORIGINAL_PREFIX}{}:{}:{}:{}",
            class.as_str(),
            video.codec,
            video.height,
            if video.tone_map { "sdr" } else { "hdr" }
        ),
        CopyTarget::Music { class, quality } => {
            format!("{MUSIC_PREFIX}{}:{}", class.as_str(), quality.as_str())
        }
        CopyTarget::Profile(name) => format!("{PROFILE_PREFIX}{name}"),
        CopyTarget::Ahead => format!(
            "{AHEAD_PREFIX}{}:{}:{}:{}",
            video.codec,
            video.height,
            video
                .max_bitrate
                .map_or_else(|| "auto".to_owned(), |b| b.to_string()),
            if video.tone_map { "sdr" } else { "same" }
        ),
    }
}

fn decode_error(column: &str, value: &str) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.to_owned(),
        source: format!("{value:?} is not a value this build knows").into(),
    }
}

/// The video codec `s` names, as the protocol spells it; `None` for one
/// this build does not know.
pub(crate) fn parse_video_codec(s: &str) -> Option<VideoCodec> {
    match serde_json::from_value(serde_json::Value::String(s.to_owned())) {
        Ok(VideoCodec::Unknown) | Err(_) => None,
        Ok(c) => Some(c),
    }
}

fn codec(s: &str) -> Result<VideoCodec, sqlx::Error> {
    parse_video_codec(s).ok_or_else(|| decode_error("video_codec", s))
}

/// The `keep_audio` column of `keep`.
fn keep_audio_text(keep: Option<&BTreeSet<AudioCodec>>) -> Option<String> {
    keep.map(|codecs| {
        codecs
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(",")
    })
}

/// The codecs a `keep_audio` column names; one this build does not know
/// is refused, as the copy would be made otherwise than it says.
fn keep_audio(text: Option<String>) -> Result<Option<BTreeSet<AudioCodec>>, sqlx::Error> {
    let Some(text) = text else {
        return Ok(None);
    };
    text.split(',')
        .filter(|name| !name.is_empty())
        .map(
            |name| match serde_json::from_value(serde_json::Value::String(name.to_owned())) {
                Ok(AudioCodec::Unknown) | Err(_) => Err(decode_error("keep_audio", name)),
                Ok(codec) => Ok(codec),
            },
        )
        .collect::<Result<_, _>>()
        .map(Some)
}

/// The copy target `key`, a `target` column.
fn target_of(key: &str) -> Result<CopyTarget, sqlx::Error> {
    if let Some(name) = key.strip_prefix(PROFILE_PREFIX) {
        return Ok(CopyTarget::Profile(name.to_owned()));
    }
    if key.starts_with(AHEAD_PREFIX) {
        return Ok(CopyTarget::Ahead);
    }
    if let Some(rest) = key.strip_prefix(ORIGINAL_PREFIX)
        && let Some(class) = rest.split(':').next().and_then(DeviceClass::parse)
    {
        return Ok(CopyTarget::Original { class });
    }
    if let Some(rest) = key.strip_prefix(MUSIC_PREFIX)
        && let Some((class, quality)) = rest.split_once(':')
        && let (Some(class), Some(quality)) =
            (DeviceClass::parse(class), MusicQuality::parse(quality))
    {
        return Ok(CopyTarget::Music { class, quality });
    }
    Err(decode_error("target", key))
}

fn unsigned(column: &str, v: i64) -> Result<u64, sqlx::Error> {
    u64::try_from(v).map_err(|_| decode_error(column, &v.to_string()))
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn profile_row(r: &sqlx::sqlite::SqliteRow) -> Result<Profile, sqlx::Error> {
    use sqlx::Row;
    Ok(Profile {
        name: r.try_get("name")?,
        video_codec: codec(&r.try_get::<String, _>("video_codec")?)?,
        height: u32::try_from(r.try_get::<i64, _>("height")?).unwrap_or(0),
        max_bitrate: unsigned("max_bitrate", r.try_get("max_bitrate")?)?,
    })
}

/// A copy's columns, for [`copy_row`].
macro_rules! columns {
    () => {
        "id, media_id, media_path, source_stamp, target, video_codec, height, \
         max_bitrate, tone_map, video_copied, keep_audio, failure, outdated, \
         requested_at, last_used_at, \
         EXISTS (SELECT 1 FROM optimized_request_items i \
           JOIN optimized_requests r ON r.id = i.request_id \
           WHERE i.copy_id = optimized_copies.id AND r.pinned = 1) AS pinned"
    };
}

/// A track's columns, for [`track_row`], the track named `t`.
macro_rules! track_columns {
    () => {
        "t.id, t.media_path, t.source_stamp, t.recipe, t.state, t.size_bytes, t.failure, \
         t.outdated, t.requested_at, t.made_at, t.last_used_at, \
         NOT EXISTS (SELECT 1 FROM optimized_copy_tracks l \
           JOIN optimized_copies c ON c.id = l.copy_id \
           WHERE l.track_id = t.id AND c.outdated = 0 AND c.target NOT LIKE 'ahead:%') \
         AS ahead_only, \
         NOT EXISTS (SELECT 1 FROM optimized_copy_tracks l \
           JOIN optimized_copies c ON c.id = l.copy_id \
           WHERE l.track_id = t.id AND c.outdated = 0 AND c.target NOT LIKE 'ahead:%' \
           AND NOT EXISTS (SELECT 1 FROM optimized_request_items i WHERE i.copy_id = c.id)) \
         AS on_demand_only, \
         EXISTS (SELECT 1 FROM optimized_copy_tracks l \
           JOIN optimized_copies c ON c.id = l.copy_id \
           JOIN optimized_request_items i ON i.copy_id = c.id \
           JOIN optimized_requests r ON r.id = i.request_id \
           WHERE l.track_id = t.id AND c.outdated = 0 AND r.pinned = 1) AS pinned"
    };
}

/// A copy's row, its state as `tracks` (its tracks' rows) make it.
fn copy_row(r: &sqlx::sqlite::SqliteRow, tracks: Vec<TrackRow>) -> Result<CopyRow, sqlx::Error> {
    use sqlx::Row;
    let target = target_of(&r.try_get::<String, _>("target")?)?;
    let requested_at: i64 = r.try_get("requested_at")?;
    let own_failure: Option<String> = r.try_get("failure")?;
    let failed = tracks.iter().find(|t| t.state == CopyState::Failed);
    let (state, failure) = match (own_failure, failed) {
        (Some(why), _) => (CopyState::Failed, Some(why)),
        (None, Some(t)) => (CopyState::Failed, t.failure.clone()),
        (None, None) if tracks.iter().all(|t| t.state == CopyState::Ready) => {
            (CopyState::Ready, None)
        }
        (None, None) if tracks.iter().any(|t| t.state == CopyState::Making) => {
            (CopyState::Making, None)
        }
        (None, None) => (CopyState::Waiting, None),
    };
    let ready = state == CopyState::Ready;
    Ok(CopyRow {
        id: r.try_get("id")?,
        media_id: r.try_get("media_id")?,
        media_path: r.try_get("media_path")?,
        source_stamp: r.try_get("source_stamp")?,
        video: match target {
            // A track's (PX-51) has no video: its row's says so as
            // `unknown`, which no film's is.
            CopyTarget::Music { .. } => crate::downloads::music::NO_VIDEO,
            _ => CopyVideo {
                codec: codec(&r.try_get::<String, _>("video_codec")?)?,
                height: u32::try_from(r.try_get::<i64, _>("height")?).unwrap_or(0),
                max_bitrate: r
                    .try_get::<Option<i64>, _>("max_bitrate")?
                    .map(|b| unsigned("max_bitrate", b))
                    .transpose()?,
                tone_map: r.try_get("tone_map")?,
                copied: r.try_get("video_copied")?,
            },
        },
        target,
        keep_audio: keep_audio(r.try_get("keep_audio")?)?,
        state,
        size_bytes: ready.then(|| tracks.iter().filter_map(|t| t.size_bytes).sum()),
        failure,
        outdated: r.try_get("outdated")?,
        requested_at,
        made_at: ready.then(|| {
            tracks
                .iter()
                .filter_map(|t| t.made_at)
                .max()
                .unwrap_or(requested_at)
        }),
        last_used_at: r.try_get("last_used_at")?,
        pinned: r.try_get("pinned")?,
        tracks,
    })
}

fn track_row(r: &sqlx::sqlite::SqliteRow) -> Result<TrackRow, sqlx::Error> {
    use sqlx::Row;
    Ok(TrackRow {
        id: r.try_get("id")?,
        media_path: r.try_get("media_path")?,
        source_stamp: r.try_get("source_stamp")?,
        recipe: Recipe::parse(&r.try_get::<String, _>("recipe")?)?,
        state: CopyState::parse(&r.try_get::<String, _>("state")?)?,
        size_bytes: r
            .try_get::<Option<i64>, _>("size_bytes")?
            .map(|s| unsigned("size_bytes", s))
            .transpose()?,
        failure: r.try_get("failure")?,
        outdated: r.try_get("outdated")?,
        requested_at: r.try_get("requested_at")?,
        made_at: r.try_get("made_at")?,
        last_used_at: r.try_get("last_used_at")?,
        ahead_only: r.try_get("ahead_only")?,
        on_demand_only: r.try_get("on_demand_only")?,
        pinned: r.try_get("pinned")?,
    })
}

impl Store {
    /// The profiles, in the order the admin put them.
    pub async fn optimized_profiles(&self) -> Result<Vec<Profile>, StoreError> {
        let rows = sqlx::query(
            "SELECT name, video_codec, height, max_bitrate FROM optimized_profiles \
             ORDER BY position, name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(profile_row).collect::<Result<_, _>>()?)
    }

    /// Profile `name`, if there is one.
    pub async fn optimized_profile(&self, name: &str) -> Result<Option<Profile>, StoreError> {
        let found = sqlx::query(
            "SELECT name, video_codec, height, max_bitrate FROM optimized_profiles \
             WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(profile_row).transpose()?)
    }

    /// Write `profile` over the one of its name, or add it last. If its
    /// video changed, its copies are outdated, never used again, and so
    /// are their video tracks no other live copy uses: their audio tracks
    /// stay for the next copy. The caller has checked [`Profile::problem`].
    pub async fn put_optimized_profile(
        &self,
        profile: &Profile,
    ) -> Result<ProfileWritten, StoreError> {
        let mut tx = self.begin_write().await?;
        let before = sqlx::query(
            "SELECT name, video_codec, height, max_bitrate FROM optimized_profiles \
             WHERE name = ?1",
        )
        .bind(&profile.name)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(profile_row)
        .transpose()?;
        let now = self.now();
        let written = match &before {
            Some(before) => {
                sqlx::query(
                    "UPDATE optimized_profiles SET video_codec = ?2, height = ?3, \
                     max_bitrate = ?4, updated_at = ?5 WHERE name = ?1",
                )
                .bind(&profile.name)
                .bind(profile.video_codec.as_str())
                .bind(i64::from(profile.height))
                .bind(signed(profile.max_bitrate))
                .bind(now)
                .execute(&mut *tx)
                .await?;
                let changed = before.video_differs(profile);
                let outdated = if changed {
                    outdate_profile_copies(&mut tx, &profile.name).await?
                } else {
                    0
                };
                ProfileWritten {
                    created: false,
                    changed,
                    outdated,
                }
            }
            None => {
                sqlx::query(
                    "INSERT INTO optimized_profiles \
                     (name, video_codec, height, max_bitrate, position, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, \
                     (SELECT COALESCE(MAX(position) + 1, 0) FROM optimized_profiles), ?5)",
                )
                .bind(&profile.name)
                .bind(profile.video_codec.as_str())
                .bind(i64::from(profile.height))
                .bind(signed(profile.max_bitrate))
                .bind(now)
                .execute(&mut *tx)
                .await?;
                ProfileWritten {
                    created: true,
                    changed: false,
                    outdated: 0,
                }
            }
        };
        tx.commit().await?;
        Ok(written)
    }

    /// Remove profile `name`, its copies outdated as an edit of its video
    /// outdates them, and its on-demand requests with it: whether it was
    /// there, and how many copies it outdated.
    pub async fn delete_optimized_profile(&self, name: &str) -> Result<Option<u64>, StoreError> {
        let mut tx = self.begin_write().await?;
        let gone = sqlx::query("DELETE FROM optimized_profiles WHERE name = ?1")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        if gone.rows_affected() == 0 {
            return Ok(None);
        }
        let outdated = outdate_profile_copies(&mut tx, name).await?;
        requests::delete_profile_requests(&mut tx, name).await?;
        tx.commit().await?;
        Ok(Some(outdated))
    }

    /// The live copy for `copy`'s file and target, or a new one made of
    /// `recipes`, its tracks: one copy per file and target, whoever asks,
    /// and one track per file and recipe, whichever copies need it. A
    /// track not made yet is waiting. A live copy of the file as it was
    /// (another stamp) is outdated, with every track of that file as it
    /// was, and a new one asked for. The bool is whether the copy is new.
    pub async fn request_optimized_copy(
        &self,
        copy: &NewCopy,
        recipes: &[Recipe],
    ) -> Result<(CopyRow, bool), StoreError> {
        let target = target_key(&copy.target, &copy.video);
        let mut tx = self.begin_write().await?;
        let live: Option<(String, String)> = sqlx::query_as(
            "SELECT id, source_stamp FROM optimized_copies \
             WHERE media_path = ?1 AND target = ?2 AND outdated = 0",
        )
        .bind(&copy.media_path)
        .bind(&target)
        .fetch_optional(&mut *tx)
        .await?;
        let new = match live {
            Some((id, stamp)) if stamp == copy.source_stamp => {
                tx.commit().await?;
                return Ok((self.copy_there(&id).await?, false));
            }
            Some((id, _)) => {
                sqlx::query("UPDATE optimized_copies SET outdated = 1 WHERE id = ?1")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                uuid::Uuid::new_v4().simple().to_string()
            }
            None => uuid::Uuid::new_v4().simple().to_string(),
        };
        // The file's tracks as it was are of no copy from now on.
        sqlx::query(
            "UPDATE optimized_tracks SET outdated = 1 \
             WHERE media_path = ?1 AND source_stamp != ?2 AND outdated = 0",
        )
        .bind(&copy.media_path)
        .bind(&copy.source_stamp)
        .execute(&mut *tx)
        .await?;
        let now = self.now();
        sqlx::query(
            "INSERT INTO optimized_copies (id, media_id, media_path, source_stamp, target, \
             video_codec, height, max_bitrate, tone_map, video_copied, keep_audio, \
             requested_at, last_used_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        )
        .bind(&new)
        .bind(&copy.media_id)
        .bind(&copy.media_path)
        .bind(&copy.source_stamp)
        .bind(&target)
        .bind(copy.video.codec.as_str())
        .bind(i64::from(copy.video.height))
        .bind(copy.video.max_bitrate.map(signed))
        .bind(copy.video.tone_map)
        .bind(copy.video.copied)
        .bind(keep_audio_text(copy.target.keep_audio().as_ref()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        for (position, recipe) in recipes.iter().enumerate() {
            let key = recipe.key();
            let found: Option<String> = sqlx::query_scalar(
                "SELECT id FROM optimized_tracks \
                 WHERE media_path = ?1 AND recipe = ?2 AND outdated = 0",
            )
            .bind(&copy.media_path)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
            let track = match found {
                Some(track) => track,
                None => {
                    let track = uuid::Uuid::new_v4().simple().to_string();
                    sqlx::query(
                        "INSERT INTO optimized_tracks (id, media_path, source_stamp, recipe, \
                         state, requested_at, last_used_at) \
                         VALUES (?1, ?2, ?3, ?4, 'waiting', ?5, ?5)",
                    )
                    .bind(&track)
                    .bind(&copy.media_path)
                    .bind(&copy.source_stamp)
                    .bind(&key)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    track
                }
            };
            sqlx::query(
                "INSERT OR IGNORE INTO optimized_copy_tracks (copy_id, track_id, position) \
                 VALUES (?1, ?2, ?3)",
            )
            .bind(&new)
            .bind(&track)
            .bind(position as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok((self.copy_there(&new).await?, true))
    }

    /// Copy `id`, which the caller knows is there.
    /// The live copy of `media_path` for `target` made with `video`, if
    /// there is one: what a request for it would be given.
    pub async fn live_optimized_copy(
        &self,
        media_path: &str,
        target: &CopyTarget,
        video: &CopyVideo,
    ) -> Result<Option<CopyRow>, StoreError> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM optimized_copies \
             WHERE media_path = ?1 AND target = ?2 AND outdated = 0",
        )
        .bind(media_path)
        .bind(target_key(target, video))
        .fetch_optional(&self.pool)
        .await?;
        match id {
            Some(id) => self.optimized_copy(&id).await,
            None => Ok(None),
        }
    }

    async fn copy_there(&self, id: &str) -> Result<CopyRow, StoreError> {
        self.optimized_copy(id)
            .await?
            .ok_or(StoreError::Db(sqlx::Error::RowNotFound))
    }

    /// Copy `id`, if there is one.
    pub async fn optimized_copy(&self, id: &str) -> Result<Option<CopyRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(found) = found else {
            return Ok(None);
        };
        let tracks = self.optimized_copy_tracks(id).await?;
        Ok(Some(copy_row(&found, tracks)?))
    }

    /// `rows` (of [`columns!`]) as copies, each with its tracks.
    async fn copies_of(
        &self,
        rows: &[sqlx::sqlite::SqliteRow],
    ) -> Result<Vec<CopyRow>, StoreError> {
        use sqlx::Row;
        let ids = rows
            .iter()
            .map(|r| r.try_get::<String, _>("id"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut of: HashMap<String, Vec<TrackRow>> = HashMap::new();
        // In parts: SQLite takes only so many values in one statement.
        for part in ids.chunks(500) {
            let mut query = sqlx::QueryBuilder::new(concat!(
                "SELECT m.copy_id, ",
                track_columns!(),
                " FROM optimized_tracks t JOIN optimized_copy_tracks m ON m.track_id = t.id \
                 WHERE m.copy_id IN "
            ));
            super::push_in_list(&mut query, part);
            query.push(" ORDER BY m.copy_id, m.position");
            for r in query.build().fetch_all(&self.pool).await? {
                of.entry(r.try_get("copy_id")?)
                    .or_default()
                    .push(track_row(&r)?);
            }
        }
        let copies = rows
            .iter()
            .zip(ids)
            .map(|(r, id)| copy_row(r, of.remove(&id).unwrap_or_default()))
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(copies)
    }

    /// Every copy, in the order the admin's page lists them: outdated ones
    /// first, then `ahead` ones, then the least recently used.
    pub async fn optimized_copies(&self) -> Result<Vec<CopyRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies \
             ORDER BY outdated DESC, target LIKE 'ahead:%' DESC, last_used_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        self.copies_of(&rows).await
    }

    /// The outdated copies, by id: what the pool's sweep removes once no
    /// lease holds them.
    pub async fn outdated_optimized_copies(&self) -> Result<Vec<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM optimized_copies WHERE outdated = 1 ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// The binge-ahead copies.
    pub async fn ahead_optimized_copies(&self) -> Result<Vec<CopyRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies WHERE target LIKE 'ahead:%' ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        self.copies_of(&rows).await
    }

    /// The live, ready copies of media file `media_path`: what the
    /// planner may play.
    pub async fn ready_optimized_copies(
        &self,
        media_path: &str,
    ) -> Result<Vec<CopyRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies WHERE media_path = ?1 AND outdated = 0 ORDER BY id"
        ))
        .bind(media_path)
        .fetch_all(&self.pool)
        .await?;
        let mut copies = self.copies_of(&rows).await?;
        copies.retain(|c| c.state == CopyState::Ready);
        Ok(copies)
    }

    /// Copy `id`'s tracks, in the order it asked for them: its video
    /// first.
    pub async fn optimized_copy_tracks(&self, id: &str) -> Result<Vec<TrackRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            track_columns!(),
            " FROM optimized_tracks t JOIN optimized_copy_tracks m ON m.track_id = t.id \
             WHERE m.copy_id = ?1 ORDER BY m.position"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(track_row).collect::<Result<_, _>>()?)
    }

    /// Track `id`, if there is one.
    pub async fn optimized_track(&self, id: &str) -> Result<Option<TrackRow>, StoreError> {
        let found = sqlx::query(concat!(
            "SELECT ",
            track_columns!(),
            " FROM optimized_tracks t WHERE t.id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.as_ref().map(track_row).transpose()?)
    }

    /// Every track, in the order they are evicted: outdated ones first,
    /// then those no live copy uses but binge-ahead ones, then the least
    /// recently used. Whether one is being read is the pool's to know; it
    /// skips those.
    pub async fn optimized_tracks(&self) -> Result<Vec<TrackRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            track_columns!(),
            " FROM optimized_tracks t \
             ORDER BY t.outdated DESC, ahead_only DESC, t.last_used_at, t.id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(track_row).collect::<Result<_, _>>()?)
    }

    /// Copy `id` cannot be made, for `failure`: its file could not be
    /// read to say which tracks it needs.
    pub async fn optimized_copy_failed(&self, id: &str, failure: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE optimized_copies SET failure = ?2 WHERE id = ?1")
            .bind(id)
            .bind(failure)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Track `id` is being made.
    pub async fn optimized_track_making(&self, id: &str) -> Result<(), StoreError> {
        self.set_track_state(id, CopyState::Making, None, None)
            .await
    }

    /// Track `id`'s file is whole, `size_bytes` long.
    pub async fn optimized_track_ready(&self, id: &str, size_bytes: u64) -> Result<(), StoreError> {
        self.set_track_state(id, CopyState::Ready, Some(size_bytes), None)
            .await
    }

    /// Track `id` could not be made, for `failure`.
    pub async fn optimized_track_failed(&self, id: &str, failure: &str) -> Result<(), StoreError> {
        self.set_track_state(id, CopyState::Failed, None, Some(failure))
            .await
    }

    /// Track `id` waits again: live playback took its place in the
    /// budget, or it failed and is asked for again. It starts over (a
    /// Matroska file half written cannot be resumed).
    pub async fn optimized_track_requeued(&self, id: &str) -> Result<(), StoreError> {
        self.set_track_state(id, CopyState::Waiting, None, None)
            .await
    }

    async fn set_track_state(
        &self,
        id: &str,
        state: CopyState,
        size_bytes: Option<u64>,
        failure: Option<&str>,
    ) -> Result<(), StoreError> {
        let made_at = (state == CopyState::Ready).then(|| self.now());
        sqlx::query(
            "UPDATE optimized_tracks SET state = ?2, size_bytes = ?3, failure = ?4, \
             made_at = ?5 WHERE id = ?1",
        )
        .bind(id)
        .bind(state.as_str())
        .bind(size_bytes.map(signed))
        .bind(failure)
        .bind(made_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Track `id` was made or read now: the least recently used are
    /// evicted first.
    pub async fn optimized_track_used(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE optimized_tracks SET last_used_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Copy `id` was read now.
    #[cfg(test)]
    pub async fn optimized_copy_used(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE optimized_copies SET last_used_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Copy `id` was lent now, if it is ready and not outdated, nor any
    /// of its tracks: whether it was. It and its tracks count as used.
    pub async fn optimized_copy_used_if_ready(&self, id: &str) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        let lendable: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM optimized_copies c WHERE c.id = ?1 AND c.outdated = 0 \
             AND c.failure IS NULL AND NOT EXISTS (SELECT 1 FROM optimized_copy_tracks m \
               JOIN optimized_tracks t ON t.id = m.track_id \
               WHERE m.copy_id = c.id AND (t.state != 'ready' OR t.outdated = 1))",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if lendable.is_none() {
            return Ok(false);
        }
        let now = self.now();
        sqlx::query("UPDATE optimized_copies SET last_used_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE optimized_tracks SET last_used_at = ?2 \
             WHERE id IN (SELECT track_id FROM optimized_copy_tracks WHERE copy_id = ?1)",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Copy `id` is never lent again, nor holds its place: it was
    /// deleted while read, and goes once its readers are done.
    pub async fn outdate_optimized_copy(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE optimized_copies SET outdated = 1 WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Outdate copy `id`'s tracks no other live copy uses, as it is
    /// deleted: their ids.
    pub async fn outdate_unshared_optimized_tracks(
        &self,
        id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "UPDATE optimized_tracks SET outdated = 1 WHERE outdated = 0 \
             AND id IN (SELECT track_id FROM optimized_copy_tracks WHERE copy_id = ?1) \
             AND NOT EXISTS (SELECT 1 FROM optimized_copy_tracks m \
               JOIN optimized_copies c ON c.id = m.copy_id \
               WHERE m.track_id = optimized_tracks.id AND c.id != ?1 AND c.outdated = 0) \
             RETURNING id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids)
    }

    /// Forget copy `id`; its tracks stay, for the pool to judge, and an
    /// on-demand request's item made into it has none now. Whether it was
    /// there.
    pub async fn delete_optimized_copy(&self, id: &str) -> Result<bool, StoreError> {
        let mut tx = self.begin_write().await?;
        requests::forget_copy(&mut tx, id).await?;
        sqlx::query("DELETE FROM optimized_copy_tracks WHERE copy_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let gone = sqlx::query("DELETE FROM optimized_copies WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(gone.rows_affected() > 0)
    }

    /// Forget track `id`, and every copy made with it, which is no longer
    /// whole; its file is the caller's to remove. The copies' ids.
    pub async fn delete_optimized_track(&self, id: &str) -> Result<Vec<String>, StoreError> {
        let mut tx = self.begin_write().await?;
        let copies: Vec<String> =
            sqlx::query_scalar("SELECT copy_id FROM optimized_copy_tracks WHERE track_id = ?1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for copy in &copies {
            requests::forget_copy(&mut tx, copy).await?;
            sqlx::query("DELETE FROM optimized_copy_tracks WHERE copy_id = ?1")
                .bind(copy)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM optimized_copies WHERE id = ?1")
                .bind(copy)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM optimized_tracks WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(copies)
    }

    /// Tracks a stopped server was making wait again, to be made from
    /// the start: how many.
    pub async fn requeue_interrupted_optimized_tracks(&self) -> Result<u64, StoreError> {
        let done =
            sqlx::query("UPDATE optimized_tracks SET state = 'waiting' WHERE state = 'making'")
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected())
    }
}

/// Mark profile `name`'s live copies outdated, and the video tracks no
/// live copy uses after that: how many copies.
async fn outdate_profile_copies(
    tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>,
    name: &str,
) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE optimized_copies SET outdated = 1 \
         WHERE target = 'profile:' || ?1 AND outdated = 0",
    )
    .bind(name)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE optimized_tracks SET outdated = 1 WHERE outdated = 0 AND recipe LIKE 'video:%' \
         AND id IN (SELECT m.track_id FROM optimized_copy_tracks m \
           JOIN optimized_copies c ON c.id = m.copy_id WHERE c.target = 'profile:' || ?1) \
         AND NOT EXISTS (SELECT 1 FROM optimized_copy_tracks m \
           JOIN optimized_copies c ON c.id = m.copy_id \
           WHERE m.track_id = optimized_tracks.id AND c.outdated = 0)",
    )
    .bind(name)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_store;
    use super::*;

    fn mobile() -> Profile {
        Profile {
            name: "Mobile".into(),
            video_codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: 4_000_000,
        }
    }

    fn mobile_video() -> CopyVideo {
        CopyVideo {
            codec: VideoCodec::H264,
            height: 1080,
            max_bitrate: Some(4_000_000),
            tone_map: false,
            copied: false,
        }
    }

    fn film(target: CopyTarget) -> NewCopy {
        NewCopy {
            media_id: "plex:movie:1".into(),
            media_path: "/films/Alien.mkv".into(),
            source_stamp: "100 5".into(),
            target,
            video: mobile_video(),
        }
    }

    fn in_mobile() -> NewCopy {
        film(CopyTarget::Profile("Mobile".into()))
    }

    #[tokio::test]
    async fn the_server_ships_four_profiles() {
        let store = test_store("optimized-shipped").await;
        let names: Vec<_> = store
            .optimized_profiles()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Mobile", "Mobile (H.264)", "TV", "Compact 4K"]);
    }

    #[tokio::test]
    async fn mobile_ships_as_hevc() {
        let store = test_store("optimized-shipped-mobile").await;
        assert_eq!(
            store.optimized_profile("Mobile").await.unwrap(),
            Some(Profile {
                video_codec: VideoCodec::Hevc,
                ..mobile()
            })
        );
    }

    #[tokio::test]
    async fn mobile_h264_ships_as_mobile_was() {
        let store = test_store("optimized-shipped-mobile-h264").await;
        assert_eq!(
            store.optimized_profile("Mobile (H.264)").await.unwrap(),
            Some(Profile {
                name: "Mobile (H.264)".into(),
                ..mobile()
            })
        );
    }

    /// A server from before migration 0042: `Mobile`'s H.264 copies are
    /// `Mobile (H.264)`'s after it, not made again.
    #[tokio::test]
    async fn the_migration_keeps_mobile_copies_as_mobile_h264() {
        let pool = before_migration("optimized-mobile-migrated", 42).await;
        sqlx::query(
            "INSERT INTO optimized_copies (id, media_id, media_path, source_stamp, target, \
             video_codec, height, max_bitrate, tone_map, requested_at, last_used_at) \
             VALUES ('c1', 'plex:movie:1', '/films/Alien.mkv', '100 5', 'profile:Mobile', \
             'h264', 1080, 4000000, 0, 1, 1)",
        )
        .execute(&pool.0)
        .await
        .unwrap();
        let store = pool.open().await;
        let target: String =
            sqlx::query_scalar("SELECT target FROM optimized_copies WHERE id = 'c1'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(target, "profile:Mobile (H.264)");
    }

    /// An admin's own `Mobile` is theirs: migration 0042 leaves it.
    #[tokio::test]
    async fn the_migration_keeps_an_edited_mobile() {
        let pool = before_migration("optimized-mobile-edited", 42).await;
        sqlx::query("UPDATE optimized_profiles SET height = 720 WHERE name = 'Mobile'")
            .execute(&pool.0)
            .await
            .unwrap();
        let store = pool.open().await;
        assert_eq!(
            store.optimized_profile("Mobile").await.unwrap(),
            Some(Profile {
                height: 720,
                ..mobile()
            })
        );
    }

    /// A database with every migration before `version`, to open as a
    /// store once a test has put its rows in.
    struct Before(sqlx::SqlitePool, spoolframe_test_dir::TestDir);

    impl Before {
        async fn open(self) -> Store {
            self.0.close().await;
            Store::open(self.1.as_ref()).await.unwrap()
        }
    }

    async fn before_migration(test: &str, version: i64) -> Before {
        let dir = super::super::tests::state_dir(test).await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < version)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        Before(pool, dir)
    }

    #[tokio::test]
    async fn a_new_profile_is_listed_last() {
        let store = test_store("optimized-new-profile").await;
        let tablet = Profile {
            name: "Tablet".into(),
            height: 720,
            ..mobile()
        };
        let written = store.put_optimized_profile(&tablet).await.unwrap();
        assert!(written.created);
        let last = store.optimized_profiles().await.unwrap().pop();
        assert_eq!(last, Some(tablet));
    }

    #[tokio::test]
    async fn the_second_request_for_a_copy_gets_the_first() {
        let store = test_store("optimized-one-copy").await;
        let (first, new) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        assert!(new);
        let (second, new) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        assert!(!new);
        assert_eq!(second.id, first.id);
    }

    #[tokio::test]
    async fn a_new_copy_waits() {
        let store = test_store("optimized-waits").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        assert_eq!(copy.state, CopyState::Waiting);
        assert_eq!(store.optimized_copy(&copy.id).await.unwrap(), Some(copy));
    }

    #[tokio::test]
    async fn a_changed_file_outdates_its_copy_and_gets_a_new_one() {
        let store = test_store("optimized-changed-file").await;
        let (old, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let changed = NewCopy {
            source_stamp: "200 9".into(),
            ..in_mobile()
        };
        let (new, created) = store.request_optimized_copy(&changed, &[]).await.unwrap();
        assert!(created);
        assert_ne!(new.id, old.id);
        assert!(
            store
                .optimized_copy(&old.id)
                .await
                .unwrap()
                .unwrap()
                .outdated
        );
    }

    #[tokio::test]
    async fn editing_a_profiles_video_outdates_its_copies() {
        let store = test_store("optimized-edit-outdates").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let faster = Profile {
            max_bitrate: 6_000_000,
            ..mobile()
        };
        let written = store.put_optimized_profile(&faster).await.unwrap();
        assert_eq!(written.outdated, 1);
        assert!(
            store
                .optimized_copy(&copy.id)
                .await
                .unwrap()
                .unwrap()
                .outdated
        );
    }

    #[tokio::test]
    async fn writing_a_profile_unchanged_outdates_nothing() {
        let store = test_store("optimized-edit-same").await;
        store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let shipped = store.optimized_profile("Mobile").await.unwrap().unwrap();
        let written = store.put_optimized_profile(&shipped).await.unwrap();
        assert_eq!(
            written,
            ProfileWritten {
                created: false,
                changed: false,
                outdated: 0
            }
        );
    }

    #[tokio::test]
    async fn after_an_edit_a_request_makes_a_new_copy() {
        let store = test_store("optimized-edit-new-copy").await;
        let (old, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        store
            .put_optimized_profile(&Profile {
                height: 720,
                ..mobile()
            })
            .await
            .unwrap();
        let (new, created) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        assert!(created);
        assert_ne!(new.id, old.id);
    }

    #[tokio::test]
    async fn deleting_a_profile_outdates_its_copies() {
        let store = test_store("optimized-delete-profile").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        assert_eq!(
            store.delete_optimized_profile("Mobile").await.unwrap(),
            Some(1)
        );
        assert!(
            store
                .optimized_copy(&copy.id)
                .await
                .unwrap()
                .unwrap()
                .outdated
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_profile_says_so() {
        let store = test_store("optimized-delete-missing").await;
        assert_eq!(store.delete_optimized_profile("Nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn ahead_copies_with_different_plans_are_two_copies() {
        let store = test_store("optimized-ahead-plans").await;
        let (a, _) = store
            .request_optimized_copy(&film(CopyTarget::Ahead), &[])
            .await
            .unwrap();
        let other_plan = NewCopy {
            video: CopyVideo {
                tone_map: true,
                ..mobile_video()
            },
            ..film(CopyTarget::Ahead)
        };
        let (b, created) = store
            .request_optimized_copy(&other_plan, &[])
            .await
            .unwrap();
        assert!(created);
        assert_ne!(a.id, b.id);
    }

    /// The E-AC-3 a DTS track 1 becomes, 5.1 at 640 kbit/s.
    const EAC3: Recipe = Recipe::Audio(AudioRecipe {
        stream: 1,
        codec: AudioCodec::Eac3,
        channels: 6,
        bitrate: 640_000,
    });

    /// [`in_mobile`]'s tracks: its video, and track 1 converted.
    fn mobile_tracks() -> [Recipe; 2] {
        [Recipe::Video(mobile_video()), EAC3]
    }

    #[tokio::test]
    async fn a_copy_is_ready_once_each_of_its_tracks_is() {
        let store = test_store("optimized-ready").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let tracks = store.optimized_copy_tracks(&copy.id).await.unwrap();
        store
            .optimized_track_ready(&tracks[0].id, 1_000)
            .await
            .unwrap();
        assert!(
            store
                .ready_optimized_copies("/films/Alien.mkv")
                .await
                .unwrap()
                .is_empty()
        );
        store
            .optimized_track_ready(&tracks[1].id, 234)
            .await
            .unwrap();
        let ready = store
            .ready_optimized_copies("/films/Alien.mkv")
            .await
            .unwrap();
        assert_eq!(
            ready.iter().map(|c| c.size_bytes).collect::<Vec<_>>(),
            [Some(1_234)]
        );
    }

    #[tokio::test]
    async fn a_copy_with_no_track_to_store_is_ready_at_once() {
        let store = test_store("optimized-nothing-stored").await;
        let (copy, _) = store
            .request_optimized_copy(&for_apple(), &[])
            .await
            .unwrap();
        assert_eq!((copy.state, copy.size_bytes), (CopyState::Ready, Some(0)));
    }

    #[tokio::test]
    async fn a_track_two_copies_need_is_one_track() {
        let store = test_store("optimized-shared-track").await;
        let (mobile, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let (apple, _) = store
            .request_optimized_copy(&for_apple(), &[EAC3])
            .await
            .unwrap();
        let shared =
            |tracks: Vec<TrackRow>| tracks.into_iter().find(|t| t.recipe == EAC3).map(|t| t.id);
        assert_eq!(
            shared(store.optimized_copy_tracks(&mobile.id).await.unwrap()),
            shared(store.optimized_copy_tracks(&apple.id).await.unwrap())
        );
    }

    /// The list of copies carries each one's own tracks, in its order,
    /// as asking for one copy's does.
    #[tokio::test]
    async fn listed_copies_carry_their_own_tracks_in_order() {
        let store = test_store("optimized-listed-tracks").await;
        store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        store
            .request_optimized_copy(&for_apple(), &[EAC3])
            .await
            .unwrap();
        let mut each = Vec::new();
        for copy in store.optimized_copies().await.unwrap() {
            each.push((
                copy.tracks,
                store.optimized_copy_tracks(&copy.id).await.unwrap(),
            ));
        }
        assert!(
            each.len() == 2
                && each
                    .iter()
                    .all(|(listed, own)| !own.is_empty() && listed == own),
            "{each:?}"
        );
    }

    #[tokio::test]
    async fn a_recipe_reads_back_as_it_was_written() {
        for recipe in mobile_tracks() {
            assert_eq!(Recipe::parse(&recipe.key()).unwrap(), recipe);
        }
    }

    #[tokio::test]
    async fn an_edit_of_a_profile_outdates_its_video_track_not_its_audio() {
        let store = test_store("optimized-edit-video-only").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        store
            .put_optimized_profile(&Profile {
                max_bitrate: 6_000_000,
                ..mobile()
            })
            .await
            .unwrap();
        let outdated: Vec<_> = store
            .optimized_copy_tracks(&copy.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.recipe, t.outdated))
            .collect();
        assert_eq!(
            outdated,
            [(Recipe::Video(mobile_video()), true), (EAC3, false)]
        );
    }

    #[tokio::test]
    async fn an_edit_keeps_a_video_track_another_copy_uses() {
        let store = test_store("optimized-edit-shared-video").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        store
            .request_optimized_copy(&film(CopyTarget::Ahead), &mobile_tracks())
            .await
            .unwrap();
        store.delete_optimized_profile("Mobile").await.unwrap();
        let tracks = store.optimized_copy_tracks(&copy.id).await.unwrap();
        assert!(!tracks[0].outdated);
    }

    #[tokio::test]
    async fn a_changed_file_outdates_its_tracks() {
        let store = test_store("optimized-changed-tracks").await;
        let (old, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let changed = NewCopy {
            source_stamp: "200 9".into(),
            ..in_mobile()
        };
        store
            .request_optimized_copy(&changed, &mobile_tracks())
            .await
            .unwrap();
        let tracks = store.optimized_copy_tracks(&old.id).await.unwrap();
        assert!(tracks.iter().all(|t| t.outdated));
    }

    #[tokio::test]
    async fn tracks_only_ahead_copies_use_are_evicted_before_the_others() {
        let store = test_store("optimized-track-order").await;
        let clock = store.clock().clone();
        store
            .request_optimized_copy(&in_mobile(), &[EAC3])
            .await
            .unwrap();
        clock.advance(std::time::Duration::from_secs(60));
        let ahead_video = Recipe::Video(CopyVideo {
            height: 720,
            ..mobile_video()
        });
        store
            .request_optimized_copy(&film(CopyTarget::Ahead), &[ahead_video])
            .await
            .unwrap();
        let order: Vec<_> = store
            .optimized_tracks()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.recipe)
            .collect();
        assert_eq!(order, [ahead_video, EAC3]);
    }

    #[tokio::test]
    async fn an_evicted_track_takes_the_copies_made_with_it() {
        let store = test_store("optimized-track-evicted").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let video = &store.optimized_copy_tracks(&copy.id).await.unwrap()[0];
        assert_eq!(
            store.delete_optimized_track(&video.id).await.unwrap(),
            std::slice::from_ref(&copy.id)
        );
        assert_eq!(store.optimized_copy(&copy.id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_copy_whose_track_failed_says_why() {
        let store = test_store("optimized-failed").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let track = &store.optimized_copy_tracks(&copy.id).await.unwrap()[1];
        store
            .optimized_track_failed(&track.id, "ffmpeg exited 1")
            .await
            .unwrap();
        let failed = store.optimized_copy(&copy.id).await.unwrap().unwrap();
        assert_eq!(
            (failed.state, failed.failure.as_deref()),
            (CopyState::Failed, Some("ffmpeg exited 1"))
        );
    }

    #[tokio::test]
    async fn an_outdated_copy_is_not_offered_to_the_planner() {
        let store = test_store("optimized-outdated-not-offered").await;
        store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        store.delete_optimized_profile("Mobile").await.unwrap();
        assert!(
            store
                .ready_optimized_copies("/films/Alien.mkv")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn copies_are_listed_outdated_then_ahead_then_least_recently_used() {
        let store = test_store("optimized-eviction-order").await;
        let clock = store.clock().clone();
        let (used_first, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let (used_last, _) = store
            .request_optimized_copy(
                &NewCopy {
                    media_path: "/films/Heat.mkv".into(),
                    ..in_mobile()
                },
                &[],
            )
            .await
            .unwrap();
        let (ahead, _) = store
            .request_optimized_copy(&film(CopyTarget::Ahead), &[])
            .await
            .unwrap();
        let (outdated, _) = store
            .request_optimized_copy(&film(CopyTarget::Profile("TV".into())), &[])
            .await
            .unwrap();
        store.delete_optimized_profile("TV").await.unwrap();
        clock.advance(std::time::Duration::from_secs(60));
        store.optimized_copy_used(&used_last.id).await.unwrap();
        let order: Vec<_> = store
            .optimized_copies()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, [outdated.id, ahead.id, used_first.id, used_last.id]);
    }

    /// The film's own video, copied, for Apple (PX-38).
    fn for_apple() -> NewCopy {
        NewCopy {
            video: CopyVideo {
                codec: VideoCodec::Hevc,
                height: 2160,
                max_bitrate: None,
                tone_map: false,
                copied: true,
            },
            ..film(CopyTarget::Original {
                class: DeviceClass::Apple,
            })
        }
    }

    #[tokio::test]
    async fn an_original_copy_reads_back_as_it_was_asked_for() {
        let store = test_store("optimized-original-round-trip").await;
        let (copy, _) = store
            .request_optimized_copy(&for_apple(), &[])
            .await
            .unwrap();
        let read = store.optimized_copy(&copy.id).await.unwrap().unwrap();
        assert_eq!(
            (read.target, read.video, read.keep_audio),
            (
                for_apple().target,
                for_apple().video,
                Some(DeviceClass::Apple.keep_audio())
            )
        );
    }

    #[tokio::test]
    async fn an_original_copy_s_target_names_its_class_and_copied_video() {
        let store = test_store("optimized-original-key").await;
        store
            .request_optimized_copy(&for_apple(), &[])
            .await
            .unwrap();
        let key: String = sqlx::query_scalar("SELECT target FROM optimized_copies")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(key, "original:apple:copy");
    }

    #[tokio::test]
    async fn a_track_s_copy_reads_back_with_its_class_and_quality() {
        let store = test_store("optimized-music-round-trip").await;
        let asked = NewCopy {
            target: CopyTarget::Music {
                class: DeviceClass::Linux,
                quality: MusicQuality::Aac256,
            },
            video: crate::downloads::music::NO_VIDEO,
            ..for_apple()
        };
        let (copy, _) = store.request_optimized_copy(&asked, &[]).await.unwrap();
        let read = store.optimized_copy(&copy.id).await.unwrap().unwrap();
        let key: String = sqlx::query_scalar("SELECT target FROM optimized_copies")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            (read.target, read.video, key.as_str()),
            (asked.target, asked.video, "music:linux:aac256")
        );
    }

    #[tokio::test]
    async fn an_original_copy_for_each_class_is_two_copies() {
        let store = test_store("optimized-original-classes").await;
        let (apple, _) = store
            .request_optimized_copy(&for_apple(), &[])
            .await
            .unwrap();
        let for_linux = NewCopy {
            target: CopyTarget::Original {
                class: DeviceClass::Linux,
            },
            ..for_apple()
        };
        let (linux, created) = store.request_optimized_copy(&for_linux, &[]).await.unwrap();
        assert!(created);
        assert_ne!(apple.id, linux.id);
    }

    #[tokio::test]
    async fn a_profile_copy_keeps_px_37_s_audio() {
        let store = test_store("optimized-profile-keep").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let read = store.optimized_copy(&copy.id).await.unwrap().unwrap();
        assert_eq!(read.keep_audio, None);
    }

    #[tokio::test]
    async fn original_copies_are_evicted_after_ahead_ones_by_least_recent_use() {
        let store = test_store("optimized-original-eviction").await;
        let clock = store.clock().clone();
        let (original, _) = store
            .request_optimized_copy(&for_apple(), &[])
            .await
            .unwrap();
        let (profile, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        let (ahead, _) = store
            .request_optimized_copy(&film(CopyTarget::Ahead), &[])
            .await
            .unwrap();
        clock.advance(std::time::Duration::from_secs(60));
        store.optimized_copy_used(&original.id).await.unwrap();
        let order: Vec<_> = store
            .optimized_copies()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, [ahead.id, profile.id, original.id]);
    }

    #[tokio::test]
    async fn a_track_a_stopped_server_was_making_waits_again() {
        let store = test_store("optimized-interrupted").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &mobile_tracks())
            .await
            .unwrap();
        let track = &store.optimized_copy_tracks(&copy.id).await.unwrap()[0];
        store.optimized_track_making(&track.id).await.unwrap();
        assert_eq!(
            store.requeue_interrupted_optimized_tracks().await.unwrap(),
            1
        );
        assert_eq!(
            store
                .optimized_track(&track.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            CopyState::Waiting
        );
    }

    /// Migration 0039 drops 0.13's whole-file copies: they are a cache,
    /// and the pool removes their files at its next start.
    #[tokio::test]
    async fn the_migration_drops_whole_file_copies() {
        let dir = super::super::tests::state_dir("optimized-whole-files-dropped").await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.as_ref().join(super::super::DB_FILE))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .iter()
            .filter(|m| m.version < 39)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        before.run(&pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO optimized_copies (id, media_id, media_path, source_stamp, target, \
               video_codec, height, max_bitrate, tone_map, state, size_bytes, requested_at, \
               made_at, last_used_at) VALUES ('c1', 'plex:movie:1', '/films/Alien.mkv', \
               '100 5', 'profile:Mobile', 'h264', 1080, 4000000, 0, 'ready', 1234, 1, 2, 3)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let store = Store::open(dir.as_ref()).await.unwrap();
        assert!(store.optimized_copies().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_deleted_copy_is_gone() {
        let store = test_store("optimized-delete-copy").await;
        let (copy, _) = store
            .request_optimized_copy(&in_mobile(), &[])
            .await
            .unwrap();
        assert!(store.delete_optimized_copy(&copy.id).await.unwrap());
        assert_eq!(store.optimized_copy(&copy.id).await.unwrap(), None);
    }

    #[test]
    fn a_profile_in_vp9_is_refused() {
        let vp9 = Profile {
            video_codec: VideoCodec::Vp9,
            ..mobile()
        };
        assert!(vp9.problem().is_some());
    }

    #[test]
    fn a_profile_of_a_million_lines_is_refused() {
        let tall = Profile {
            height: 1_000_000,
            ..mobile()
        };
        assert!(tall.problem().is_some());
    }

    #[test]
    fn a_profile_without_a_name_is_refused() {
        let nameless = Profile {
            name: " ".into(),
            ..mobile()
        };
        assert!(nameless.problem().is_some());
    }

    #[test]
    fn the_shipped_mobile_profile_is_fine() {
        assert_eq!(mobile().problem(), None);
    }
}
