//! The subdl.com API (PX-02): the second site subtitles are searched and
//! downloaded from, on demand only, beside [`crate::opensubtitles`].
//!
//! A search is `GET api.subdl.com/api/v1/subtitles` with the key in its
//! query, as SubDL documents it; a download is the result's file at
//! `dl.subdl.com`, a zip archive (or now and then the file itself), with
//! the key in the `x-api-key` header. SubDL writes the caller's key into
//! every result's link: a link is read only for its path, checked against
//! the shape SubDL gives it, and the rest of it dropped, so the key is in
//! nothing the server keeps, logs or answers with. Calls are spaced
//! [`MIN_INTERVAL`] apart and a 429 is obeyed, as for OpenSubtitles;
//! answers are read with a cap, by serde.

use std::time::Duration;

use serde::Deserialize;

use crate::subtitle_site::{Pacer, SiteError, read_capped, retry_after};

/// The API, as it is reached.
pub const API_URL: &str = "https://api.subdl.com/api/v1";

/// Where its files are downloaded from.
pub const DOWNLOAD_URL: &str = "https://dl.subdl.com";

/// How long one call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The least time between two calls. SubDL counts requests a day (2 000
/// on a free key); this keeps a burst of searches from looking like
/// abuse.
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// The most of a JSON answer read: thirty results are some 20 KB.
const MAX_JSON: usize = 4 << 20;

/// The most results asked for: SubDL's own most a page.
const PER_PAGE: u32 = 30;

/// SubDL, as the admin set it up: the key, and where it is (other
/// addresses only in tests).
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub api_url: url::Url,
    pub download_url: url::Url,
    pub api_key: String,
}

impl std::fmt::Debug for Account {
    /// Never the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("api_url", &self.api_url.as_str())
            .field("download_url", &self.download_url.as_str())
            .finish_non_exhaustive()
    }
}

/// What a search asks for. Every part left out is not sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// `tt0078748`: a film's, or an episode's show's.
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<u64>,
    /// The title, only when there is no id.
    pub film_name: Option<String>,
    pub year: Option<u16>,
    /// Asked for an episode: its show's ids or title, and these.
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// `movie` or `tv`.
    pub kind: &'static str,
    /// SubDL's codes, comma-separated: `EN,BR_PT`.
    pub languages: String,
}

impl Query {
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = vec![("type", self.kind.to_owned())];
        let mut put = |key, value: Option<String>| {
            if let Some(value) = value {
                pairs.push((key, value));
            }
        };
        put("imdb_id", self.imdb_id.clone());
        put("tmdb_id", self.tmdb_id.map(|n| n.to_string()));
        put("film_name", self.film_name.clone());
        put("year", self.year.map(|n| n.to_string()));
        put("season_number", self.season_number.map(|n| n.to_string()));
        put("episode_number", self.episode_number.map(|n| n.to_string()));
        put("languages", Some(self.languages.clone()));
        pairs.push(("subs_per_page", PER_PAGE.to_string()));
        // Each result's hearing-impaired flag, which is left out without.
        pairs.push(("hi", "1".into()));
        pairs.push(("client", "custom_integration".into()));
        pairs
    }
}

/// One subtitle a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Its id on SubDL, `3172974-3188291`: what its link names.
    pub id: String,
    /// Its file's path on [`DOWNLOAD_URL`], `/subtitle/3172974-3188291.zip`,
    /// without the query SubDL put the key in.
    pub path: String,
    pub release: String,
    /// As the server names languages: `en`, `pt-br`.
    pub language: String,
    /// What its link ends in, when it is a subtitle's own: `srt`; `None`
    /// for an archive.
    pub format: Option<String>,
    pub fps: Option<f64>,
    pub hearing_impaired: bool,
}

#[derive(Debug, Deserialize)]
struct SearchAnswer {
    #[serde(default)]
    status: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    subtitles: Vec<Entry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Entry {
    release_name: Option<String>,
    language: Option<String>,
    url: Option<String>,
    hi: Option<bool>,
    fps: Option<serde_json::Value>,
}

/// SubDL, called with one key.
#[derive(Debug)]
pub struct Site {
    account: Account,
    http: reqwest::Client,
    pacer: Pacer,
}

impl Site {
    pub fn new(account: Account) -> Self {
        let hosts = [
            account.api_url.host_str().map(str::to_owned),
            account.download_url.host_str().map(str::to_owned),
        ];
        let http = reqwest::Client::builder()
            .user_agent(concat!("Spoolframe v", env!("CARGO_PKG_VERSION")))
            .timeout(CALL_TIMEOUT)
            // A redirect only to SubDL itself.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let host = attempt.url().host_str();
                if attempt.previous().len() >= 3 {
                    attempt.error("too many redirects")
                } else if hosts.iter().flatten().any(|h| Some(h.as_str()) == host)
                    || host.is_some_and(|h| h == "subdl.com" || h.ends_with(".subdl.com"))
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .expect("a client with rustls and no proxy settings builds");
        Self {
            account,
            http,
            pacer: Pacer::new(MIN_INTERVAL),
        }
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    /// The subtitles `query` finds: SubDL's first page.
    pub async fn search(&self, query: &Query) -> Result<Vec<Found>, SiteError> {
        let mut url = endpoint(&self.account.api_url, "subtitles");
        url.query_pairs_mut()
            .append_pair("api_key", &self.account.api_key)
            .extend_pairs(query.pairs());
        let response = self.send(self.http.get(url)).await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(&response);
            let body = read_capped(response, MAX_JSON).await.unwrap_or_default();
            return Err(self.refusal(status.as_u16(), retry_after, &body));
        }
        let body = read_capped(response, MAX_JSON).await?;
        read_search(&body)
    }

