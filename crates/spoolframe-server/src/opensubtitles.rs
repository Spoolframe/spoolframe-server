//! The opensubtitles.com REST API (PX-02): the site subtitles are searched
//! and downloaded from, on demand only. Its old XML-RPC API is shut down.
//!
//! Every request carries Spoolframe's own API key, built in
//! ([`builtin_key`]): an app's key at OpenSubtitles names the app, not
//! the server, so no admin sets one. A download also carries the token of
//! the account the admin set, when one is set (without one the site
//! counts downloads by address, a handful a day). The site limits
//! requests per second: calls are spaced [`MIN_INTERVAL`] apart, and when
//! it answers 429 no call is made until it said to wait. Everything it
//! sends is read with a cap, by serde into the structs below; the
//! download's link must lead to the site itself.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The site's API, as it is reached.
pub const API_URL: &str = "https://api.opensubtitles.com/api/v1";

/// How long one call to the site may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The least time between two calls: the site allows five a second.
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// The most of a JSON answer read: a page of search results is some
/// 100 KB.
const MAX_JSON: usize = 4 << 20;

/// How long a sign-in's token is used: the site's last a day.
const TOKEN_LIFE: Duration = Duration::from_secs(23 * 60 * 60);

/// The site, as the admin set it up: the account, and where it is
/// (another address only in tests). The key is [`builtin_key`].
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub api_url: url::Url,
    /// Signed in for downloads when both are set.
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for Account {
    /// Never the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("api_url", &self.api_url.as_str())
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

pub use crate::subtitle_site::SiteError;

/// What the pad that hides the built-in key is made from: the first 32
/// bytes of BLAKE3 of it.
const KEY_PAD_LABEL: &[u8] = b"spoolframe opensubtitles key v1";

/// Spoolframe's OpenSubtitles API key, XORed with the pad. Obfuscated,
/// not secret: anyone with the binary can recover it, and OpenSubtitles
/// expects an app's key to ship in the app. This keeps it out of `grep`,
/// out of secret scanners, and out of the binary's read-only data as
/// text: the pad is computed at run time, so the compiler cannot fold
/// the two into the plain key.
const SCRAMBLED_KEY: [u8; 32] = [
    0x84, 0xa7, 0xe6, 0x26, 0x57, 0xd2, 0xda, 0x21, 0x47, 0xac, 0xc2, 0xcf, 0x29, 0x4f, 0xc9, 0xfb,
    0xc7, 0x51, 0x73, 0xdb, 0xea, 0x64, 0x86, 0x6b, 0x57, 0x40, 0x86, 0x12, 0x78, 0xd2, 0x84, 0xb1,
];

/// The built-in API key, decoded. Made right before it is put on a
/// request and dropped with it: never kept, logged, shown in settings,
/// or sent anywhere but OpenSubtitles.
pub fn builtin_key() -> String {
    let pad = blake3::hash(KEY_PAD_LABEL);
    let key: Vec<u8> = SCRAMBLED_KEY
        .iter()
        .zip(pad.as_bytes())
        .map(|(a, b)| a ^ b)
        .collect();
    String::from_utf8(key).expect("the built-in key is ASCII")
}
use crate::subtitle_site::{Pacer, client, endpoint, read_capped, refused, retry_after};

/// What a search asks for. Every part left out is not sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// The file's OpenSubtitles hash, 16 hex digits ([`movie_hash`]).
    pub moviehash: Option<String>,
    /// The item's own IMDb or TMDB id: a film's, or an episode's.
    pub imdb_id: Option<u64>,
    pub tmdb_id: Option<u64>,
    /// An episode's show's, with the season and episode, when the episode
    /// has none of its own.
    pub parent_imdb_id: Option<u64>,
    pub parent_tmdb_id: Option<u64>,
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// The title, only when there is no id.
    pub query: Option<String>,
    pub year: Option<u16>,
    /// Comma-separated: `en,pt-br`.
    pub languages: String,
}

