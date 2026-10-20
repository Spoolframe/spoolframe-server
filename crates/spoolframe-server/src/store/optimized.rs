//! Optimized versions (PX-37): storage. The admin's profiles, and the
//! pool's copies, one row each. Making, serving and evicting the files
//! is the pool's; this module only keeps their records.

use spoolframe_protocol::{ConvertedVideo, PlaybackPlan};
use spoolframe_types::VideoCodec;

use super::{Store, StoreError};

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
    /// Its copies marked outdated: its video changed.
    pub outdated: u64,
}

/// What a copy is made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyTarget {
    /// A profile, by name.
    Profile(String),
    /// Binge-ahead: the next episode, with a playback's own plan.
    Ahead,
}

/// How a copy's video is made: the profile's, or the playback's for an
/// `ahead` copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyVideo {
    pub codec: VideoCodec,
    pub height: u32,
    /// Bits per second; `None` for the encoder's own choice.
    pub max_bitrate: Option<u64>,
    /// HDR mapped to SDR.
    pub tone_map: bool,
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
            }),
            _ => None,
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
        } = video;
        Self::Encode {
            codec,
            height,
            max_bitrate,
            tone_map,
        }
    }
}

/// Where a copy is in its making.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyState {
    /// Asked for, waiting for a place in the encode budget.
    Waiting,
    /// Its ffmpeg is running.
    Making,
    /// Its file is whole.
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

/// A copy's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRow {
    /// The server's own; it names the file.
    pub id: String,
    pub media_id: String,
    pub media_path: String,
    pub source_stamp: String,
    pub target: CopyTarget,
    pub video: CopyVideo,
    pub state: CopyState,
    pub size_bytes: Option<u64>,
    pub failure: Option<String>,
    pub outdated: bool,
    pub requested_at: i64,
    pub made_at: Option<i64>,
    pub last_used_at: i64,
}

/// The `target` column's prefix for a profile's copy, its name after it.
const PROFILE_PREFIX: &str = "profile:";

/// The `target` column's prefix for a binge-ahead copy, its plan after it.
const AHEAD_PREFIX: &str = "ahead:";

/// The `target` column: one string per target, so a media file has one
/// live copy for each. An `ahead` copy is unique by its plan.
fn target_key(target: &CopyTarget, video: &CopyVideo) -> String {
    match target {
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
         max_bitrate, tone_map, state, size_bytes, failure, outdated, requested_at, made_at, \
         last_used_at"
    };
}

