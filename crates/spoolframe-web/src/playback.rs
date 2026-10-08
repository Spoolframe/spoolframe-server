//! What Server › Playback and an item's Debug tab decide (P3-15), apart
//! from the pages: how a plan, a stream, a read and an end read.
//! Host-compiled, so `cargo test` covers it.

use spoolframe_admin_api::{
    CacheState, EndedWhy, PlanKind, PlaybackRow, PreviewCache, ReadMark, ReadPoint, StreamKind,
    StreamOut,
};

use crate::catalog::{bytes, clock};

/// A plan's pill: its word and its colour class.
pub fn plan_pill(kind: PlanKind) -> (&'static str, &'static str) {
    match kind {
        PlanKind::Direct => ("Direct", "act-green"),
        PlanKind::Remux => ("Remux", "act-blue"),
        PlanKind::Transcode => ("Transcode", "act-peach"),
        PlanKind::Unsupported => ("Cannot play", "act-red"),
    }
}

/// How the playback comes in: `Range`, `HLS`, `Progressive`.
pub fn delivery(name: &str) -> String {
    match name {
        "range" => "Range".to_owned(),
        "hls" => "HLS".to_owned(),
        "progressive" => "Progressive".to_owned(),
        "unknown" | "" => "—".to_owned(),
        other => other.to_owned(),
    }
}

