//! Download add-ons (feature `spoolframe:download-addons`): what may be
//! added to a download, a subtitle read as WebVTT outside a playback, and
//! an audio track downloaded as a file of its own, against a server with
//! real ffmpeg and ffprobe, on a film made at test time whose timestamps
//! start at 10 s.

use spoolframe_protocol::downloads::{AddonAudio, AddonSubtitle, DownloadState};
use spoolframe_protocol::{Error, Feature, Features, Reply, Request, Response, SessionId};
use spoolframe_types::{Container, Platform};

use crate::downloads_wire::{PASSWORD, events, moves_until_settled, probe, read_from, server};
use crate::support::{ServerOptions, TestClient, TestServer, movie};

/// Write the movie: 4 s of H.264 with a DTS 5.1 track and a French AAC
/// stereo one, and an English SubRip cue from 0.5 s to 1.5 s, in
/// Matroska, every stream's timestamps moved to start at 10 s.
pub(crate) fn write_offset_film(server: &TestServer) {
    let dir = server.media.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let srt = dir.join("cue.srt");
    std::fs::write(&srt, "1\n00:00:00,500 --> 00:00:01,500\nHello\n").unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000"])
        .arg("-i")
        .arg(&srt)
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s"])
        .args(["-t", "4", "-pix_fmt", "yuv420p", "-c:v", "libx264"])
        .args(["-c:a:0", "dca", "-c:a:1", "aac", "-strict", "-2"])
        .args(["-ac:a:0", "6", "-ac:a:1", "2", "-c:s", "srt"])
        .args([
            "-metadata:s:a:1",
            "language=fre",
            "-metadata:s:s:0",
            "language=eng",
        ])
        .args(["-output_ts_offset", "10"])
        .arg(&server.media)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

/// Alice on an app on `platform` whose `Hello` named `features`.
async fn alice(
    server: &TestServer,
    platform: Platform,
    features: Features,
) -> (TestClient, SessionId) {
    let client = server.client().await;
    let info = client.hello_with_features(platform, features).await;
    assert_eq!(info.features, features & Features::all());
    let session = client.login("alice", PASSWORD).await;
    (client, session)
}

fn addons() -> Features {
    Features::only(Feature::DownloadAddons)
}

/// Subtitle `subtitle` of the movie as WebVTT, read chunk by chunk, asked
/// again while it is made, for at most 30 s.
async fn webvtt(client: &TestClient, session: &SessionId, subtitle: u32) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (mut bytes, mut version) = (Vec::new(), None);
    loop {
        let read = Request::ReadTrackSubtitle {
            id: movie().id().clone(),
            subtitle,
            offset: bytes.len() as u64,
            version,
        };
        match client.call(Some(session), read).await {
            Response::Ok(Reply::SubtitleChunk {
                format,
                bytes: chunk,
                total,
                version: read,
            }) => {
                assert_eq!(format, spoolframe_types::SubtitleFormat::WebVtt);
                version = Some(read);
                bytes.extend_from_slice(&chunk);
                if bytes.len() as u64 >= total {
                    return String::from_utf8(bytes).unwrap();
                }
            }
            Response::Err(Error::SubtitleNotReady { .. }) => {
                assert!(std::time::Instant::now() < deadline, "never made");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            other => panic!("not read: {other:?}"),
        }
    }
}

/// The first cue's times line of `text`.
fn first_cue(text: &str) -> &str {
    text.lines().find(|l| l.contains("-->")).expect("a cue")
}

/// What may be added: both audio tracks, sized, and the subtitle.
#[tokio::test]
async fn the_options_name_every_track() {
    let server = server("addons-options", ServerOptions::default()).await;
    write_offset_film(&server);
    let (phone, session) = alice(&server, Platform::Ios, addons()).await;
    let options = match phone
        .call(
            Some(&session),
            Request::DownloadAddonOptions {
                id: movie().id().clone(),
            },
        )
        .await
    {
        Response::Ok(Reply::DownloadAddonOptions(options)) => options,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (
            options.tracks.audio.len(),
            options
                .audio
                .iter()
                .map(|a| a.size.is_some())
                .collect::<Vec<_>>(),
            options.subtitles,
        ),
        (
            2,
            vec![true, true],
            vec![AddonSubtitle {
                track: 0,
                left_out: None
            }]
        ),
        "{:?}",
        options.audio
    );
    assert!(
        options
            .audio
            .iter()
            .all(|AddonAudio { left_out, .. }| left_out.is_none())
    );
    server.stop().await;
}

/// On an Apple device the cues are on the MP4's timeline, which keeps the
/// film's 10 s start; on Linux they are as the film's subtitle has them.
#[tokio::test]
async fn a_subtitle_is_webvtt_on_the_download_s_timeline() {
    let server = server("addons-subtitle", ServerOptions::default()).await;
    write_offset_film(&server);
    let (phone, phone_session) = alice(&server, Platform::Ios, addons()).await;
    let (linux, linux_session) = alice(&server, Platform::Linux, addons()).await;
    let apple = webvtt(&phone, &phone_session, 0).await;
    let own = webvtt(&linux, &linux_session, 0).await;
    assert_eq!(
        (
            apple.starts_with("WEBVTT"),
            first_cue(&apple),
            first_cue(&own),
        ),
        (
            true,
            "00:00:10.500 --> 00:00:11.500",
            "00:00.500 --> 00:01.500"
        )
    );
    server.stop().await;
}

/// The French AAC is copied into an M4A, its language and the film's
/// 10 s start kept; the DTS is converted. Each is a download, told and
/// read as any is.
#[tokio::test]
async fn an_audio_track_is_an_m4a_of_its_own() {
    let server = server(
        "addons-audio",
        ServerOptions {
            detect_encoders: true,
            ..ServerOptions::default()
        },
    )
    .await;
    write_offset_film(&server);
    let (phone, session) = alice(&server, Platform::Ios, addons()).await;
    let mut phone_events = events(&phone).await;
    let prepare = |audio| Request::PrepareAudioAddon {
        id: movie().id().clone(),
        audio,
    };
    let mut files = Vec::new();
    for audio in [1, 0] {
        let info = match phone.call(Some(&session), prepare(audio)).await {
            Response::Ok(Reply::Download(info)) => info,
            other => panic!("{other:?}"),
        };
        let file = info.file.clone().expect("what the file is");
        assert_eq!(
            (file.container, file.tracks.audio),
            (Container::Mp4, vec![audio])
        );
        let moves = moves_until_settled(&mut phone_events, &info.id).await;
        assert!(
            matches!(moves.last(), Some(DownloadState::Ready { .. })),
            "{moves:?}"
        );
        let bytes = read_from(&phone, &session, &info.id, 0, 4 << 20).await;
        let path = server.dir.as_ref().join(format!("addon-{audio}.m4a"));
        std::fs::write(&path, &bytes).unwrap();
        files.push(path);
    }
    let started = |file| probe(file, "format=start_time")[0].parse::<f64>().unwrap();
    assert_eq!(
        (
            probe(&files[0], "stream=codec_type,codec_name"),
            probe(&files[0], "stream_tags=language"),
            started(&files[0]) > 9.9,
            probe(&files[1], "stream=codec_type").len(),
            probe(&files[1], "stream=codec_name") != vec!["dts".to_owned()],
            started(&files[1]) > 9.9,
        ),
        (
            vec!["aac,audio".to_owned()],
            vec!["fre".to_owned()],
            true,
            1,
            true,
            true,
        )
    );
    server.stop().await;
}

/// Without the feature, or on a platform with no downloads, each is
/// `Unsupported`.
#[tokio::test]
async fn without_the_feature_or_downloads_they_are_unsupported() {
    let server = server("addons-off", ServerOptions::default()).await;
    write_offset_film(&server);
    let (off, off_session) = alice(&server, Platform::Ios, Features::empty()).await;
    let (tv, tv_session) = alice(&server, Platform::Tvos, addons()).await;
    let ask = || Request::DownloadAddonOptions {
        id: movie().id().clone(),
    };
    assert!(matches!(
        (
            off.call(Some(&off_session), ask()).await,
            tv.call(Some(&tv_session), ask()).await,
        ),
        (
            Response::Err(Error::Unsupported { .. }),
            Response::Err(Error::Unsupported { .. })
        )
    ));
    server.stop().await;
}
