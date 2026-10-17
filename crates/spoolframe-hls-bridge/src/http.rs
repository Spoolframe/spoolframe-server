//! The HTTP side of the bridge, moved from the Apple app's
//! `spoolframe-ffi/src/bridge.rs` (bc2eb28) with its functions as they were
//! (PX-49): the playbacks are a [`Bridge`] value instead of globals, reads go
//! through [`Source`], and the caller binds the listener. Not formatted by
//! rustfmt, so it still diffs cleanly against the app's copy. Since: the
//! crate's own [`Error`], and [`BridgeOptions`] for what the app read from
//! its environment.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::Frame;
use hyper::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::bridge::{Bridge, BridgeOptions, Kind, MenuEntry, Stream};
use crate::source::{ManifestKind, Segment, Source};
use crate::Error;

/// The most one SDK read asks for: a few MB keeps the first bytes coming
/// quickly while a large range streams.
const READ: u64 = 4 << 20;

/// The most one HLS segment may hold. Several seconds of 4K at a high
/// bitrate is well under this; a server sending more (or saying it will) is
/// refused rather than filling memory (security/audit FFI-001).
pub const SEGMENT_MAX: u64 = 256 << 20;

/// How often one segment's encode may restart, and how long the whole read
/// may take, before it's given up.
const SEGMENT_RESTARTS: u32 = 8;
const SEGMENT_DEADLINE: Duration = Duration::from_secs(180);

/// What the bridge answers with: whole bodies, or a file range streamed
/// as the SDK reads it.
pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;

pub(crate) async fn accept<S: Source>(bridge: Bridge<S>, listener: TcpListener) {
    // Failing again and again, the socket is gone (taken back while the app
    // was suspended): end, and the next `base` makes another.
    let mut failures = 0;
    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                failures = 0;
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    // HTTP/1.1, kept alive as AVPlayer expects.
                    let served = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), hyper::service::service_fn(move |request| answer(bridge.clone(), request)))
                        .await;
                    if let Err(e) = served {
                        tracing::debug!(error = %e, "bridge connection ended");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "bridge accept failed");
                failures += 1;
                if failures >= 20 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

pub(crate) async fn answer<S: Source, B>(bridge: Bridge<S>, request: Request<B>) -> Result<Response<Body>, Infallible> {
    let head = request.method() == Method::HEAD;
    if !head && request.method() != Method::GET {
        return Ok(status(StatusCode::METHOD_NOT_ALLOWED));
    }
    let range = request.headers().get(RANGE).map(|v| v.to_str().ok().and_then(parse_range));
    let mut segments = request.uri().path().trim_start_matches('/').splitn(2, '/');
    let token = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    let stream = bridge.streams.lock().expect("never poisoned").get(token).cloned();
    let Some(stream) = stream else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    stream.asked();
    if let Some(hook) = &bridge.on_request { hook(token); }
    let log = bridge.options.log;
    let answer = async { match (&stream.kind, name) {
        (Kind::Direct { size, mime }, "file") => direct(stream.clone(), *size, mime, range, head),
        // A media segment streams as the server sends it (PX-46): AVPlayer
        // sees its first bytes early, and times the link, not this bridge.
        (Kind::Hls { .. }, _) if media_segment(name).is_some() => {
            let (variant, n) = media_segment(name).expect("matched");
            let asked = std::time::Instant::now();
            match open_segment(&stream, variant, n).await {
                Ok(opened) => streamed(stream.clone(), variant, n, opened, head, asked),
                Err(e) => {
                    note(&stream, format!("bridge: v{variant}/seg{n} failed after {} ms: {e}", asked.elapsed().as_millis()));
                    hls_failed(&stream, name, e, log)
                }
            }
        }
        (Kind::Hls { .. }, _) => {
            let asked = std::time::Instant::now();
            let answer = hls(&stream, name, &bridge.options).await;
            // A rung's init: how long the map took, for the playback log.
            if name.ends_with("init.mp4") {
                let took = asked.elapsed().as_millis();
                match &answer {
                    Ok(Some((_, body))) => note(&stream, format!("bridge: {name} {} bytes in {took} ms", body.len())),
                    Err(e) => note(&stream, format!("bridge: {name} failed after {took} ms: {e}")),
                    Ok(None) => {}
                }
            }
            match answer {
                Ok(Some((mime, body))) => whole(mime, body, head),
                Ok(None) => status(StatusCode::NOT_FOUND),
                Err(e) => hls_failed(&stream, name, e, log),
            }
        }
        _ => status(StatusCode::NOT_FOUND),
    }};
    Ok(tokio::select! {
        response = answer => response,
        _ = stream.cancelled() => status(StatusCode::GONE),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange { From(u64, Option<u64>), Suffix(u64) }

/// Reject malformed and multipart ranges; never underflow a length.
fn parse_range(value: &str) -> Option<ByteRange> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok()).flatten()
    };
    if start.is_empty() { return number(end).filter(|n| *n > 0).map(ByteRange::Suffix); }
    let start = number(start)?;
    let end = if end.is_empty() { None } else { Some(number(end)?) };
    if end.is_some_and(|end| end < start) { return None; }
    Some(ByteRange::From(start, end))
}

