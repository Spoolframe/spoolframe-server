//! Subtitles from OpenSubtitles and SubDL (PX-02), as an item's page and
//! Settings › Subtitles show them: a result's site and marks, how each
//! site's part of a search went, what a download or a refusal says, and
//! the settings' fields, testable on the host.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use spoolframe_admin_api::{
    SubtitleAccountCount, SubtitleDownloaded, SubtitleOrigin, SubtitlePairUsage, SubtitleResult,
    SubtitleSearch, SubtitleSiteUsage, SubtitleSource, SubtitleSourceState, SubtitleTimingInfo,
    SubtitleUserUsage, error_code,
};

use crate::logic::{ApiError, thousands};

/// What a result is marked with, each with its pill's tone: its site,
/// found by the hash, then what a viewer weighs.
pub fn marks(r: &SubtitleResult) -> Vec<(String, &'static str)> {
    let mut marks = vec![(
        SubtitleSource::name_of(&r.source).to_owned(),
        "pill pill-source",
    )];
    let mut mark = |text: &str, tone| marks.push((text.to_owned(), tone));
    if r.hash_match {
        mark("Made for this file", "pill pill-trusted");
    }
    if r.hearing_impaired {
        mark("SDH", "pill");
    }
    if r.forced {
        mark("Forced", "pill");
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
            let name = SubtitleSource::name_of(&status.source);
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
                    SubtitleSource::name_of(site)
                ),
                _ => "The server's subtitle account has no downloads left today".to_owned(),
            };
            match resets {
                Some(at) if at > now => format!("{who}: more in {}.", wait(at - now)),
                _ => format!("{who}."),
            }
        }
        error_code::NOT_CONFIGURED => {
            "No subtitle site is turned on: pick one in Settings › Subtitles.".into()
        }
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

/// A site's day in Settings › Subtitles, `now` being Unix seconds:
/// `OpenSubtitles: 3 downloads by this server. Its account: 5 of 20
/// used, more in 3 h.` The account's count is the site's own, as it said
/// it at the server's last download, and only while it holds.
pub fn site_usage_line(site: &SubtitleSiteUsage, now: i64) -> String {
    let name = SubtitleSource::name_of(&site.source);
    let downloads = match site.downloads {
        1 => "1 download".to_owned(),
        n => format!("{} downloads", thousands(n.into())),
    };
    let mut line = format!("{name}: {downloads} by this server.");
    let holds = |a: &SubtitleAccountCount| a.resets_at.is_none_or(|at| at > now);
    match &site.account {
        Some(a) if holds(a) => {
            match (a.used, a.limit) {
                (Some(used), Some(limit)) => {
                    line.push_str(&format!(" Its account: {used} of {limit} used"));
                }
                _ => line.push_str(&format!(" Its account: {} left", a.remaining)),
            }
            if let Some(at) = a.resets_at {
                line.push_str(&format!(", more in {}", wait(at - now)));
            }
            line.push('.');
        }
        _ if SubtitleSource::of(&site.source) == Some(SubtitleSource::SubDl) => {
            line.push_str(" SubDL does not say its account's count.");
        }
        _ => line.push_str(" Its account's count shows after the next download."),
    }
    line
}

/// A user's downloads in the day against the daily limit: `2 of 5`, or
/// `2` without a limit.
pub fn user_usage(user: &SubtitleUserUsage, limit: u32) -> String {
    if limit == 0 {
        user.downloads.to_string()
    } else {
        format!("{} of {limit}", user.downloads)
    }
}

