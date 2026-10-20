//! What Server › Optimized (PX-37) says, apart from the page: profiles as
//! a line and as the edit dialog's fields, the pool's use against its
//! budget, a copy's name and state, a job's time left, and what deleting
//! did.

use spoolframe_admin_api::{
    OptimizedCopy, OptimizedCopyDeleted, OptimizedCopyState, OptimizedDeleted, OptimizedProfile,
};

use crate::catalog::bytes;
use crate::playback::bitrate_text;

/// The codecs a copy is made in, as the edit dialog offers them: the
/// protocol's name and the page's.
pub const CODECS: [(&str, &str); 3] = [("h264", "H.264"), ("hevc", "HEVC"), ("av1", "AV1")];

/// `H.264` for `h264`; a name it does not know as it is.
pub fn codec_label(codec: &str) -> &str {
    CODECS
        .iter()
        .find(|(name, _)| *name == codec)
        .map_or(codec, |(_, label)| label)
}

/// A profile's video: "H.264 · 1080p · 4.0 Mbit/s".
pub fn profile_line(profile: &OptimizedProfile) -> String {
    format!(
        "{} · {}p · {}",
        codec_label(&profile.video_codec),
        profile.height,
        bitrate_text(profile.max_bitrate)
    )
}

/// Bits per second as the dialog's Mbit/s field shows them: `4`, `2.5`,
/// `0.75`.
pub fn mbit_field(bits: u64) -> String {
    let text = format!("{:.3}", bits as f64 / 1e6);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The dialog's Mbit/s field, in bits per second; why not, said for the
/// admin.
pub fn parse_mbit(text: &str) -> Result<u64, String> {
    let typed = text.trim().replace(',', ".");
    match typed.parse::<f64>() {
        Ok(mbit) if mbit.is_finite() && mbit > 0.0 => Ok((mbit * 1e6).round() as u64),
        _ => Err("Type the bitrate in Mbit/s: 4, or 2.5.".to_owned()),
    }
}

/// The dialog's height field (`1080`, or `1080p`), in lines.
pub fn parse_height(text: &str) -> Result<u32, String> {
    let typed = text.trim();
    typed
        .strip_suffix(['p', 'P'])
        .unwrap_or(typed)
        .parse::<u32>()
        .map_err(|_| "Type the height in lines: 1080, or 2160.".to_owned())
}

/// The usage bar: how full it is (0 to 100), its words ("41.5 GB of
/// 100 GB"), and its colour, yellow from 90 %.
pub fn usage(used: u64, max: u64) -> (u32, String, &'static str) {
    let percent = used
        .saturating_mul(100)
        .checked_div(max)
        .map_or(100, |p| p.min(100) as u32);
    let tone = if percent >= 90 {
        "act-yellow"
    } else {
        "act-teal"
    };
    (percent, format!("{} of {}", bytes(used), bytes(max)), tone)
}

/// What a copy is of: the item's title and, under it, its episode's
/// title or else the file's name; the file's name alone when the server
/// could not name the item.
pub fn copy_name(copy: &OptimizedCopy) -> (String, String) {
    let file = file_name(&copy.path).to_owned();
    match (&copy.title, &copy.subtitle) {
        (Some(title), Some(sub)) => (title.clone(), sub.clone()),
        (Some(title), None) => (title.clone(), file),
        (None, _) => (file, copy.media_id.clone()),
    }
}

/// The last part of `path`.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A copy's state, and its colour.
pub fn state_text(state: OptimizedCopyState) -> (&'static str, &'static str) {
    match state {
        OptimizedCopyState::Waiting => ("Waiting", "text-ctp-subtext0"),
        OptimizedCopyState::Making => ("Making", "text-ctp-blue"),
        OptimizedCopyState::Ready => ("Ready", "text-ctp-green"),
        OptimizedCopyState::Failed => ("Failed", "text-ctp-red"),
    }
}

/// How many live copies `profile` has: those not outdated.
pub fn copies_of(copies: &[OptimizedCopy], profile: &str) -> usize {
    copies
        .iter()
        .filter(|c| c.target == profile && !c.outdated)
        .count()
}

/// A copy's size: its file's, or a dash before it is made.
pub fn size_text(copy: &OptimizedCopy) -> String {
    copy.size_bytes.map_or_else(|| "—".to_owned(), bytes)
}

/// How long a job has to go: "12 s left", "4 min left", "1 h 5 min left".
pub fn eta_text(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s left"),
        60..3600 => format!("{} min left", secs / 60),
        _ if secs % 3600 < 60 => format!("{} h left", secs / 3600),
        _ => format!("{} h {} min left", secs / 3600, secs % 3600 / 60),
    }
}

/// A job's progress, 0 to 100, once its length is known.
pub fn job_percent(fraction: Option<f64>) -> Option<u32> {
    fraction.map(|f| (f.clamp(0.0, 1.0) * 100.0).round() as u32)
}

/// What the edit dialog says once a change to `profile`'s video outdated
/// `n` of its copies; nothing when none were.
pub fn outdated_message(profile: &str, n: u64) -> Option<String> {
    match n {
        0 => None,
        1 => Some(format!(
            "1 copy made for {profile} is outdated now: it is never played or downloaded again, and goes first when room is needed."
        )),
        n => Some(format!(
            "{n} copies made for {profile} are outdated now: they are never played or downloaded again, and go first when room is needed."
        )),
    }
}