    /// The file at `path` (a [`Found::path`]): an archive or a subtitle,
    /// as SubDL has it.
    pub async fn download(&self, path: &str) -> Result<Vec<u8>, SiteError> {
        if !is_file_path(path) {
            return Err(SiteError::Unreadable("not a SubDL file's path".into()));
        }
        let mut url = self.account.download_url.clone();
        url.set_path(path);
        url.set_query(None);
        let request = self
            .http
            .get(url)
            .header("x-api-key", &self.account.api_key);
        let response = self.send(request).await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(&response);
            let body = read_capped(response, MAX_JSON).await.unwrap_or_default();
            return Err(self.refusal(status.as_u16(), retry_after, &body));
        }
        read_capped(response, spoolframe_media::subtitle_file::MAX_ARCHIVE_BYTES).await
    }

    /// A refusal, by its status and what its body says.
    fn refusal(&self, status: u16, retry_after: Option<u64>, body: &[u8]) -> SiteError {
        let message = serde_json::from_slice::<SearchAnswer>(body)
            .ok()
            .and_then(|a| a.error)
            .unwrap_or_default();
        match status {
            429 if says_quota(&message) => SiteError::QuotaExhausted { resets_at: None },
            429 => self.pacer.told_to_wait(retry_after),
            500.. => SiteError::Unreachable(format!("{status}")),
            _ if says_quota(&message) => SiteError::QuotaExhausted { resets_at: None },
            code => SiteError::Refused {
                status: code,
                message: message.chars().take(200).collect(),
            },
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, SiteError> {
        self.pacer.turn().await?;
        crate::subtitle_site::send(request).await
    }
}

/// `path` under the API at `base`.
fn endpoint(base: &url::Url, path: &str) -> url::Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an http(s) URL has a path")
        .pop_if_empty()
        .push(path);
    url
}

/// Whether SubDL's words say a quota or a limit is used up.
fn says_quota(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["limit", "quota", "exceeded"]
        .iter()
        .any(|w| message.contains(w))
}

/// The search answer's subtitles. `status: false` with SubDL's "can't
/// find" is no result; with anything else, a refusal.
fn read_search(body: &[u8]) -> Result<Vec<Found>, SiteError> {
    let answer: SearchAnswer =
        serde_json::from_slice(body).map_err(|e| SiteError::Unreadable(e.to_string()))?;
    if !answer.status {
        let message = answer.error.unwrap_or_default();
        let lower = message.to_ascii_lowercase();
        if lower.contains("find") || lower.contains("not found") {
            return Ok(Vec::new());
        }
        if says_quota(&message) {
            return Err(SiteError::QuotaExhausted { resets_at: None });
        }
        return Err(SiteError::Refused {
            status: 200,
            message: message.chars().take(200).collect(),
        });
    }
    let mut found = Vec::new();
    for entry in answer.subtitles {
        // The path alone: SubDL put the caller's key in the query.
        let Some(path) = entry
            .url
            .as_deref()
            .map(|u| u.split(['?', '#']).next().unwrap_or_default())
            .filter(|p| is_file_path(p))
        else {
            continue;
        };
        let Some(language) = entry.language.as_deref().and_then(language_of) else {
            continue;
        };
        let file = path.trim_start_matches("/subtitle/");
        let (id, extension) = match file.split_once('.') {
            Some((id, ext)) => (id, Some(ext.to_ascii_lowercase())),
            None => (file, None),
        };
        found.push(Found {
            id: id.to_owned(),
            path: path.to_owned(),
            release: entry
                .release_name
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect(),
            language,
            format: extension.filter(|e| e != "zip"),
            fps: entry
                .fps
                .and_then(|f| match f {
                    serde_json::Value::Number(n) => n.as_f64(),
                    serde_json::Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                })
                .filter(|f: &f64| f.is_finite() && *f > 0.0 && *f < 1000.0),
            hearing_impaired: entry.hi.unwrap_or(false),
        });
    }
    Ok(found)
}

/// Whether `path` is the shape SubDL gives a file: `/subtitle/` then two
/// numbers joined by `-`, and maybe an extension of letters and digits.
fn is_file_path(path: &str) -> bool {
    let Some(file) = path.strip_prefix("/subtitle/") else {
        return false;
    };
    let (id, extension) = file.split_once('.').unwrap_or((file, "zip"));
    let number = |s: &str| !s.is_empty() && s.len() <= 12 && s.bytes().all(|b| b.is_ascii_digit());
    id.split_once('-')
        .is_some_and(|(a, b)| number(a) && number(b))
        && (1..=4).contains(&extension.len())
        && extension.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// SubDL's language code as the server names it: `EN` is `en`, `BR_PT`
/// `pt-br`, `ZH_BG` (Big5, traditional) `zh-tw`.
pub fn language_of(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_lowercase();
    match code.as_str() {
        "br_pt" => Some("pt-br".into()),
        "zh_bg" => Some("zh-tw".into()),
        c if (2..=3).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphabetic()) => Some(code),
        _ => None,
    }
}

/// A language as SubDL takes it: `en` is `EN`, `pt-br` `BR_PT`, `zh-tw`
/// `ZH_BG`; any other region is its language's.
pub fn code_of(language: &str) -> String {
    match language.to_ascii_lowercase().as_str() {
        "pt-br" => "BR_PT".into(),
        "zh-tw" | "zh-hk" => "ZH_BG".into(),
        other => other
            .split('-')
            .next()
            .unwrap_or(other)
            .to_ascii_uppercase(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
