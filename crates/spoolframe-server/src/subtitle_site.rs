//! What the subtitle sites (PX-02) have in common: which one a result or a
//! download is from, how a call to one fails, and the pacing and capped
//! reading every call to one goes through. Each site's own API is in
//! [`crate::opensubtitles`] and [`crate::subdl`].

use std::time::{Duration, Instant};

/// A subtitle site, as the server calls it.
pub use spoolframe_types::SubtitleSource as Source;

/// Why a call to a site failed. Which site is the caller's to say: each
/// message reads after its name, "SubDL cannot be reached: …".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SiteError {
    /// It could not be reached, took too long, or failed on its side.
    #[error("cannot be reached: {0}")]
    Unreachable(String),
    /// It asked to wait this long before the next call.
    #[error("asks to wait {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    /// The account's downloads or requests are used up until `resets_at`
    /// (Unix seconds), when the site says.
    #[error("has no downloads left today for the server's account")]
    QuotaExhausted { resets_at: Option<i64> },
    /// It refused the key, the account or the request: the admin's to fix.
    #[error("refused the request ({status}): {message}")]
    Refused { status: u16, message: String },
    /// It answered with something that is not what it documents.
    #[error("answered with something unreadable: {0}")]
    Unreadable(String),
    /// The file is larger than the server takes.
    #[error("sent a file larger than {0} bytes")]
    TooLarge(usize),
}

/// The most of a site's own words a refusal keeps, in characters.
const MAX_MESSAGE: usize = 200;

/// A refusal with `status` and what the site said of it, cut short.
pub fn refused(status: u16, message: &str) -> SiteError {
    SiteError::Refused {
        status,
        message: message.chars().take(MAX_MESSAGE).collect(),
    }
}

impl SiteError {
    /// What a user (an admin too) is told of it, `site` named: never the
    /// site's own words (a refusal's text, what made an answer
    /// unreadable), which may name the account or be anything at all.
    /// Those are the caller's to log.
    pub fn user_message(&self, site: Source) -> String {
        match self {
            SiteError::Refused { .. } => format!("{site} refused the server's request"),
            SiteError::Unreadable(_) => format!("{site} answered with something unreadable"),
            // The server's own words.
            SiteError::Unreachable(_)
            | SiteError::RateLimited { .. }
            | SiteError::QuotaExhausted { .. }
            | SiteError::TooLarge(_) => format!("{site} {self}"),
        }
    }
}

/// A client for one site's calls, taking `timeout` each, that follows a
/// redirect only to a host `is_site` says is the site's own (at most
/// three): a key in a header or the query must not be sent elsewhere.
pub fn client(
    timeout: Duration,
    is_site: impl Fn(&url::Url) -> bool + Send + Sync + 'static,
) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("Spoolframe v", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 3 {
                attempt.error("too many redirects")
            } else if is_site(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("a client with rustls and no proxy settings builds")
}

/// `path` under the API at `base`.
pub fn endpoint(base: &url::Url, path: &str) -> url::Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an http(s) URL has a path")
        .pop_if_empty()
        .push(path);
    url
}

/// Calls to one site in turn: `interval` apart, and none while it said
/// to wait.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    /// When the last call was made, held while a call waits its turn.
    last_call: tokio::sync::Mutex<Option<Instant>>,
    /// No call until then: the site said to wait.
    wait_until: parking_lot::Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_call: tokio::sync::Mutex::new(None),
            wait_until: parking_lot::Mutex::new(None),
        }
    }

    /// Wait for this call's turn; `RateLimited` at once while the site
    /// said to wait.
    pub async fn turn(&self) -> Result<(), SiteError> {
        if let Some(until) = *self.wait_until.lock() {
            let now = Instant::now();
            if until > now {
                return Err(SiteError::RateLimited {
                    retry_after_ms: (until - now).as_millis() as u64,
                });
            }
        }
        let mut last = self.last_call.lock().await;
        if let Some(at) = *last {
            let next = at + self.interval;
            if next > Instant::now() {
                tokio::time::sleep_until(next.into()).await;
            }
        }
        *last = Some(Instant::now());
        Ok(())
    }

    /// The site answered 429: no call for `retry_after` seconds (1 if it
    /// did not say, at most an hour). What the caller is told.
    pub fn told_to_wait(&self, retry_after: Option<u64>) -> SiteError {
        let wait = Duration::from_secs(retry_after.unwrap_or(1).clamp(1, 3600));
        *self.wait_until.lock() = Some(Instant::now() + wait);
        SiteError::RateLimited {
            retry_after_ms: wait.as_millis() as u64,
        }
    }
}

/// `request` sent, a reqwest error told by its kind only: reqwest's names
/// the URL, which may carry the key, the hash and the item's ids.
pub async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, SiteError> {
    request.send().await.map_err(|e| {
        let kind = if e.is_timeout() {
            "it took too long"
        } else if e.is_connect() {
            "no connection"
        } else if e.is_redirect() {
            "it redirected away from the site"
        } else {
            "the request failed"
        };
        SiteError::Unreachable(kind.into())
    })
}

/// The `Retry-After` of `response`, in seconds.
pub fn retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// The body of `response`, refused past `max` bytes.
pub async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, SiteError> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(SiteError::TooLarge(max));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SiteError::Unreachable("the answer broke off".into()))?
    {
        if body.len() + chunk.len() > max {
            return Err(SiteError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user is told the server's own words of a failure, never the
    /// site's: a refusal may name the account.
    #[test]
    fn a_user_is_told_of_a_failure_in_the_server_s_words() {
        let site = Source::SubDl;
        let said = |e: SiteError| e.user_message(site);
        assert_eq!(
            said(refused(403, "key of account 48151623 is suspended")),
            "SubDL refused the server's request"
        );
        assert_eq!(
            said(SiteError::Unreadable("expected `,` near 48151623".into())),
            "SubDL answered with something unreadable"
        );
        assert_eq!(
            said(SiteError::Unreachable("it took too long".into())),
            "SubDL cannot be reached: it took too long"
        );
        let SiteError::Refused { message, .. } = refused(400, &"x".repeat(500)) else {
            unreachable!()
        };
        assert_eq!(message.len(), MAX_MESSAGE);
    }
}
