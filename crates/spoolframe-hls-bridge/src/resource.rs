//! What a playback's HLS names read, whole, with no transport: the
//! playlists (confined to the playback's own resources, the master with
//! its subtitle menu), segments read through restarts, and subtitle
//! segments. The HTTP side (`http.rs`, feature `http`) answers requests
//! with it, and [`crate::Bridge::resource`] gives it to a caller that
//! reads by name itself (Media3's `DataSource`). Not formatted by rustfmt.

use std::time::Duration;

use crate::bridge::{BridgeOptions, Kind, MenuEntry, Stream};
use crate::source::{ManifestKind, Source};
use crate::Error;

/// The most one SDK read asks for: a few MB keeps the first bytes coming
/// quickly while a large range streams.
pub(crate) const READ: u64 = 4 << 20;

/// The most one HLS segment may hold. Several seconds of 4K at a high
/// bitrate is well under this; a server sending more (or saying it will) is
/// refused rather than filling memory.
pub const SEGMENT_MAX: u64 = 256 << 20;

/// How often one segment's encode may restart, and how long the whole read
/// may take, before it's given up.
pub(crate) const SEGMENT_RESTARTS: u32 = 8;
pub(crate) const SEGMENT_DEADLINE: Duration = Duration::from_secs(180);

/// A playlist, segment or subtitle segment, whole.
pub(crate) async fn hls<S: Source>(stream: &Stream<S>, name: &str, options: &BridgeOptions) -> Result<Option<(&'static str, Vec<u8>)>, Error> {
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
pub(crate) fn with_menu(master: &str, entries: &[MenuEntry], selected: Option<u32>) -> String {
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
pub(crate) fn placeholder(media: &str) -> String {
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
/// pointed it, outside the pinned connection.
pub(crate) fn confine(playlist: &str, log: bool) -> Result<(), Error> {
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
pub(crate) fn debug_rewrite(master: Vec<u8>, rules: Option<&str>) -> Vec<u8> {
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
pub(crate) fn routable(name: &str) -> bool {
    // The bridge routes on the path: a query or fragment after a name is
    // still that resource.
    let name = name.split(['?', '#']).next().unwrap_or_default();
    // A ladder rung's playlist and media.
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
pub(crate) fn attribute(text: &str) -> String {
    text.chars()
        .map(|c| if c == '"' { '\'' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// A language tag as BCP 47 spells them ("en", "pt-BR"), else none.
pub(crate) fn language_tag(text: &str) -> Option<&str> {
    let ok = !text.is_empty()
        && text.len() <= 35
        && text.split('-').all(|part| !part.is_empty() && part.len() <= 8 && part.chars().all(|c| c.is_ascii_alphanumeric()));
    ok.then_some(text)
}

pub(crate) fn number(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()
}

/// Segment `n` of ladder rung `variant` (0 for one encode), whole. A read
/// past what's encoded waits on the server and may come back empty: ask
/// again. A restart (a far seek) moves the epoch on: read the segment again
/// from its start.
pub(crate) async fn segment<S: Source>(stream: &Stream<S>, variant: u32, n: u32) -> Result<Vec<u8>, Error> {
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
pub(crate) fn note<S: Source>(stream: &Stream<S>, line: String) {
    let mut notes = stream.notes.lock().expect("never poisoned");
    if notes.len() < 400 {
        notes.push(line);
    }
}