impl Query {
    /// The query string, its keys in order and its values in lower case,
    /// as the site wants them (it redirects a request otherwise).
    fn pairs(&self) -> BTreeMap<&'static str, String> {
        let mut pairs = BTreeMap::new();
        let mut put = |key, value: Option<String>| {
            if let Some(value) = value {
                pairs.insert(key, value.to_lowercase());
            }
        };
        put("moviehash", self.moviehash.clone());
        put("imdb_id", self.imdb_id.map(|n| n.to_string()));
        put("tmdb_id", self.tmdb_id.map(|n| n.to_string()));
        put("parent_imdb_id", self.parent_imdb_id.map(|n| n.to_string()));
        put("parent_tmdb_id", self.parent_tmdb_id.map(|n| n.to_string()));
        put("season_number", self.season_number.map(|n| n.to_string()));
        put("episode_number", self.episode_number.map(|n| n.to_string()));
        put("query", self.query.clone());
        put("year", self.year.map(|n| n.to_string()));
        put("languages", Some(self.languages.clone()));
        pairs
    }
}

/// One subtitle file a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub file_id: u64,
    /// Its file's name on the site, to tell its format by.
    pub file_name: Option<String>,
    pub release: String,
    pub language: String,
    pub fps: Option<f64>,
    pub downloads: u64,
    /// 0 to 10; 0 while unrated.
    pub rating: f64,
    pub hearing_impaired: bool,
    /// Only the parts in another language than the rest: the site's
    /// `foreign_parts_only`.
    pub forced: bool,
    pub machine_translated: bool,
    pub ai_translated: bool,
    pub hash_match: bool,
}

/// What a download got: the file, and the account's downloads left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub bytes: Vec<u8>,
    pub remaining: Option<u32>,
    /// When the account's count starts again, Unix seconds.
    pub resets_at: Option<i64>,
}

/// The site's answers, as far as they are read. Every field may be
/// missing or null: the site leaves out what it does not know.
#[derive(Debug, Deserialize)]
struct SearchAnswer {
    #[serde(default)]
    data: Vec<SearchEntry>,
}