/// The toast after "Delete them now".
pub fn outdated_deleted_text(done: OptimizedDeleted) -> String {
    let now = match done.now {
        0 => "No outdated copy was deleted".to_owned(),
        1 => "1 outdated copy deleted".to_owned(),
        n => format!("{n} outdated copies deleted"),
    };
    match done.deferred {
        0 => now,
        1 => format!("{now}; 1 being read goes when its reader is done"),
        n => format!("{now}; {n} being read go when their readers are done"),
    }
}

/// The toast after a copy's Delete.
pub fn copy_deleted_text(deleted: OptimizedCopyDeleted) -> &'static str {
    match deleted {
        OptimizedCopyDeleted::Now => "Copy deleted",
        OptimizedCopyDeleted::Deferred => {
            "The copy goes once it is not being read or made: nobody starts reading it from now on"
        }
        OptimizedCopyDeleted::NotFound => "That copy was gone already",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(title: Option<&str>, subtitle: Option<&str>) -> OptimizedCopy {
        OptimizedCopy {
            id: "c1".into(),
            media_id: "plex:movie:1".into(),
            title: title.map(Into::into),
            subtitle: subtitle.map(Into::into),
            path: "/media/films/Alien (1979).mkv".into(),
            target: "Mobile".into(),
            outdated: false,
            state: OptimizedCopyState::Ready,
            size_bytes: Some(2_400_000_000),
            failure: None,
            reading: false,
            requested_at: 0,
            made_at: Some(0),
            last_used_at: 0,
            progress: None,
        }
    }

    /// PX-37.
    #[test]
    fn a_profile_reads_as_codec_height_and_bitrate() {
        let mobile = OptimizedProfile {
            name: "Mobile".into(),
            video_codec: "h264".into(),
            height: 1080,
            max_bitrate: 4_000_000,
        };
        assert_eq!(profile_line(&mobile), "H.264 · 1080p · 4.0 Mbit/s");
    }

    /// PX-37: what the dialog shows is what it reads back.
    #[test]
    fn the_bitrate_field_is_in_mbit_and_reads_back() {
        assert_eq!(
            [4_000_000, 2_500_000, 750_000].map(mbit_field),
            ["4", "2.5", "0.75"]
        );
        assert_eq!(
            ["4", " 2.5 ", "0,75"].map(|t| parse_mbit(t).unwrap()),
            [4_000_000, 2_500_000, 750_000]
        );
    }

    /// PX-37.
    #[test]
    fn a_bitrate_that_is_no_number_is_refused() {
        assert!(parse_mbit("fast").is_err() && parse_mbit("0").is_err());
    }

    /// PX-37.
    #[test]
    fn a_height_may_be_typed_with_its_p() {
        assert_eq!(
            (parse_height("1080p"), parse_height("2160")),
            (Ok(1080), Ok(2160))
        );
    }

    /// PX-37.
    #[test]
    fn the_usage_bar_turns_yellow_near_the_budget() {
        assert_eq!(
            usage(41_500_000_000, 100_000_000_000),
            (41, "41.5 GB of 100 GB".to_owned(), "act-teal")
        );
        assert_eq!(usage(95, 100).2, "act-yellow");
    }

    /// PX-37: over the budget (a copy being made counts at what it is
    /// expected to grow to) the bar is full, not past its end.
    #[test]
    fn the_usage_bar_stops_at_full() {
        assert_eq!(usage(150, 100).0, 100);
    }

    /// PX-37.
    #[test]
    fn a_copy_is_named_by_its_item_or_else_its_file() {
        assert_eq!(
            copy_name(&copy(Some("Alien"), None)),
            ("Alien".to_owned(), "Alien (1979).mkv".to_owned())
        );
        assert_eq!(
            copy_name(&copy(Some("Severance · S2 E4"), Some("Woe's Hollow"))),
            ("Severance · S2 E4".to_owned(), "Woe's Hollow".to_owned())
        );
        assert_eq!(
            copy_name(&copy(None, None)).0,
            "Alien (1979).mkv".to_owned()
        );
    }

    /// PX-37: a profile's count leaves out the copies its edit outdated.
    #[test]
    fn a_profile_counts_its_live_copies() {
        let outdated = OptimizedCopy {
            outdated: true,
            ..copy(None, None)
        };
        let tv = OptimizedCopy {
            target: "TV".into(),
            ..copy(None, None)
        };
        assert_eq!(copies_of(&[copy(None, None), outdated, tv], "Mobile"), 1);
    }

    /// PX-37.
    #[test]
    fn a_job_s_time_left_is_in_the_unit_that_fits() {
        assert_eq!(
            [12, 250, 3600, 3900].map(eta_text),
            ["12 s left", "4 min left", "1 h left", "1 h 5 min left"]
        );
    }

    /// PX-37.
    #[test]
    fn a_job_s_progress_is_a_whole_percent() {
        assert_eq!(
            (job_percent(Some(0.416)), job_percent(None)),
            (Some(42), None)
        );
    }

    /// PX-37: the edit dialog offers "Delete them now" only when copies
    /// were outdated.
    #[test]
    fn the_edit_says_how_many_copies_it_outdated() {
        assert_eq!(outdated_message("Mobile", 0), None);
        assert_eq!(
            outdated_message("Mobile", 3).unwrap(),
            "3 copies made for Mobile are outdated now: they are never played or downloaded again, and go first when room is needed."
        );
        assert!(
            outdated_message("TV", 1)
                .unwrap()
                .starts_with("1 copy made for TV is")
        );
    }

    /// PX-37.
    #[test]
    fn delete_them_now_says_what_waits_for_its_readers() {
        assert_eq!(
            outdated_deleted_text(OptimizedDeleted {
                now: 2,
                deferred: 1
            }),
            "2 outdated copies deleted; 1 being read goes when its reader is done"
        );
    }
}
