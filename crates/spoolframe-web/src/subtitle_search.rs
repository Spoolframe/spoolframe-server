//! Subtitles from OpenSubtitles and SubDL (PX-02), as an item's page and
//! Settings › Subtitles show them: a result's site and marks, how each
//! site's part of a search went, what a download or a refusal says, and
//! the settings' fields, testable on the host.

use serde_json::{Value, json};
use spoolframe_admin_api::{
    SubtitleDownloaded, SubtitleOrigin, SubtitleResult, SubtitleSearch, SubtitleSourceState,
    SubtitleTimingInfo, error_code,
};

use crate::logic::{ApiError, thousands};

/// The site ids the server knows, in the order Settings › Subtitles
/// lists them.
pub const SOURCES: [&str; 2] = ["opensubtitles", "subdl"];

/// A site's name, by its id: `subdl` is SubDL. One this build does not
/// know is shown as its id.
pub fn source_name(id: &str) -> &str {
    match id {
        "opensubtitles" => "OpenSubtitles",
        "subdl" => "SubDL",
        other => other,
    }
}

/// What a result is marked with, each with its pill's tone: its site,
/// found by the hash, then what a viewer weighs.
pub fn marks(r: &SubtitleResult) -> Vec<(String, &'static str)> {
    let mut marks = vec![(source_name(&r.source).to_owned(), "pill pill-source")];
    let mut mark = |text: &str, tone| marks.push((text.to_owned(), tone));
    if r.hash_match {
        mark("Made for this file", "pill pill-trusted");
    }
    if r.hearing_impaired {
        mark("SDH", "pill");
    }
    match r.origin {
        SubtitleOrigin::MachineTranslated => mark("Machine-translated", "pill pill-warn"),
        SubtitleOrigin::Ai => mark("AI-made", "pill pill-warn"),
        SubtitleOrigin::Human => {}
    }
    if r.downloaded {
        mark("Downloaded", "pill pill-trusted");
    }
    if r.rejected {
        mark("Removed before", "pill pill-bad");
    }
    marks
}

/// What a search says of each site that did not answer: `SubDL is out of
/// requests for today: more in 3 h.`, `now` being Unix seconds. Nothing
/// for one that answered.
pub fn source_lines(found: &SubtitleSearch, now: i64) -> Vec<String> {
    found
        .sources
        .iter()
        .filter_map(|status| {
            let name = source_name(&status.source);
            Some(match &status.state {
                SubtitleSourceState::Found { .. } => return None,
                SubtitleSourceState::QuotaExhausted {
                    resets_at: Some(at),
                } if *at > now => {
                    format!(
                        "{name} is out of requests for today: more in {}.",
                        wait(at - now)
                    )
                }
                SubtitleSourceState::QuotaExhausted { .. } => {
                    format!("{name} is out of requests for today.")
                }
                SubtitleSourceState::RateLimited { .. } => {
                    format!("{name} asks to wait a moment; search again.")
                }
                SubtitleSourceState::Failed { message } => format!("{message}."),
                SubtitleSourceState::Unknown => format!("{name} did not answer."),
            })
        })
        .collect()
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
            let site = data.and_then(|d| d.get("site")).and_then(Value::as_str);
            let who = match (whose, site) {
                (Some("user"), _) => "You have no subtitle downloads left today".to_owned(),
                (_, Some(site)) => format!(
                    "The server's {} account has no downloads left today",
                    source_name(site)
                ),
                _ => "The server's subtitle account has no downloads left today".to_owned(),
            };
            match resets {
                Some(at) if at > now => format!("{who}: more in {}.", wait(at - now)),
                _ => format!("{who}."),
            }
        }
        error_code::NOT_CONFIGURED => "No subtitle site is set up: give OpenSubtitles or SubDL an \
                                        API key in Settings › Subtitles."
            .into(),
        error_code::NOT_ALLOWED => "An admin turned subtitle downloads off for this user.".into(),
        error_code::RATE_LIMITED => "The subtitle site asks to wait a moment; try again.".into(),
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

/// `subtitles.sources` with the sites ticked: `on[i]` for [`SOURCES`]`[i]`,
/// in that order.
pub fn sources_value(on: [bool; 2]) -> Value {
    json!(
        SOURCES
            .iter()
            .zip(on)
            .filter(|(_, on)| *on)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
    )
}

/// Which of [`SOURCES`] `subtitles.sources` names; every one when it is
/// not set, as the server reads it.
pub fn sources_on(setting: Option<&Value>) -> [bool; 2] {
    match setting.and_then(Value::as_array) {
        None => [true; 2],
        Some(list) => SOURCES.map(|id| list.iter().any(|v| v.as_str() == Some(id))),
    }
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
             ${{SF_ENV_SUBTITLES_KEY}}, not the secret itself."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> SubtitleResult {
        SubtitleResult {
            id: "opensubtitles:101".into(),
            source: "opensubtitles".into(),
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
            marks(&r)
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["OpenSubtitles", "Made for this file", "SDH"]
        );
        let machine = SubtitleResult {
            source: "subdl".into(),
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
            marks(&machine)
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["SubDL", "Machine-translated", "Removed before"]
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
            "The server's subtitle account has no downloads left today: more in 40 min."
        );
        let subdl = ApiError {
            code: error_code::QUOTA_EXHAUSTED,
            message: "server words".into(),
            data: Some(json!({ "whose": "account", "site": "subdl", "resetsAt": null })),
        };
        assert_eq!(
            refusal(&subdl, 1_000),
            "The server's SubDL account has no downloads left today."
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

    /// A site that failed or ran out is said by name; one that answered
    /// is not.
    #[test]
    fn a_search_says_which_site_did_not_answer() {
        use spoolframe_admin_api::SubtitleSourceStatus;
        let status = |source: &str, state| SubtitleSourceStatus {
            source: source.into(),
            state,
        };
        let found = SubtitleSearch {
            languages: vec!["en".into()],
            results: Vec::new(),
            sources: vec![
                status("opensubtitles", SubtitleSourceState::Found { found: 4 }),
                status(
                    "subdl",
                    SubtitleSourceState::QuotaExhausted {
                        resets_at: Some(1_000 + 3 * 3600),
                    },
                ),
                status(
                    "podnapisi",
                    SubtitleSourceState::Failed {
                        message: "Podnapisi cannot be reached: no connection".into(),
                    },
                ),
            ],
        };
        assert_eq!(
            source_lines(&found, 1_000),
            [
                "SubDL is out of requests for today: more in 3 h.",
                "Podnapisi cannot be reached: no connection.",
            ]
        );
    }

    /// The sites ticked are the setting; unset, both are on.
    #[test]
    fn the_sites_ticked_are_the_setting() {
        assert_eq!(sources_value([true, false]), json!(["opensubtitles"]));
        assert_eq!(
            sources_value([true, true]),
            json!(["opensubtitles", "subdl"])
        );
        assert_eq!(sources_value([false, false]), json!([]));
        assert_eq!(sources_on(None), [true, true]);
        assert_eq!(sources_on(Some(&json!(["subdl"]))), [false, true]);
        assert_eq!(sources_on(Some(&json!([]))), [false, false]);
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