fn copy_row(r: &sqlx::sqlite::SqliteRow) -> Result<CopyRow, sqlx::Error> {
    use sqlx::Row;
    let key: String = r.try_get("target")?;
    let target = if let Some(name) = key.strip_prefix(PROFILE_PREFIX) {
        CopyTarget::Profile(name.to_owned())
    } else if key.starts_with(AHEAD_PREFIX) {
        CopyTarget::Ahead
    } else {
        return Err(decode_error("target", &key));
    };
    Ok(CopyRow {
        id: r.try_get("id")?,
        media_id: r.try_get("media_id")?,
        media_path: r.try_get("media_path")?,
        source_stamp: r.try_get("source_stamp")?,
        target,
        video: CopyVideo {
            codec: codec(&r.try_get::<String, _>("video_codec")?)?,
            height: u32::try_from(r.try_get::<i64, _>("height")?).unwrap_or(0),
            max_bitrate: r
                .try_get::<Option<i64>, _>("max_bitrate")?
                .map(|b| unsigned("max_bitrate", b))
                .transpose()?,
            tone_map: r.try_get("tone_map")?,
        },
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

    /// Write `profile` over the one of its name, or add it last. Its
    /// copies are outdated if its video changed: they are never used
    /// again. The caller has checked [`Profile::problem`].
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
                let outdated = if before.video_differs(profile) {
                    outdate_profile_copies(&mut tx, &profile.name).await?
                } else {
                    0
                };
                ProfileWritten {
                    created: false,
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
                    outdated: 0,
                }
            }
        };
        tx.commit().await?;
        Ok(written)
    }

    /// Remove profile `name`, its copies outdated: whether it was there,
    /// and how many it outdated.
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
        tx.commit().await?;
        Ok(Some(outdated))
    }

    /// The live copy for `copy`'s file and target, or a new one waiting
    /// to be made: one copy per file and target, whoever asks. A live
    /// copy of the file as it was (another stamp) is outdated and a new
    /// one asked for. The bool is whether the copy is new.
    pub async fn request_optimized_copy(
        &self,
        copy: &NewCopy,
    ) -> Result<(CopyRow, bool), StoreError> {
        let target = target_key(&copy.target, &copy.video);
        let mut tx = self.begin_write().await?;
        let live = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies \
             WHERE media_path = ?1 AND target = ?2 AND outdated = 0"
        ))
        .bind(&copy.media_path)
        .bind(&target)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(copy_row)
        .transpose()?;
        if let Some(live) = live {
            if live.source_stamp == copy.source_stamp {
                return Ok((live, false));
            }
            sqlx::query("UPDATE optimized_copies SET outdated = 1 WHERE id = ?1")
                .bind(&live.id)
                .execute(&mut *tx)
                .await?;
        }
        let now = self.now();
        let row = CopyRow {
            id: uuid::Uuid::new_v4().simple().to_string(),
            media_id: copy.media_id.clone(),
            media_path: copy.media_path.clone(),
            source_stamp: copy.source_stamp.clone(),
            target: copy.target.clone(),
            video: copy.video,
            state: CopyState::Waiting,
            size_bytes: None,
            failure: None,
            outdated: false,
            requested_at: now,
            made_at: None,
            last_used_at: now,
        };
        sqlx::query(
            "INSERT INTO optimized_copies (id, media_id, media_path, source_stamp, target, \
             video_codec, height, max_bitrate, tone_map, state, requested_at, last_used_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        )
        .bind(&row.id)
        .bind(&row.media_id)
        .bind(&row.media_path)
        .bind(&row.source_stamp)
        .bind(&target)
        .bind(row.video.codec.as_str())
        .bind(i64::from(row.video.height))
        .bind(row.video.max_bitrate.map(signed))
        .bind(row.video.tone_map)
        .bind(row.state.as_str())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((row, true))
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
        Ok(found.as_ref().map(copy_row).transpose()?)
    }

    /// Every copy, in the order they are evicted: outdated ones first,
    /// then `ahead` ones, then the least recently used. Whether one is
    /// being read is the pool's to know; it skips those.
    pub async fn optimized_copies(&self) -> Result<Vec<CopyRow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM optimized_copies \
             ORDER BY outdated DESC, target LIKE 'ahead:%' DESC, last_used_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(copy_row).collect::<Result<_, _>>()?)
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
            " FROM optimized_copies \
             WHERE media_path = ?1 AND outdated = 0 AND state = 'ready' ORDER BY id"
        ))
        .bind(media_path)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(copy_row).collect::<Result<_, _>>()?)
    }

    /// Copy `id` is being made.
    pub async fn optimized_copy_making(&self, id: &str) -> Result<(), StoreError> {
        self.set_copy_state(id, CopyState::Making, None, None).await
    }

    /// Copy `id`'s file is whole, `size_bytes` long.
    pub async fn optimized_copy_ready(&self, id: &str, size_bytes: u64) -> Result<(), StoreError> {
        self.set_copy_state(id, CopyState::Ready, Some(size_bytes), None)
            .await
    }

    /// Copy `id` could not be made, for `failure`.
    pub async fn optimized_copy_failed(&self, id: &str, failure: &str) -> Result<(), StoreError> {
        self.set_copy_state(id, CopyState::Failed, None, Some(failure))
            .await
    }

    /// Copy `id` waits again: live playback took its place in the budget,
    /// and it starts over (an MKV half written cannot be resumed).
    pub async fn optimized_copy_requeued(&self, id: &str) -> Result<(), StoreError> {
        self.set_copy_state(id, CopyState::Waiting, None, None)
            .await
    }

    async fn set_copy_state(
        &self,
        id: &str,
        state: CopyState,
        size_bytes: Option<u64>,
        failure: Option<&str>,
    ) -> Result<(), StoreError> {
        let made_at = (state == CopyState::Ready).then(|| self.now());
        sqlx::query(
            "UPDATE optimized_copies SET state = ?2, size_bytes = ?3, failure = ?4, \
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

    /// Copy `id` was read now: the least recently used are evicted first.
    pub async fn optimized_copy_used(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE optimized_copies SET last_used_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(self.now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Copy `id` was lent now, if it is ready and not outdated: whether
    /// it was. The least recently used are evicted first.
    pub async fn optimized_copy_used_if_ready(&self, id: &str) -> Result<bool, StoreError> {
        let done = sqlx::query(
            "UPDATE optimized_copies SET last_used_at = ?2 \
             WHERE id = ?1 AND state = 'ready' AND outdated = 0",
        )
        .bind(id)
        .bind(self.now())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Forget copy `id`; its file is the caller's to remove. Whether it
    /// was there.
    pub async fn delete_optimized_copy(&self, id: &str) -> Result<bool, StoreError> {
        let gone = sqlx::query("DELETE FROM optimized_copies WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(gone.rows_affected() > 0)
    }

    /// Copies a stopped server was making wait again, to be made from
    /// the start: how many.
    pub async fn requeue_interrupted_optimized_copies(&self) -> Result<u64, StoreError> {
        let done =
            sqlx::query("UPDATE optimized_copies SET state = 'waiting' WHERE state = 'making'")
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected())
    }
}

/// Mark profile `name`'s live copies outdated: how many.
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
    async fn the_server_ships_three_profiles() {
        let store = test_store("optimized-shipped").await;
        let names: Vec<_> = store
            .optimized_profiles()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Mobile", "TV", "Compact 4K"]);
    }

    #[tokio::test]
    async fn a_shipped_profile_is_what_the_ticket_names() {
        let store = test_store("optimized-shipped-mobile").await;
        assert_eq!(
            store.optimized_profile("Mobile").await.unwrap(),
            Some(mobile())
        );
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
        let (first, new) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        assert!(new);
        let (second, new) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        assert!(!new);
        assert_eq!(second.id, first.id);
    }

    #[tokio::test]
    async fn a_new_copy_waits() {
        let store = test_store("optimized-waits").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        assert_eq!(copy.state, CopyState::Waiting);
        assert_eq!(store.optimized_copy(&copy.id).await.unwrap(), Some(copy));
    }

    #[tokio::test]
    async fn a_changed_file_outdates_its_copy_and_gets_a_new_one() {
        let store = test_store("optimized-changed-file").await;
        let (old, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        let changed = NewCopy {
            source_stamp: "200 9".into(),
            ..in_mobile()
        };
        let (new, created) = store.request_optimized_copy(&changed).await.unwrap();
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
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
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
        store.request_optimized_copy(&in_mobile()).await.unwrap();
        let written = store.put_optimized_profile(&mobile()).await.unwrap();
        assert_eq!(
            written,
            ProfileWritten {
                created: false,
                outdated: 0
            }
        );
    }

    #[tokio::test]
    async fn after_an_edit_a_request_makes_a_new_copy() {
        let store = test_store("optimized-edit-new-copy").await;
        let (old, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        store
            .put_optimized_profile(&Profile {
                height: 720,
                ..mobile()
            })
            .await
            .unwrap();
        let (new, created) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        assert!(created);
        assert_ne!(new.id, old.id);
    }

    #[tokio::test]
    async fn deleting_a_profile_outdates_its_copies() {
        let store = test_store("optimized-delete-profile").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
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
            .request_optimized_copy(&film(CopyTarget::Ahead))
            .await
            .unwrap();
        let other_plan = NewCopy {
            video: CopyVideo {
                tone_map: true,
                ..mobile_video()
            },
            ..film(CopyTarget::Ahead)
        };
        let (b, created) = store.request_optimized_copy(&other_plan).await.unwrap();
        assert!(created);
        assert_ne!(a.id, b.id);
    }

    #[tokio::test]
    async fn a_ready_copy_is_offered_to_the_planner() {
        let store = test_store("optimized-ready").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        store.optimized_copy_making(&copy.id).await.unwrap();
        assert!(
            store
                .ready_optimized_copies("/films/Alien.mkv")
                .await
                .unwrap()
                .is_empty()
        );
        store.optimized_copy_ready(&copy.id, 1_234).await.unwrap();
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
    async fn an_outdated_copy_is_not_offered_to_the_planner() {
        let store = test_store("optimized-outdated-not-offered").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        store.optimized_copy_ready(&copy.id, 1).await.unwrap();
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
    async fn a_failed_copy_says_why() {
        let store = test_store("optimized-failed").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        store
            .optimized_copy_failed(&copy.id, "ffmpeg exited 1")
            .await
            .unwrap();
        let failed = store.optimized_copy(&copy.id).await.unwrap().unwrap();
        assert_eq!(
            (failed.state, failed.failure.as_deref()),
            (CopyState::Failed, Some("ffmpeg exited 1"))
        );
    }

    #[tokio::test]
    async fn copies_are_listed_outdated_then_ahead_then_least_recently_used() {
        let store = test_store("optimized-eviction-order").await;
        let clock = store.clock().clone();
        let (used_first, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        let (used_last, _) = store
            .request_optimized_copy(&NewCopy {
                media_path: "/films/Heat.mkv".into(),
                ..in_mobile()
            })
            .await
            .unwrap();
        let (ahead, _) = store
            .request_optimized_copy(&film(CopyTarget::Ahead))
            .await
            .unwrap();
        let (outdated, _) = store
            .request_optimized_copy(&film(CopyTarget::Profile("TV".into())))
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

    #[tokio::test]
    async fn a_copy_a_stopped_server_was_making_waits_again() {
        let store = test_store("optimized-interrupted").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
        store.optimized_copy_making(&copy.id).await.unwrap();
        assert_eq!(
            store.requeue_interrupted_optimized_copies().await.unwrap(),
            1
        );
        assert_eq!(
            store.optimized_copy(&copy.id).await.unwrap().unwrap().state,
            CopyState::Waiting
        );
    }

    #[tokio::test]
    async fn a_deleted_copy_is_gone() {
        let store = test_store("optimized-delete-copy").await;
        let (copy, _) = store.request_optimized_copy(&in_mobile()).await.unwrap();
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
