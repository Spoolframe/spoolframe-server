//! Subtitles from OpenSubtitles (PX-02), as an item's page and Settings ›
//! Subtitles show them: a result's marks, what a download or a refusal
//! says, and the settings' fields, testable on the host.

use serde_json::{Value, json};
use spoolframe_admin_api::{
    SubtitleDownloaded, SubtitleOrigin, SubtitleResult, SubtitleTimingInfo, error_code,
};

use crate::logic::{ApiError, thousands};

/// What a result is marked with, each with its pill's tone: found by the
/// hash first, then what a viewer weighs.
pub fn marks(r: &SubtitleResult) -> Vec<(&'static str, &'static str)> {
    let mut marks = Vec::new();
    if r.hash_match {
        marks.push(("Made for this file", "pill pill-trusted"));
    }
    if r.hearing_impaired {
        marks.push(("SDH", "pill"));
    }
    match r.origin {
        SubtitleOrigin::MachineTranslated => marks.push(("Machine-translated", "pill pill-warn")),
        SubtitleOrigin::Ai => marks.push(("AI-made", "pill pill-warn")),
        SubtitleOrigin::Human => {}
    }
    if r.downloaded {
        marks.push(("Downloaded", "pill pill-trusted"));
    }
    if r.rejected {
        marks.push(("Removed before", "pill pill-bad"));
    }
    marks
}

/// A result's line under its release: language, format, frame rate,
/// rating and downloads.
pub fn details(r: &SubtitleResult) -> String {
    let mut parts = vec![r.language.clone()];
    if let Some(format) = &r.format {
        parts.push(format.to_ascii_uppercase());
    }
    if let Some(fps) = r.fps_milli {
        parts.push(format!("{} fps", fps_text(fps)));
    }
    parts.push(match r.rating {
        Some(tenths) => format!("rated {}.{}", tenths / 10, tenths % 10),
        None => "unrated".into(),
    });
    parts.push(match r.downloads {
        1 => "1 download".into(),
        n => format!("{} downloads", thousands(n)),
    });
    parts.join(" · ")
}

/// 23976 as `23.976`, 25000 as `25`.
fn fps_text(milli: u32) -> String {
    let whole = milli / 1000;
    let rest = milli % 1000;
    if rest == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{rest:03}")
            .trim_end_matches('0')
            .to_owned()
    }
}

/// What a download says, with the downloads left today.
pub fn downloaded_line(got: &SubtitleDownloaded) -> String {
    let mut line = format!("Downloaded as subtitle {}, for everyone", got.track + 1);
    if let Some(left) = got.user_remaining {
        line.push_str(&format!("; you have {left} left today"));
    }
    if let Some(left) = got.account_remaining {
        line.push_str(&format!("; the account has {left} left today"));
    }
    line
}

/// A refused search or download, in words: whose downloads ran out and
/// when more come, `now` being Unix seconds.
pub fn refusal(e: &ApiError, now: i64) -> String {
    match e.code {
        error_code::QUOTA_EXHAUSTED => {
            let data = e.data.as_ref();
            let whose = data.and_then(|d| d.get("whose")).and_then(Value::as_str);
            let resets = data.and_then(|d| d.get("resetsAt")).and_then(Value::as_i64);
            let who = match whose {
                Some("user") => "You have no subtitle downloads left today",
                _ => "The server's OpenSubtitles account has no downloads left today",
            };
            match resets {
                Some(at) if at > now => format!("{who}: more in {}.", wait(at - now)),
                _ => format!("{who}."),
            }
        }
        error_code::NOT_CONFIGURED => {
            "OpenSubtitles is not set up: give it an API key in Settings › Subtitles.".into()
        }
        error_code::NOT_ALLOWED => "An admin turned subtitle downloads off for this user.".into(),
        error_code::RATE_LIMITED => "OpenSubtitles asks to wait a moment; try again.".into(),
        _ => e.explain(),
    }
}