/// A codec or format as people write it.
pub fn codec(name: &str) -> String {
    match name {
        "h264" => "H.264",
        "hevc" => "HEVC",
        "av1" => "AV1",
        "vp9" => "VP9",
        "aac" => "AAC",
        "ac3" => "AC-3",
        "eac3" => "E-AC-3",
        "dts" => "DTS",
        "true_hd" => "TrueHD",
        "opus" => "Opus",
        "flac" => "FLAC",
        "srt" => "SRT",
        "web_vtt" => "WebVTT",
        "ass" => "ASS",
        "pgs" => "PGS",
        "vob_sub" => "VobSub",
        "dvb_sub" => "DVB",
        "mov_text" => "mov_text",
        "matroska" => "Matroska",
        "mp4" => "MP4",
        "mpeg_ts" => "MPEG-TS",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// The list's "why" under a plan's pill: the plan's own reasons, as the
/// server wrote them; or that the file plays as it is.
pub fn why_line(row: &PlaybackRow) -> String {
    if row.reasons.is_empty() {
        match row.plan {
            PlanKind::Direct => "The file plays as it is".to_owned(),
            _ => "No reason given".to_owned(),
        }
    } else {
        row.reasons.join(" · ")
    }
}

/// A stream row's name: `Video`, `Audio 1`, `Subtitle 3`, `Container`.
pub fn stream_name(kind: StreamKind, index: Option<u32>) -> String {
    let word = match kind {
        StreamKind::Container => "Container",
        StreamKind::Video => "Video",
        StreamKind::Audio => "Audio",
        StreamKind::Subtitle => "Subtitle",
    };
    match index {
        Some(n) => format!("{word} {n}"),
        None => word.to_owned(),
    }
}

/// What a plan does with a stream, and the colour it is shown in.
pub fn out_text(out: &StreamOut) -> (String, &'static str) {
    match out {
        StreamOut::Direct => ("As it is".to_owned(), "text-ctp-green"),
        StreamOut::Copy { codec: c } => (format!("Copy ({})", codec(c)), "text-ctp-green"),
        StreamOut::Encode {
            codec: c,
            height,
            channels,
            bitrate,
            tone_map,
            encoder,
        } => {
            let mut what = format!("Encode → {}", codec(c));
            if let Some(h) = height {
                what.push_str(&format!(" {h}p"));
            }
            if let Some(ch) = channels {
                what.push_str(&format!(" {ch}ch"));
            }
            if let Some(b) = bitrate {
                what.push_str(&format!(", {}", bitrate_text(*b)));
            }
            if *tone_map {
                what.push_str(", tone-mapped");
            }
            if let Some(e) = encoder {
                what.push_str(&format!(" ({e})"));
            }
            (what, "text-ctp-peach")
        }
        StreamOut::ReadAsText { from, to } => (
            format!("Read as text: {} → {}", codec(from), codec(to)),
            "text-ctp-mauve",
        ),
        StreamOut::Convert { from, to } => (
            format!("Convert: {} → {}", codec(from), codec(to)),
            "text-ctp-mauve",
        ),
        StreamOut::BurnIn { format } => (
            format!("Burn into the video ({})", codec(format)),
            "text-ctp-peach",
        ),
        StreamOut::Beside { format } => (
            format!("Beside the file ({})", codec(format)),
            "text-ctp-green",
        ),
        StreamOut::Remux { to } => (format!("Into {}", codec(to)), "text-ctp-blue"),
        StreamOut::Unplayable => ("Cannot play".to_owned(), "text-ctp-red"),
    }
}

/// Bits per second as a person reads it: `8.0 Mbit/s`, `640 kbit/s`.
pub fn bitrate_text(bits: u64) -> String {
    if bits >= 1_000_000 {
        format!("{:.1} Mbit/s", bits as f64 / 1e6)
    } else {
        format!("{} kbit/s", bits / 1000)
    }
}

/// How a playback ended, and whether that is a failure.
pub fn ended_text(why: &EndedWhy) -> (String, bool) {
    match why {
        EndedWhy::Finished => ("Stopped by the app".to_owned(), false),
        EndedWhy::Stopped => ("Stopped by an admin".to_owned(), false),
        EndedWhy::Idle => ("Expired: no reads for 5 min".to_owned(), false),
        EndedWhy::Replaced => (
            "Replaced by another playback on the device".to_owned(),
            false,
        ),
        EndedWhy::DeviceGone => ("The device went away".to_owned(), false),
        EndedWhy::SignedOut => ("Signed out".to_owned(), false),
        EndedWhy::AccessRemoved => ("Its library was removed".to_owned(), false),
        EndedWhy::Failed { reason } => (format!("Failed: {reason}"), true),
    }
}

/// The furthest read: `byte 1.2 MB` or `segment 321`.
pub fn furthest(mark: Option<ReadMark>) -> String {
    match mark {
        Some(ReadMark::Byte { offset }) => format!("byte {}", bytes(offset)),
        Some(ReadMark::Segment { n }) => format!("segment {n}"),
        None => "nothing read".to_owned(),
    }
}

/// The share of the film converted, 0 to 100, when its length is known.
pub fn converted_percent(converted_ms: u64, duration_ms: Option<u64>) -> Option<u8> {
    let total = duration_ms.filter(|d| *d > 0)?;
    Some((converted_ms.min(total) * 100 / total) as u8)
}

/// A position in the film, or a dash.
pub fn at(ms: Option<u64>) -> String {
    ms.map_or_else(|| "—".to_owned(), clock)
}

/// The reads chart's two lines, `read` and `converted`, as SVG `points`
/// over the `window_ms` before `now_ms`: time across, film time up, both
/// on one scale. `None` with fewer than two reads in the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadsChart {
    pub read: String,
    pub converted: Option<String>,
    /// The highest film time drawn, ms: the top of the chart.
    pub top_ms: u64,
}

pub fn reads_chart(
    points: &[ReadPoint],
    now_ms: i64,
    window_ms: i64,
    width: f64,
    height: f64,
) -> Option<ReadsChart> {
    const PAD: f64 = 2.0;
    let start = now_ms - window_ms;
    let shown: Vec<&ReadPoint> = points
        .iter()
        .filter(|p| p.at >= start && p.at <= now_ms && p.read_ms.is_some())
        .collect();
    if shown.len() < 2 {
        return None;
    }
    let top = shown
        .iter()
        .flat_map(|p| [p.read_ms, p.converted_ms])
        .flatten()
        .max()
        .unwrap_or(0)
        .max(1);
    let xy = |at: i64, ms: u64| {
        let x = (at - start) as f64 / window_ms as f64 * width;
        let y = (height - PAD) - ms as f64 / top as f64 * (height - 2.0 * PAD);
        format!("{x:.1},{y:.1}")
    };
    let read: Vec<String> = shown
        .iter()
        .filter_map(|p| Some(xy(p.at, p.read_ms?)))
        .collect();
    let converted: Vec<String> = shown
        .iter()
        .filter_map(|p| Some(xy(p.at, p.converted_ms?)))
        .collect();
    Some(ReadsChart {
        read: read.join(" "),
        converted: (converted.len() >= 2).then(|| converted.join(" ")),
        top_ms: top,
    })
}

/// A subtitle cache row's state, and its tone.
pub fn cache_text(state: &CacheState) -> (String, &'static str) {
    match state {
        CacheState::NotMade => ("Not made yet".to_owned(), "cat-quiet"),
        CacheState::Waiting => ("Waiting its turn".to_owned(), "state state-warn"),
        CacheState::Making { done_percent } => {
            (format!("Reading · {done_percent} %"), "state text-ctp-blue")
        }
        CacheState::Ready => ("Done".to_owned(), "state state-ok"),
        CacheState::Failed { reason } => (format!("Failed: {reason}"), "state state-bad"),
        CacheState::Unavailable { reason } => (format!("Cannot: {reason}"), "cat-quiet"),
    }
}

/// The previews' state, and its tone.
pub fn previews_text(state: &PreviewCache) -> (String, &'static str) {
    match state {
        PreviewCache::Ready {
            count, interval_ms, ..
        } => (
            format!("{count} frames · every {} s", interval_ms / 1000),
            "state state-ok",
        ),
        PreviewCache::Making { done_percent } => {
            (format!("Making · {done_percent} %"), "state text-ctp-blue")
        }
        PreviewCache::NotMade => (
            "Not made yet: made when first played".to_owned(),
            "cat-quiet",
        ),
        PreviewCache::None { reason } => (format!("None: {reason}"), "cat-quiet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(at: i64, read: u64, converted: Option<u64>) -> ReadPoint {
        ReadPoint {
            at,
            read_ms: Some(read),
            converted_ms: converted,
        }
    }

    #[test]
    fn the_chart_puts_both_lines_on_one_scale_over_the_window() {
        let chart = reads_chart(
            &[
                point(0, 0, Some(1000)),
                point(500, 1000, Some(2000)),
                // Before the window.
                point(-100, 0, None),
            ],
            1000,
            1000,
            100.0,
            22.0,
        )
        .unwrap();
        assert_eq!(chart.top_ms, 2000);
        assert_eq!(chart.read, "0.0,20.0 50.0,11.0");
        assert_eq!(chart.converted.as_deref(), Some("0.0,11.0 50.0,2.0"));
    }

    #[test]
    fn no_chart_without_two_reads() {
        assert_eq!(reads_chart(&[point(0, 0, None)], 10, 100, 1.0, 1.0), None);
    }

    #[test]
    fn an_encode_says_what_it_makes() {
        let (text, _) = out_text(&StreamOut::Encode {
            codec: "h264".into(),
            height: Some(1080),
            channels: None,
            bitrate: Some(8_000_000),
            tone_map: true,
            encoder: Some("h264_vaapi".into()),
        });
        assert_eq!(
            text,
            "Encode → H.264 1080p, 8.0 Mbit/s, tone-mapped (h264_vaapi)"
        );
    }

    #[test]
    fn a_failure_is_said_with_its_reason() {
        assert_eq!(
            ended_text(&EndedWhy::Failed {
                reason: "ffmpeg exited with status 1".into()
            }),
            ("Failed: ffmpeg exited with status 1".to_owned(), true)
        );
    }

    #[test]
    fn the_share_converted_needs_a_length() {
        assert_eq!(converted_percent(30_000, Some(60_000)), Some(50));
        assert_eq!(converted_percent(90_000, Some(60_000)), Some(100));
        assert_eq!(converted_percent(1, None), None);
    }
}