#[derive(Debug, Deserialize)]
struct SearchEntry {
    attributes: Attributes,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Attributes {
    language: Option<String>,
    download_count: Option<u64>,
    hearing_impaired: Option<bool>,
    foreign_parts_only: Option<bool>,
    fps: Option<f64>,
    ratings: Option<f64>,
    ai_translated: Option<bool>,
    machine_translated: Option<bool>,
    release: Option<String>,
    moviehash_match: Option<bool>,
    files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    file_id: u64,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginAnswer {
    token: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DownloadAnswer {
    link: String,
    #[serde(default)]
    remaining: Option<i64>,
    #[serde(default)]
    reset_time_utc: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Refusal {
    message: Option<String>,
    reset_time_utc: Option<String>,
}

/// A sign-in's token, and where the site said to go with it.
#[derive(Debug, Clone)]
struct Session {
    token: String,
    api_url: url::Url,
    until: Instant,
}

/// The site, called for one account.
#[derive(Debug)]
pub struct Site {
    account: Account,
    http: reqwest::Client,
    pacer: Pacer,
    session: tokio::sync::Mutex<Option<Session>>,
}

impl Site {
    pub fn new(account: Account) -> Self {
        let api_host = account.api_url.host_str().map(str::to_owned);
        // The download's file is served from one of the site's hosts.
        let http = client(CALL_TIMEOUT, move |url| site_host(url, api_host.as_deref()));
        Self {
            account,
            http,
            pacer: Pacer::new(MIN_INTERVAL),
            session: tokio::sync::Mutex::new(None),
        }
    }

    /// The subtitles `query` finds: the first page, the site's best.
    pub async fn search(&self, query: &Query) -> Result<Vec<Found>, SiteError> {
        let mut url = endpoint(&self.account.api_url, "subtitles");
        url.query_pairs_mut().extend_pairs(query.pairs());
        let request = self.http.get(url);
        let body = self.call(request).await?;
        read_search(&body)
    }

    /// Download file `file_id`, as `sub_format` (`srt`) when given, else
    /// as it is on the site.
    pub async fn download(
        &self,
        file_id: u64,
        sub_format: Option<&str>,
    ) -> Result<Downloaded, SiteError> {
        let mut retried = false;
        loop {
            let session = self.session().await?;
            let api_url = session
                .as_ref()
                .map_or(&self.account.api_url, |s| &s.api_url);
            let mut body = serde_json::json!({ "file_id": file_id });
            if let Some(format) = sub_format {
                body["sub_format"] = format.into();
            }
            let mut request = self.http.post(endpoint(api_url, "download")).json(&body);
            if let Some(session) = &session {
                request = request.bearer_auth(&session.token);
            }
            let answer = match self.call(request).await {
                // A token the site no longer takes: sign in again, once.
                Err(SiteError::Refused { status: 401, .. }) if session.is_some() && !retried => {
                    *self.session.lock().await = None;
                    retried = true;
                    continue;
                }
                other => other?,
            };
            let answer: DownloadAnswer = serde_json::from_slice(&answer)
                .map_err(|e| SiteError::Unreadable(e.to_string()))?;
            let link = url::Url::parse(&answer.link)
                .map_err(|e| SiteError::Unreadable(format!("the download link: {e}")))?;
            if !site_host(&link, self.account.api_url.host_str())
                || !(link.scheme() == "https" || link.scheme() == self.account.api_url.scheme())
            {
                return Err(SiteError::Unreadable(
                    "the download link leads away from the site".into(),
                ));
            }
            let bytes = self
                .fetch(link, spoolframe_media::subtitle_file::MAX_DOWNLOAD_BYTES)
                .await?;
            return Ok(Downloaded {
                bytes,
                remaining: answer.remaining.and_then(|n| u32::try_from(n).ok()),
                resets_at: answer.reset_time_utc.as_deref().and_then(utc_seconds),
            });
        }
    }

    /// The account's token, signing in when there is none or it is old;
    /// `None` without an account.
    async fn session(&self) -> Result<Option<Session>, SiteError> {
        let (Some(username), Some(password)) = (&self.account.username, &self.account.password)
        else {
            return Ok(None);
        };
        let mut kept = self.session.lock().await;
        if let Some(session) = kept.as_ref()
            && session.until > Instant::now()
        {
            return Ok(Some(session.clone()));
        }
        let request = self
            .http
            .post(endpoint(&self.account.api_url, "login"))
            .json(&serde_json::json!({ "username": username, "password": password }));
        let body = self.call(request).await?;
        let answer: LoginAnswer =
            serde_json::from_slice(&body).map_err(|e| SiteError::Unreadable(e.to_string()))?;
        let session = Session {
            token: answer.token,
            api_url: answer
                .base_url
                .as_deref()
                .and_then(|host| moved_api(&self.account.api_url, host))
                .unwrap_or_else(|| self.account.api_url.clone()),
            until: Instant::now() + TOKEN_LIFE,
        };
        *kept = Some(session.clone());
        Ok(Some(session))
    }

    /// `request` sent with the key, in its turn, and its answer's body.
    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, SiteError> {
        let request = request
            .header("Api-Key", builtin_key())
            .header(reqwest::header::ACCEPT, "application/json");
        let response = self.send(request).await?;
        let status = response.status();
        if status.is_success() {
            return read_capped(response, MAX_JSON).await;
        }
        let retry_after = retry_after(&response);
        let body = read_capped(response, MAX_JSON).await.unwrap_or_default();
        let refusal: Refusal = serde_json::from_slice(&body).unwrap_or_default();
        let message = refusal.message.unwrap_or_default();
        Err(match status.as_u16() {
            429 => self.pacer.told_to_wait(retry_after),
            // The site's word for a download past the account's quota.
            406 => SiteError::QuotaExhausted {
                resets_at: refusal.reset_time_utc.as_deref().and_then(utc_seconds),
            },
            500.. => SiteError::Unreachable(format!("{status}")),
            code => refused(code, &message),
        })
    }

    /// The file at `link`, at most `max` bytes.
    async fn fetch(&self, link: url::Url, max: usize) -> Result<Vec<u8>, SiteError> {
        let response = self.send(self.http.get(link)).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(SiteError::Unreachable(format!(
                "the subtitle file: {status}"
            )));
        }
        read_capped(response, max).await
    }

    /// Send `request` when its turn comes: [`MIN_INTERVAL`] after the last,
    /// and not while the site said to wait.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, SiteError> {
        self.pacer.turn().await?;
        crate::subtitle_site::send(request).await
    }
}

/// Whether `url` is on the site: the API's own host, or one of
/// opensubtitles.com's or .org's.
fn site_host(url: &url::Url, api_host: Option<&str>) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if Some(host) == api_host {
        return true;
    }
    ["opensubtitles.com", "opensubtitles.org"]
        .iter()
        .any(|site| host == *site || host.ends_with(&format!(".{site}")))
}

/// The API at `host` (a sign-in's `base_url`, a VIP's own), if it is one
/// of the site's.
fn moved_api(api: &url::Url, host: &str) -> Option<url::Url> {
    let host = host
        .trim()
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let host = host.split('/').next()?;
    let mut moved = api.clone();
    moved.set_host(Some(host)).ok()?;
    (moved
        .host_str()
        .is_some_and(|h| h.ends_with(".opensubtitles.com"))
        && moved.scheme() == "https")
        .then_some(moved)
}

/// The search answer's files, one result each.
fn read_search(body: &[u8]) -> Result<Vec<Found>, SiteError> {
    let answer: SearchAnswer =
        serde_json::from_slice(body).map_err(|e| SiteError::Unreadable(e.to_string()))?;
    let mut found = Vec::new();
    for entry in answer.data {
        let a = entry.attributes;
        let Some(language) = a.language.filter(|l| !l.is_empty()) else {
            continue;
        };
        for file in &a.files {
            found.push(Found {
                file_id: file.file_id,
                file_name: file.file_name.clone(),
                release: a.release.clone().unwrap_or_default(),
                language: language.clone(),
                fps: a.fps.filter(|f| f.is_finite() && *f > 0.0),
                downloads: a.download_count.unwrap_or(0),
                rating: a
                    .ratings
                    .filter(|r| r.is_finite())
                    .unwrap_or(0.0)
                    .clamp(0.0, 10.0),
                hearing_impaired: a.hearing_impaired.unwrap_or(false),
                forced: a.foreign_parts_only.unwrap_or(false),
                machine_translated: a.machine_translated.unwrap_or(false),
                ai_translated: a.ai_translated.unwrap_or(false),
                hash_match: a.moviehash_match.unwrap_or(false),
            });
        }
    }
    Ok(found)
}

/// `2022-04-08T13:03:16.000Z` (the site's UTC times) as Unix seconds.
pub fn utc_seconds(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if separators
        .iter()
        .any(|(at, c)| text.as_bytes().get(*at) != Some(c))
        || !matches!(text.as_bytes().get(10), Some(b'T' | b' '))
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The OpenSubtitles hash of the file at `path`, 16 hex digits: its size
/// plus the sum of its first and last 64 KiB as little-endian 64-bit
/// words, wrapping. `None` for a file under 128 KiB, which the site does
/// not hash.
pub fn movie_hash(path: &std::path::Path) -> std::io::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size < 2 * CHUNK {
        return Ok(None);
    }
    let mut sum = size;
    let mut buffer = vec![0u8; CHUNK as usize];
    for at in [0, size - CHUNK] {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut buffer)?;
        for word in buffer.as_chunks::<8>().0 {
            sum = sum.wrapping_add(u64::from_le_bytes(*word));
        }
    }
    Ok(Some(format!("{sum:016x}")))
}

#[cfg(test)]
pub(crate) mod tests;