/// `secs` as hours and minutes: `5 h 12 min`, `40 min`.
fn wait(secs: i64) -> String {
    let minutes = (secs + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// The item's downloaded tracks among its subtitle files.
pub fn downloads(timings: &[SubtitleTimingInfo]) -> Vec<SubtitleTimingInfo> {
    timings
        .iter()
        .filter(|t| t.download.is_some())
        .cloned()
        .collect()
}

/// Who downloaded a track, and its origin: `Alice · machine-translated`.
pub fn downloaded_by(t: &SubtitleTimingInfo) -> String {
    let Some(d) = &t.download else {
        return String::new();
    };
    let mut line = format!("Downloaded by {}", d.by);
    match d.origin {
        SubtitleOrigin::MachineTranslated => line.push_str(" · machine-translated"),
        SubtitleOrigin::Ai => line.push_str(" · AI-made"),
        SubtitleOrigin::Human => {}
    }
    if d.hash_match {
        line.push_str(" · made for this file");
    }
    line
}

/// The languages field as the setting: `en, pt-BR` is `["en", "pt-BR"]`;
/// empty takes the setting out (English).
pub fn languages_value(typed: &str) -> Option<Value> {
    let list = crate::logic::entries(typed);
    (!list.is_empty()).then(|| json!(list))
}

/// Whether a secret field holds a reference the server reads its secret
/// from, `${SF_ENV_NAME}`, as the server takes it at run time; the
/// field's error otherwise.
pub fn secret_reference(label: &str, typed: &str) -> Result<Option<Value>, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Ok(None);
    }
    let name = typed
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .and_then(|s| s.strip_prefix("SF_ENV_"));
    match name {
        Some(rest)
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            Ok(Some(Value::String(typed.to_owned())))
        }
        _ => Err(format!(
            "{label}: name the server's environment variable that holds it, as \
             ${{SF_ENV_OPENSUBTITLES_KEY}}, not the secret itself."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> SubtitleResult {
        SubtitleResult {
            file_id: 101,
            release: "Film.2019.1080p".into(),
            language: "en".into(),
            format: Some("srt".into()),
            fps_milli: Some(23_976),
            downloads: 12_345,
            rating: Some(87),
            hearing_impaired: true,
            origin: SubtitleOrigin::Human,
            hash_match: true,
            rejected: false,
            downloaded: false,
        }
    }

    #[test]
    fn a_result_says_what_it_is_and_how_it_is_marked() {
        let r = result();
        assert_eq!(
            details(&r),
            "en · SRT · 23.976 fps · rated 8.7 · 12,345 downloads"
        );
        assert_eq!(
            marks(&r).iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Made for this file", "SDH"]
        );
        let machine = SubtitleResult {
            origin: SubtitleOrigin::MachineTranslated,
            hash_match: false,
            hearing_impaired: false,
            rejected: true,
            rating: None,
            fps_milli: Some(25_000),
            downloads: 1,
            ..r
        };
        assert_eq!(
            marks(&machine).iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Machine-translated", "Removed before"]
        );
        assert_eq!(
            details(&machine),
            "en · SRT · 25 fps · unrated · 1 download"
        );
    }

    #[test]
    fn a_quota_refusal_says_whose_and_when_more_come() {
        let refused = |whose: &str, at: i64| ApiError {
            code: error_code::QUOTA_EXHAUSTED,
            message: "server words".into(),
            data: Some(json!({ "whose": whose, "resetsAt": at })),
        };
        assert_eq!(
            refusal(&refused("user", 1_000 + 5 * 3600 + 12 * 60), 1_000),
            "You have no subtitle downloads left today: more in 5 h 12 min."
        );
        assert_eq!(
            refusal(&refused("account", 1_000 + 40 * 60), 1_000),
            "The server's OpenSubtitles account has no downloads left today: more in 40 min."
        );
        let off = ApiError {
            code: error_code::NOT_CONFIGURED,
            message: "server words".into(),
            data: None,
        };
        assert!(refusal(&off, 0).contains("Settings › Subtitles"));
    }

    #[test]
    fn a_download_says_what_is_left() {
        let got = SubtitleDownloaded {
            track: 2,
            subtitle: spoolframe_admin_api::SubtitleTrack {
                language: Some("en".into()),
                title: None,
                forced: false,
                default: false,
                external: true,
                role: spoolframe_admin_api::SubtitleRole::Full,
                timing: None,
                download: None,
            },
            account_remaining: Some(17),
            user_remaining: Some(1),
        };
        assert_eq!(
            downloaded_line(&got),
            "Downloaded as subtitle 3, for everyone; you have 1 left today; the account has 17 left today"
        );
    }

    #[test]
    fn a_secret_field_takes_only_a_reference() {
        assert_eq!(secret_reference("API key", " "), Ok(None));
        assert_eq!(
            secret_reference("API key", "${SF_ENV_OS_KEY}"),
            Ok(Some(json!("${SF_ENV_OS_KEY}")))
        );
        assert!(secret_reference("API key", "abc123").is_err());
        assert!(secret_reference("API key", "${HOME}").is_err());
        assert_eq!(languages_value("en, pt-BR"), Some(json!(["en", "pt-BR"])));
        assert_eq!(languages_value(""), None);
    }
}