/// A paired server's row on Settings › Subtitles (P8-07): its users'
/// downloads together, as one, against the limit per server.
pub fn pair_usage(pair: &SubtitlePairUsage, limit: u32) -> String {
    if limit == 0 {
        format!("{}: {} today", pair.name, pair.downloads)
    } else {
        format!("{}: {} of {limit} today", pair.name, pair.downloads)
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

/// The sites ticked: the setting's, or every one when it is not set, as
/// the server reads it.
pub type Ticked = BTreeSet<SubtitleSource>;

/// Which sites `subtitles.sources` names; every one when it is not set.
pub fn sources_on(setting: Option<&Value>) -> Ticked {
    match setting.and_then(Value::as_array) {
        None => SubtitleSource::ALL.into(),
        Some(list) => list
            .iter()
            .filter_map(|v| SubtitleSource::of(v.as_str()?))
            .collect(),
    }
}

/// `subtitles.sources` once the boxes ticked when the form opened,
/// `before`, are `now`: `None` when they did not change, so the setting
/// is left as it is (unset, it follows the server's default). Changed,
/// the sites keep the order `setting` gives them, the order they are
/// asked in; one newly ticked comes last.
pub fn sources_value(setting: Option<&Value>, before: &Ticked, now: &Ticked) -> Option<Value> {
    if before == now {
        return None;
    }
    let mut order: Vec<SubtitleSource> = match setting.and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(|v| SubtitleSource::of(v.as_str()?))
            .collect(),
        None => SubtitleSource::ALL.to_vec(),
    };
    order.retain(|s| now.contains(s));
    for s in now {
        if !order.contains(s) {
            order.push(*s);
        }
    }
    Some(json!(order.iter().map(|s| s.id()).collect::<Vec<_>>()))
}

/// The daily limit, every site's.
pub const LIMIT: &str = "subtitles.daily_limit_per_user";

/// The daily limit as typed: a whole number, or nothing (none).
pub fn limit_value(typed: &str) -> Result<Option<Value>, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Ok(None);
    }
    let n: u32 = typed
        .parse()
        .map_err(|_| "The daily limit is a whole number.".to_owned())?;
    Ok(Some(json!(n)))
}

/// The languages field as the setting: `en, pt-BR` is `["en", "pt-BR"]`;
/// empty takes the setting out (English).
pub fn languages_value(typed: &str) -> Option<Value> {
    let list = crate::logic::entries(typed);
    (!list.is_empty()).then(|| json!(list))
}