fn range_bounds(size: u64, range: Option<Option<ByteRange>>) -> Result<(u64, u64, bool), ()> {
    match range {
        None => Ok((0, size.saturating_sub(1), false)),
        Some(None) => Err(()),
        Some(Some(_)) if size == 0 => Err(()),
        Some(Some(ByteRange::From(start, end))) if start < size && end.is_none_or(|end| end >= start) =>
            Ok((start, end.unwrap_or(size - 1).min(size - 1), true)),
        Some(Some(ByteRange::Suffix(length))) if length > 0 => Ok((size.saturating_sub(length), size - 1, true)),
        _ => Err(()),
    }
}

fn direct<S: Source>(stream: Arc<Stream<S>>, size: u64, mime: &str, range: Option<Option<ByteRange>>, head: bool) -> Response<Body> {
    let Ok((start, end, partial)) = range_bounds(size, range) else {
        return Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(CONTENT_RANGE, format!("bytes */{size}"))
            .body(empty()).expect("a valid response");
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut response = Response::builder()
        .status(if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header(CONTENT_TYPE, mime)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_LENGTH, length);
    if partial {
        response = response.header(CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    if head || length == 0 {
        return response.body(empty()).expect("a valid response");
    }
    // The length is promised: a read that fails ends the body short, which
    // tells AVPlayer.
    let reads = futures_util::stream::try_unfold(start, move |offset| {
        let stream = stream.clone();
        async move {
            if offset > end {
                return Ok(None);
            }
            let want = (end - offset + 1).min(READ);
            let read = tokio::select! {
                read = stream.playback.read(offset, want) => read,
                _ = stream.cancelled() => return Err(std::io::Error::other("playback URL retired")),
            };
            match read {
                Ok(bytes) if !bytes.is_empty() && bytes.len() as u64 <= want => {
                    let next = offset + bytes.len() as u64;
                    Ok(Some((Frame::data(Bytes::from(bytes)), next)))
                }
                Ok(bytes) if bytes.is_empty() => Ok(None),
                Ok(_) => Err(std::io::Error::other("read exceeded the requested range")),
                Err(e) => {
                    if matches!(e, Error::Stopped) {
                        stream.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    if matches!(e, Error::Expired) {
                        stream.expired.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    tracing::warn!(error = %e, offset, "bridge file read failed");
                    Err(std::io::Error::other(e.to_string()))
                }
            }
        }
    });
    response.body(StreamBody::new(reads).boxed_unsync()).expect("a valid response")
}

/// A playlist, segment or subtitle segment, whole.
async fn hls<S: Source>(stream: &Stream<S>, name: &str, options: &BridgeOptions) -> Result<Option<(&'static str, Vec<u8>)>, Error> {
    const PLAYLIST: &str = "application/vnd.apple.mpegurl";
    let log = options.log;
    let manifest = |kind, variant| async move {
        let text = stream.playback.manifest(kind, variant).await?;
        confine(&text, log).map(|()| text.into_bytes())
    };
    // A ladder rung's own: `v<n>/media.m3u8`, `v<n>/init.mp4`, `v<n>/seg<k>.m4s`.
    if let Some((rung, file)) = name.split_once('/') {
        let Some(variant) = number(rung, "v", "") else { return Ok(None) };
        return Ok(Some(match file {
            "media.m3u8" => (PLAYLIST, manifest(ManifestKind::Media, variant).await?),
            "init.mp4" => ("video/mp4", segment(stream, variant, 0).await?),
            _ => match number(file, "seg", ".m4s") {
                Some(n) => ("video/iso.segment", segment(stream, variant, n).await?),
                None => return Ok(None),
            },
        }));
    }
    Ok(Some(match name {
        "master.m3u8" => {
            let master = manifest(ManifestKind::Multivariant, 0).await?;
            let menu = stream.menu.lock().expect("never poisoned");
            let body = match menu.as_deref() {
                Some(entries) => with_menu(&String::from_utf8_lossy(&master), entries, stream.subtitle).into_bytes(),
                None => master,
            };
            (PLAYLIST, debug_rewrite(body, options.master_rewrite.as_deref()))
        }
        // As the server's own segments begin, with no cues.
        "empty.vtt" => ("text/vtt", b"WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n".to_vec()),
        "media.m3u8" => (PLAYLIST, manifest(ManifestKind::Media, 0).await?),
        "subtitles.m3u8" => (PLAYLIST, manifest(ManifestKind::Subtitles, 0).await?),
        "init.mp4" => ("video/mp4", segment(stream, 0, 0).await?),
        _ => {
            if let Some(n) = number(name, "seg", ".m4s") {
                ("video/iso.segment", segment(stream, 0, n).await?)
            } else if number(name, "subtitles-", ".m3u8").is_some() {
                // A track not playing: one empty cue file over the length.
                let body = stream
                    .placeholder
                    .get_or_try_init(|| async {
                        let media = manifest(ManifestKind::Media, 0).await?;
                        Ok::<_, Error>(placeholder(&String::from_utf8_lossy(&media)).into_bytes())
                    })
                    .await?;
                (PLAYLIST, body.clone())
            } else if let Some(n) = number(name, "subs", ".vtt") {
                ("text/vtt", stream.playback.read_subtitle_segment(n).await?.bytes)
            } else {
                return Ok(None);
            }
        }
    }))
}

/// `master` with its subtitle rendition replaced by one per entry,
/// `selected` the one the server plays: written once, before the first
/// variant (a ladder has several; each names the group, and none has
/// closed captions).
fn with_menu(master: &str, entries: &[MenuEntry], selected: Option<u32>) -> String {
    use std::fmt::Write;
    let yes = |b: bool| if b { "YES" } else { "NO" };
    // No rendition of the server's: the subtitle is burned into the
    // picture (or there's none), so even the playing track is a placeholder.
    let served = master.contains("#EXT-X-MEDIA:TYPE=SUBTITLES");
    let mut out = String::with_capacity(master.len() + entries.len() * 160);
    let mut written = false;
    for line in master.lines() {
        if line.starts_with("#EXT-X-MEDIA:TYPE=SUBTITLES") {
            continue;
        }
        let stream_inf = line.starts_with("#EXT-X-STREAM-INF:");
        if stream_inf && !written {
            written = true;
            for entry in entries {
                let playing = selected == Some(entry.index);
                let _ = write!(
                    out,
                    "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",DEFAULT={on},AUTOSELECT={on},FORCED={},URI=\"",
                    attribute(&entry.name),
                    yes(entry.forced),
                    on = yes(playing),
                );
                let _ = if playing && served {
                    write!(out, "subtitles.m3u8\"")
                } else {
                    write!(out, "subtitles-{}.m3u8\"", entry.index)
                };
                if let Some(language) = entry.language.as_deref().and_then(language_tag) {
                    let _ = write!(out, ",LANGUAGE=\"{language}\"");
                }
                out.push('\n');
            }
        }
        out.push_str(line);
        if stream_inf && !line.contains("SUBTITLES=") {
            out.push_str(",SUBTITLES=\"subs\"");
        }
        // The menu is every option: CEA-608 captions in a copied video
        // would be one more, and the entries' places would no longer be
        // the options'. (A server from 0.12.0 says so itself.)
        if stream_inf && !line.contains("CLOSED-CAPTIONS=") {
            out.push_str(",CLOSED-CAPTIONS=NONE");
        }
        out.push('\n');
    }
    out
}

/// A subtitle playlist with no cues, as long as `media` (the media
/// playlist).
fn placeholder(media: &str) -> String {
    let total: f64 = media
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|l| l.split(',').next()?.parse::<f64>().ok())
        .sum();
    let total = total.max(1.0);
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:{total:.3},\nempty.vtt\n#EXT-X-ENDLIST\n",
        total.ceil() as u64
    )
}

/// The server's playlist may only name this playback's own resources, as
/// the bridge serves them: no scheme, host, path or `..`, in a URI line or a
/// `URI="…"` attribute. Else AVPlayer would fetch from wherever a server
/// pointed it, outside the pinned connection (security/audit FFI-002).
fn confine(playlist: &str, log: bool) -> Result<(), Error> {
    let refuse = |what: &str| {
        // `BridgeOptions::log`: say what was refused, for debugging a
        // server's playlists.
        if log {
            tracing::info!(reference = what, "bridge: refused playlist reference");
        }
        Err(Error::Other(format!("playlist names {what:?}, not one of this playback's resources")))
    };
    for line in playlist.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            let mut rest = line;
            while let Some(at) = rest.find("URI=\"") {
                let value = &rest[at + 5..];
                let Some(end) = value.find('"') else { return refuse(value) };
                if !routable(&value[..end]) {
                    return refuse(&value[..end]);
                }
                rest = &value[end + 1..];
            }
        } else if !routable(line) {
            return refuse(line);
        }
    }
    Ok(())
}

/// `BridgeOptions::master_rewrite`, `"from>to;from>to"`: replace text in
/// the master playlist, to find which attribute a player refuses. A
/// debugging switch: nothing sets it in a shipped app.
fn debug_rewrite(master: Vec<u8>, rules: Option<&str>) -> Vec<u8> {
    let Some(rules) = rules else { return master };
    let mut text = String::from_utf8_lossy(&master).into_owned();
    for rule in rules.split(';') {
        if let Some((from, to)) = rule.split_once('>') {
            text = text.replace(from, to);
        }
    }
    tracing::info!(master = %text, "bridge: master rewritten");
    text.into_bytes()
}

/// A name `hls` serves.
fn routable(name: &str) -> bool {
    // The bridge routes on the path: a query or fragment after a name is
    // still that resource.
    let name = name.split(['?', '#']).next().unwrap_or_default();
    // A ladder rung's playlist and media (PX-21).
    if let Some((rung, file)) = name.split_once('/') {
        return number(rung, "v", "").is_some()
            && (matches!(file, "media.m3u8" | "init.mp4") || number(file, "seg", ".m4s").is_some());
    }
    matches!(name, "media.m3u8" | "subtitles.m3u8" | "init.mp4" | "empty.vtt")
        || number(name, "seg", ".m4s").is_some()
        || number(name, "subs", ".vtt").is_some()
        || number(name, "subtitles-", ".m3u8").is_some()
}

/// A subtitle track's name inside a quoted playlist attribute: no quotes,
/// no line breaks or other controls.
fn attribute(text: &str) -> String {
    text.chars()
        .map(|c| if c == '"' { '\'' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// A language tag as BCP 47 spells them ("en", "pt-BR"), else none.
fn language_tag(text: &str) -> Option<&str> {
    let ok = !text.is_empty()
        && text.len() <= 35
        && text.split('-').all(|part| !part.is_empty() && part.len() <= 8 && part.chars().all(|c| c.is_ascii_alphanumeric()));
    ok.then_some(text)
}

fn number(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()
}

/// Segment `n` of ladder rung `variant` (0 for one encode), whole. A read
/// past what's encoded waits on the server and may come back empty: ask
/// again. A restart (a far seek) moves the epoch on: read the segment again
/// from its start.
async fn segment<S: Source>(stream: &Stream<S>, variant: u32, n: u32) -> Result<Vec<u8>, Error> {
    let Kind::Hls { epoch } = &stream.kind else {
        return Err(Error::Other("not an HLS playback".into()));
    };
    let mut current = *epoch.lock().expect("never poisoned");
    let mut identity = stream.playback.changes();
    let mut body = Vec::new();
    let mut waits = 0;
    let mut restarts = 0;
    let started = std::time::Instant::now();
    let too_big = || Error::Other(format!("segment {n} is bigger than {} MiB", SEGMENT_MAX >> 20));
    loop {
        if started.elapsed() > SEGMENT_DEADLINE {
            return Err(Error::Other(format!("segment {n} took too long")));
        }
        if identity.has_changed().unwrap_or(true) {
            body.clear();
            identity = stream.playback.changes();
        }
        let deadline = tokio::time::Instant::from_std(started + SEGMENT_DEADLINE);
        let read = tokio::time::timeout_at(deadline, stream.playback.read_segment(variant, n, body.len() as u64, READ))
            .await.map_err(|_| Error::Other(format!("segment {n} took too long")))?;
        if identity.has_changed().unwrap_or(true) {
            body.clear();
            identity = stream.playback.changes();
            continue;
        }
        match read {
            Ok(chunk) => {
                if chunk.size > SEGMENT_MAX {
                    return Err(too_big());
                }
                if chunk.epoch != current {
                    restarts += 1;
                    if restarts > SEGMENT_RESTARTS {
                        return Err(Error::Other(format!("segment {n} keeps restarting")));
                    }
                    current = chunk.epoch;
                    *epoch.lock().expect("never poisoned") = current;
                    body.clear();
                    continue;
                }
                if body.len() as u64 + chunk.bytes.len() as u64 > SEGMENT_MAX {
                    return Err(too_big());
                }
                body.extend_from_slice(&chunk.bytes);
                if chunk.complete && body.len() as u64 >= chunk.size {
                    return Ok(body);
                }
                if chunk.bytes.is_empty() {
                    waits += 1;
                    // Each empty read is the server waiting up to 10 s.
                    if waits > 12 {
                        return Err(Error::Other(format!("segment {n} isn't coming")));
                    }
                }
            }
            // Resumed by the SDK part-way through: the rest can't come from
            // the new encode, so the segment again from its start.
            Err(Error::Resumed { .. }) => {
                restarts += 1;
                if restarts > SEGMENT_RESTARTS {
                    return Err(Error::Other(format!("segment {n} keeps restarting")));
                }
                body.clear();
            }
            Err(Error::Restarted { epoch: next }) => {
                restarts += 1;
                if restarts > SEGMENT_RESTARTS {
                    return Err(Error::Other(format!("segment {n} keeps restarting")));
                }
                current = next;
                *epoch.lock().expect("never poisoned") = next;
                body.clear();
            }
            Err(e) => return Err(e),
        }
    }
}

/// A line for the app's playback log about `stream`.
fn note<S: Source>(stream: &Stream<S>, line: String) {
    let mut notes = stream.notes.lock().expect("never poisoned");
    if notes.len() < 400 {
        notes.push(line);
    }
}

/// What a failed HLS read answers.
fn hls_failed<S: Source>(stream: &Stream<S>, name: &str, error: Error, log: bool) -> Response<Body> {
    match error {
        // Not ready, or rewritten while read: AVPlayer asks again.
        Error::NotReady { .. } => status(StatusCode::SERVICE_UNAVAILABLE),
        // The playback has no such resource (PX-49: the crate's own error).
        Error::NotFound => status(StatusCode::NOT_FOUND),
        Error::Stopped => {
            stream.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            status(StatusCode::GONE)
        }
        // Gone for good: AVPlayer stops asking, and the app starts a new
        // playback where this one was.
        Error::Expired => {
            stream.expired.store(true, std::sync::atomic::Ordering::Relaxed);
            status(StatusCode::GONE)
        }
        e => {
            if log {
                tracing::info!(error = %e, name, "bridge: read failed");
            }
            tracing::warn!(error = %e, name, "bridge read failed");
            status(StatusCode::BAD_GATEWAY)
        }
    }
}

/// `seg<k>.m4s` (variant 0) or `v<n>/seg<k>.m4s`: the variant and segment.
fn media_segment(name: &str) -> Option<(u32, u32)> {
    match name.split_once('/') {
        Some((rung, file)) => Some((number(rung, "v", "")?, number(file, "seg", ".m4s")?)),
        None => Some((0, number(name, "seg", ".m4s")?)),
    }
}

/// A segment's answer, open: its body arriving, and when the whole segment
/// must be in by.
struct Opened<S: Source> {
    identity: tokio::sync::watch::Receiver<S::Snapshot>,
    body: S::Segment,
    deadline: tokio::time::Instant,
}

/// Open segment `n` of `variant` from its start, waiting through a restart
/// before anything is promised to AVPlayer.
async fn open_segment<S: Source>(stream: &Stream<S>, variant: u32, n: u32) -> Result<Opened<S>, Error> {
    let Kind::Hls { epoch } = &stream.kind else {
        return Err(Error::Other("not an HLS playback".into()));
    };
    let deadline = tokio::time::Instant::now() + SEGMENT_DEADLINE;
    let mut restarts = 0;
    loop {
        let identity = stream.playback.changes();
        let open = tokio::time::timeout_at(deadline, stream.playback.segment_stream(variant, n, 0, SEGMENT_MAX));
        match open.await.map_err(|_| Error::Other(format!("segment {n} took too long")))? {
            Ok(body) => {
                if body.size() > SEGMENT_MAX {
                    return Err(Error::Other(format!("segment {n} is bigger than {} MiB", SEGMENT_MAX >> 20)));
                }
                *epoch.lock().expect("never poisoned") = body.epoch();
                if identity.has_changed().unwrap_or(true) {
                    restarts += 1;
                    if restarts > SEGMENT_RESTARTS { return Err(Error::Other("playback keeps changing".into())); }
                    continue;
                }
                return Ok(Opened { body, deadline, identity });
            }
            Err(Error::Resumed { .. }) | Err(Error::Restarted { .. }) => {
                restarts += 1;
                if restarts > SEGMENT_RESTARTS {
                    return Err(Error::Other(format!("segment {n} keeps restarting")));
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// A streamed segment's progress.
struct Delivery<S: Source> {
    read_id: u64,
    identity: tokio::sync::watch::Receiver<S::Snapshot>,
    stream: Arc<Stream<S>>,
    variant: u32,
    n: u32,
    body: S::Segment,
    epoch: u32,
    /// Bytes handed to AVPlayer.
    offset: u64,
    /// The segment's whole size, once an answer said it's complete.
    total: Option<u64>,
    /// The length promised in the headers, if one was.
    promised: Option<u64>,
    deadline: tokio::time::Instant,
    asked: std::time::Instant,
    first_byte: Option<Duration>,
    reopens: u32,
    waits: u32,
    /// Its playback-log line is written.
    noted: bool,
}

impl<S: Source> Delivery<S> {
    fn short(&self, why: String) -> std::io::Error {
        tracing::warn!(segment = self.n, variant = self.variant, offset = self.offset, why, "segment ended short");
        note(
            &self.stream,
            format!(
                "bridge: read {} epoch {} v{}/seg{} ended at {} after {} ms: {why}",
                self.read_id,
                self.epoch,
                self.variant,
                self.n,
                self.offset,
                self.asked.elapsed().as_millis()
            ),
        );
        std::io::Error::other(why)
    }

    fn done(&self) -> bool {
        self.total.is_some_and(|t| self.offset >= t)
    }

    /// The segment's line in the playback log, once: when its last bytes
    /// go out (a body with a promised length isn't polled after them).
    fn delivered(&mut self) {
        if self.noted {
            return;
        }
        self.noted = true;
        note(
            &self.stream,
            format!(
                "bridge: read {} epoch {} v{}/seg{} {} bytes · first byte {} ms · whole {} ms",
                self.read_id,
                self.epoch,
                self.variant,
                self.n,
                self.offset,
                self.first_byte.unwrap_or_default().as_millis(),
                self.asked.elapsed().as_millis()
            ),
        );
    }

    /// The next piece for AVPlayer, reading on through answers that end
    /// before the segment does; `None` once it's all handed on.
    async fn next(&mut self) -> Result<Option<Bytes>, std::io::Error> {
        loop {
            if self.done() {
                self.delivered();
                return Ok(None);
            }
            if self.identity.has_changed().unwrap_or(true) {
                return Err(self.short("playback identity changed".into()));
            }
            let piece = tokio::select! {
                piece = tokio::time::timeout_at(self.deadline, self.body.next()) => piece,
                _ = self.stream.cancelled() => return Err(self.short("playback URL retired".into())),
            };
            if self.identity.has_changed().unwrap_or(true) {
                return Err(self.short("playback identity changed during read".into()));
            }
            let Ok(piece) = piece else {
                return Err(self.short(format!("took over {} s", SEGMENT_DEADLINE.as_secs())));
            };
            match piece {
                Ok(Some(bytes)) if bytes.is_empty() => continue,
                Ok(Some(bytes)) => {
                    self.offset += bytes.len() as u64;
                    if self.first_byte.is_none() {
                        self.first_byte = Some(self.asked.elapsed());
                        note(&self.stream, format!("bridge: read {} v{}/seg{} epoch {} first upstream bytes {} ms; forwarding {} bytes",
                            self.read_id, self.variant, self.n, self.epoch, self.asked.elapsed().as_millis(), bytes.len()));
                    }
                    if self.promised.is_some_and(|p| self.offset > p) || self.offset > SEGMENT_MAX {
                        return Err(self.short("longer than it said".into()));
                    }
                    if self.done() {
                        // The SDK counts a segment whole (for its HLS
                        // step-down) only on the read past its last bytes;
                        // hyper won't ask for that once it has the length
                        // it was promised, so take it here (it ends at once).
                        let _ = tokio::time::timeout(Duration::from_secs(2), self.body.next()).await;
                        self.delivered();
                    }
                    return Ok(Some(Bytes::from(bytes)));
                }
                // This answer is all in: the segment may go on in another.
                Ok(None) => self.reopen().await?,
                Err(Error::Resumed { .. }) => {
                    return Err(self.short("the playback was resumed in another encode".into()));
                }
                // A seek, a track or quality change, or a step-down replaced
                // the encode mid-segment: the rest would splice in another
                // encode, so cut it short and AVPlayer asks again from 0.
                Err(Error::Restarted { epoch }) => {
                    if let Kind::Hls { epoch: current } = &self.stream.kind {
                        *current.lock().expect("never poisoned") = epoch;
                    }
                    return Err(self.short("its encode restarted".into()));
                }
                Err(e @ (Error::Stopped | Error::Expired)) => {
                    let flag = if matches!(e, Error::Stopped) { &self.stream.stopped } else { &self.stream.expired };
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(self.short(e.to_string()));
                }
                // A lost connection or a stall part-way: on from here.
                Err(e) => {
                    self.reopens += 1;
                    if self.reopens > SEGMENT_RESTARTS {
                        return Err(self.short(format!("keeps failing: {e}")));
                    }
                    self.reopen().await?;
                }
            }
        }
    }

    /// Ask for the rest of the segment from `offset`.
    async fn reopen(&mut self) -> Result<(), std::io::Error> {
        if self.identity.has_changed().unwrap_or(true) {
            return Err(self.short("playback identity changed before reopen".into()));
        }
        // On from this reader's own answer, under the identity it opened
        // with (another reader of the segment can't redirect it).
        let open = self.body.reopen(SEGMENT_MAX - self.offset);
        let open = tokio::select! {
            open = tokio::time::timeout_at(self.deadline, open) => open,
            _ = self.stream.cancelled() => return Err(self.short("playback URL retired".into())),
        };
        let Ok(open) = open else {
            return Err(self.short(format!("took over {} s", SEGMENT_DEADLINE.as_secs())));
        };
        let body = match open {
            Ok(body) => body,
            Err(Error::Restarted { epoch }) => {
                if let Kind::Hls { epoch: current } = &self.stream.kind {
                    *current.lock().expect("never poisoned") = epoch;
                }
                return Err(self.short("its encode restarted".into()));
            }
            Err(e) => return Err(self.short(e.to_string())),
        };
        if self.identity.has_changed().unwrap_or(true) {
            return Err(self.short("playback identity changed during reopen".into()));
        }
        if body.epoch() != self.epoch {
            if let Kind::Hls { epoch: current } = &self.stream.kind {
                *current.lock().expect("never poisoned") = body.epoch();
            }
            return Err(self.short("its encode restarted".into()));
        }
        if body.complete() {
            if self.promised.is_some_and(|p| p != body.size()) {
                return Err(self.short(format!("its size changed to {}", body.size())));
            }
            self.total = Some(body.size());
        } else if body.sent() == 0 {
            // Not more written yet: the server waited (up to 10 s) for it.
            self.waits += 1;
            if self.waits > 12 {
                return Err(self.short("the rest isn't coming".into()));
            }
        } else {
            self.waits = 0;
        }
        self.body = body;
        Ok(())
    }
}

/// Segment `n` of `variant` as the SDK hands it on: the headers once the
/// answer comes, then the body piece by piece. A whole segment promises its
/// length; one still being encoded is sent chunked, answer after answer,
/// until it's whole. A restart part-way can't go on from the new encode:
/// the body ends short, and AVPlayer asks again.
fn streamed<S: Source>(
    stream: Arc<Stream<S>>,
    variant: u32,
    n: u32,
    opened: Opened<S>,
    head: bool,
    asked: std::time::Instant,
) -> Response<Body> {
    let Opened { body, deadline, identity } = opened;
    static NEXT_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let read_id = NEXT_READ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    note(&stream, format!("bridge: read {read_id} v{variant}/seg{n} epoch {} headers {} ms", body.epoch(), asked.elapsed().as_millis()));
    let promised = body.complete().then(|| body.size());
    let mut response = Response::builder()
        .header(CONTENT_TYPE, "video/iso.segment")
        .header(CACHE_CONTROL, "no-store");
    if let Some(length) = promised {
        response = response.header(CONTENT_LENGTH, length);
    }
    if head || promised == Some(0) {
        return response.body(empty()).expect("a valid response");
    }
    let delivery = Delivery {
        read_id,
        identity,
        stream,
        variant,
        n,
        epoch: body.epoch(),
        total: promised,
        body,
        offset: 0,
        promised,
        deadline,
        asked,
        first_byte: None,
        reopens: 0,
        waits: 0,
        noted: false,
    };
    let pieces = futures_util::stream::try_unfold(delivery, |mut delivery| async move {
        Ok(delivery.next().await?.map(|bytes| (Frame::data(bytes), delivery)))
    });
    response.body(StreamBody::new(pieces).boxed_unsync()).expect("a valid response")
}

/// A whole body: a playlist, a segment, a subtitle file.
fn whole(mime: &'static str, body: Vec<u8>, head: bool) -> Response<Body> {
    let length = body.len();
    let body = if head { empty() } else { Full::new(Bytes::from(body)).map_err(|never| match never {}).boxed_unsync() };
    Response::builder()
        .header(CONTENT_TYPE, mime)
        .header(CONTENT_LENGTH, length)
        .header(CACHE_CONTROL, "no-store")
        .body(body)
        .expect("a valid response")
}

fn status(code: StatusCode) -> Response<Body> {
    let reason = code.canonical_reason().unwrap_or_default();
    let mut response = whole("text/plain", reason.as_bytes().to_vec(), false);
    *response.status_mut() = code;
    response
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlists_name_only_their_own_resources() {
        let good = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.0,\nseg0.m4s\nseg12.m4s\n#EXT-X-ENDLIST\n";
        assert!(confine(good, false).is_ok());
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"en\",URI=\"subtitles.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n";
        assert!(confine(master, false).is_ok());
        assert!(confine("#EXTM3U\nseg3.m4s?epoch=2\n#EXT-X-MAP:URI=\"init.mp4?v=1\"\n", false).is_ok());
        // A ladder's rungs (PX-21).
        assert!(confine("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv0/media.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2\nv3/media.m3u8\n", false).is_ok());
        assert!(confine("#EXTM3U\n#EXT-X-MAP:URI=\"v1/init.mp4\"\nv1/seg4.m4s\n", false).is_ok());
        for bad in [
            "#EXTM3U\nhttps://evil.example/seg0.m4s\n",
            "#EXTM3U\n//evil.example/seg0.m4s\n",
            "#EXTM3U\n../other/seg0.m4s\n",
            "#EXTM3U\n/token/seg0.m4s\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"http://127.0.0.1:22/\"\n",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://evil.example/k\"\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\n",
            "#EXTM3U\nfile:///etc/passwd\n",
            "#EXTM3U\nhttps://evil.example/seg0.m4s?x\n",
            "#EXTM3U\nv1/../../x\n",
            "#EXTM3U\nvx/media.m3u8\n",
            "#EXTM3U\nv1/master.m3u8\n",
            "#EXTM3U\nv1/v2/seg0.m4s\n",
        ] {
            assert!(confine(bad, false).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn subtitle_fields_cant_break_the_playlist() {
        assert_eq!(attribute("Eng\"lish\n#EXT-X-KEY:URI=\"x\""), "Eng'lish#EXT-X-KEY:URI='x'");
        assert_eq!(language_tag("pt-BR"), Some("pt-BR"));
        assert_eq!(language_tag("en\",URI=\"http://x"), None);
        assert_eq!(language_tag(""), None);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-1"), Some(ByteRange::From(0, Some(1))));
        assert_eq!(parse_range("bytes=100-"), Some(ByteRange::From(100, None)));
        assert_eq!(parse_range("items=0-1"), None);
    }

    #[test]
    fn segment_names() {
        assert_eq!(number("seg12.m4s", "seg", ".m4s"), Some(12));
        assert_eq!(number("subs3.vtt", "subs", ".vtt"), Some(3));
        assert_eq!(number("seg.m4s", "seg", ".m4s"), None);
    }

    fn entry(index: u32, name: &str) -> MenuEntry {
        MenuEntry { index, name: name.into(), language: Some("en".into()), forced: false }
    }

    #[test]
    fn the_menu_names_every_track_the_playing_one_on_the_servers_playlist() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"subtitles.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"subs\"\nmedia.m3u8\n";
        let out = with_menu(master, &[entry(0, "English"), entry(1, "French")], Some(1));
        assert_eq!(out.matches("TYPE=SUBTITLES").count(), 2);
        assert!(out.contains("NAME=\"English\",DEFAULT=NO,AUTOSELECT=NO,FORCED=NO,URI=\"subtitles-0.m3u8\""));
        assert!(out.contains("NAME=\"French\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles.m3u8\""));
        assert_eq!(out.matches("SUBTITLES=\"subs\"").count(), 1);
    }

    #[test]
    fn with_subtitles_off_the_stream_still_joins_the_group() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n";
        let out = with_menu(master, &[entry(0, "English")], None);
        assert!(out.contains("URI=\"subtitles-0.m3u8\""));
        // Burned in: picked, but nothing to read.
        let out = with_menu(master, &[entry(0, "English")], Some(0));
        assert!(out.contains("DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles-0.m3u8\""));
        assert!(out.contains("#EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"subs\""));
    }

    #[test]
    fn a_placeholder_lasts_as_long_as_the_media() {
        let media = "#EXTM3U\n#EXTINF:0.5,\nseg1.m4s\n#EXTINF:1.25,\nseg2.m4s\n";
        let out = placeholder(media);
        assert!(out.contains("#EXTINF:1.750,\nempty.vtt"));
        assert!(out.contains("#EXT-X-TARGETDURATION:2"));
    }

    #[test]
    fn menu_once_for_a_ladder() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"subtitles.m3u8\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=9000000,SUBTITLES=\"subs\"\nv0/media.m3u8\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=4000000,SUBTITLES=\"subs\"\nv1/media.m3u8\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=1000000\nv2/media.m3u8\n";
        let entries = [
            MenuEntry { index: 0, name: "English".into(), language: Some("en".into()), forced: false },
            MenuEntry { index: 1, name: "Spanish".into(), language: Some("es".into()), forced: false },
        ];
        let out = with_menu(master, &entries, Some(0));
        assert_eq!(out.matches("NAME=\"English\"").count(), 1);
        assert_eq!(out.matches("NAME=\"Spanish\"").count(), 1);
        assert_eq!(out.matches("#EXT-X-STREAM-INF:").count(), 3);
        assert_eq!(out.matches("SUBTITLES=\"subs\"").count(), 3);
        assert!(out.find("NAME=\"Spanish\"").unwrap() < out.find("#EXT-X-STREAM-INF:").unwrap());
    }

}

// The range test from `bridge_tests.rs`, which needs no SDK.
#[cfg(test)]
#[path = "range_tests.rs"]
mod range_tests;

#[cfg(all(test, feature = "sdk"))]
#[path = "bridge_tests.rs"]
mod reader_tests;