/// Whether a secret field holds a reference the server reads its secret
/// from, `${SF_ENV_NAME}`, as the server takes it at run time; the
/// field's error otherwise, naming `example` (`SF_ENV_SUBDL_KEY`) as the
/// field's own placeholder does.
pub fn secret_reference(label: &str, typed: &str, example: &str) -> Result<Option<Value>, String> {
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
             ${{{example}}}, not the secret itself."
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
            forced: false,
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
            forced: true,
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
            ["SubDL", "Forced", "Machine-translated", "Removed before"]
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

    /// A site's day: this server's downloads, and its account's count
    /// while it holds; SubDL's says it never has one.
    #[test]
    fn a_site_s_day_says_its_downloads_and_its_account() {
        let site = |source: &str, downloads, account| SubtitleSiteUsage {
            source: source.into(),
            downloads,
            account,
        };
        let count = |used, limit, resets_at| SubtitleAccountCount {
            used,
            remaining: 15,
            limit,
            resets_at,
            at: 0,
        };
        let now = 1_000;
        assert_eq!(
            site_usage_line(
                &site(
                    "opensubtitles",
                    3,
                    Some(count(Some(5), Some(20), Some(now + 3 * 3600)))
                ),
                now
            ),
            "OpenSubtitles: 3 downloads by this server. Its account: 5 of 20 used, more in 3 h."
        );
        assert_eq!(
            site_usage_line(
                &site("opensubtitles", 1, Some(count(None, None, None))),
                now
            ),
            "OpenSubtitles: 1 download by this server. Its account: 15 left."
        );
        // Past its reset, the count no longer holds.
        assert_eq!(
            site_usage_line(
                &site(
                    "opensubtitles",
                    0,
                    Some(count(Some(5), Some(20), Some(now)))
                ),
                now
            ),
            "OpenSubtitles: 0 downloads by this server. Its account's count shows after the next download."
        );
        assert_eq!(
            site_usage_line(&site("subdl", 2, None), now),
            "SubDL: 2 downloads by this server. SubDL does not say its account's count."
        );
    }

    #[test]
    fn a_user_s_day_is_against_the_limit_when_there_is_one() {
        let alice = SubtitleUserUsage {
            username: Some("alice".into()),
            name: "Alice".into(),
            downloads: 2,
            remaining: Some(3),
        };
        assert_eq!(user_usage(&alice, 5), "2 of 5");
        assert_eq!(user_usage(&alice, 0), "2");
    }

    /// A paired server's users are one row, by the server's name (P8-07).
    #[test]
    fn a_paired_server_s_day_is_one_row_against_its_limit() {
        let bob = SubtitlePairUsage {
            name: "Bob's Media".into(),
            downloads: 12,
            remaining: Some(28),
        };
        assert_eq!(pair_usage(&bob, 40), "Bob's Media: 12 of 40 today");
        assert_eq!(pair_usage(&bob, 0), "Bob's Media: 12 today");
    }

    #[test]
    fn the_limit_is_a_whole_number_or_none() {
        assert_eq!(limit_value(" 3 "), Ok(Some(json!(3))));
        assert_eq!(limit_value(" "), Ok(None));
        assert!(limit_value("three").is_err());
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

    /// The sites are written only when a box changed, keeping the order
    /// the admin set; a site ticked anew comes last. Unset, both are on.
    #[test]
    fn the_sites_ticked_are_the_setting_in_the_admin_s_order() {
        use SubtitleSource::{OpenSubtitles, SubDl};
        let ticked = |sites: &[SubtitleSource]| sites.iter().copied().collect::<Ticked>();
        let both = ticked(&[OpenSubtitles, SubDl]);
        assert_eq!(sources_value(None, &both, &both), None);
        let subdl_first = json!(["subdl", "opensubtitles"]);
        assert_eq!(sources_value(Some(&subdl_first), &both, &both), None);
        assert_eq!(
            sources_value(None, &both, &ticked(&[OpenSubtitles])),
            Some(json!(["opensubtitles"]))
        );
        assert_eq!(
            sources_value(Some(&subdl_first), &both, &ticked(&[SubDl])),
            Some(json!(["subdl"]))
        );
        // OpenSubtitles ticked again: after SubDL, which was alone.
        let subdl_only = json!(["subdl"]);
        assert_eq!(
            sources_value(Some(&subdl_only), &ticked(&[SubDl]), &both),
            Some(json!(["subdl", "opensubtitles"]))
        );
        assert_eq!(sources_value(None, &both, &ticked(&[])), Some(json!([])));
        assert_eq!(sources_on(None), both);
        assert_eq!(sources_on(Some(&json!(["subdl"]))), ticked(&[SubDl]));
        assert_eq!(sources_on(Some(&json!([]))), ticked(&[]));
    }

    #[test]
    fn a_secret_field_takes_only_a_reference() {
        let key = |typed| secret_reference("SubDL's API key", typed, "SF_ENV_SUBDL_KEY");
        assert_eq!(key(" "), Ok(None));
        assert_eq!(key("${SF_ENV_OS_KEY}"), Ok(Some(json!("${SF_ENV_OS_KEY}"))));
        assert!(key("abc123").unwrap_err().contains("${SF_ENV_SUBDL_KEY}"));
        assert!(key("${HOME}").is_err());
        let password = secret_reference("Password", "hunter2", "SF_ENV_OPENSUBTITLES_PASSWORD");
        assert!(
            password
                .unwrap_err()
                .contains("${SF_ENV_OPENSUBTITLES_PASSWORD}")
        );
        assert_eq!(languages_value("en, pt-BR"), Some(json!(["en", "pt-BR"])));
        assert_eq!(languages_value(""), None);
    }
}
