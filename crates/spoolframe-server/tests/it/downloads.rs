//! Downloads' core (PX-38, before the wire): files written with real
//! ffmpeg, ffprobe and tesseract from films made at test time, through
//! the server's own `Downloads`. Which file a request becomes is tested
//! in `downloads::plan`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spoolframe_media::MediaDescriptor;
use spoolframe_media::sandbox::Tool;
use spoolframe_server::downloads::plan::{Decision, Source, Tracks};
use spoolframe_server::downloads::{
    DeviceClass, DownloadError, DownloadState, PrepareRequest, Prepared,
};
use spoolframe_server::optimized::Deleted;
use spoolframe_server::principal::Principal;
use spoolframe_server::store::optimized::CopyTarget;
use spoolframe_types::BitmapPreference;

use crate::support::{ServerOptions, TestServer, fresh_dir, movie};

/// Who prepares the downloads.
fn alice() -> Principal {
    "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c"
        .parse()
        .unwrap()
}

/// A server with real ffmpeg and ffprobe, as `options` say otherwise.
async fn server(test: &str, options: ServerOptions) -> TestServer {
    TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await
}

/// Write the movie: 2 s of H.264 with an AAC and an AC-3 track, and
/// English, French and German SubRip subtitles, in Matroska.
pub(crate) fn write_film(server: &TestServer) {
    let dir = server.media.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"]
        .map(String::from)
        .to_vec();
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
        ]
        .map(String::from),
    );
    for (i, word) in ["Hello", "Bonjour", "Hallo"].iter().enumerate() {
        let srt = dir.join(format!("sub-{i}.srt"));
        std::fs::write(&srt, format!("1\n00:00:00,500 --> 00:00:01,500\n{word}\n")).unwrap();
        args.extend(["-i".into(), srt.display().to_string()]);
    }
    for map in ["0:v", "1:a", "2:a", "3:s", "4:s", "5:s"] {
        args.extend(["-map".into(), map.into()]);
    }
    args.extend(
        [
            "-t",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-c:a:0",
            "aac",
            "-c:a:1",
            "ac3",
            "-ac",
            "2",
            "-c:s",
            "srt",
            "-metadata:s:s:0",
            "language=eng",
            "-metadata:s:s:1",
            "language=fre",
            "-metadata:s:s:2",
            "language=ger",
        ]
        .map(String::from),
    );
    args.push(server.media.display().to_string());
    let status = std::process::Command::new("ffmpeg")
        .args(&args)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

async fn describe(media: &Path) -> MediaDescriptor {
    let prober = spoolframe_media::Prober::new(
        Tool::unconfined("ffprobe"),
        spoolframe_media::Prober::DEFAULT_TIMEOUT,
    );
    (*prober.describe(media).await.unwrap()).clone()
}

/// `tracks` of the movie for a `class` device, from `source`, the viewer
/// wanting picture subtitles as `bitmap`.
async fn prepare(
    server: &TestServer,
    class: DeviceClass,
    source: Source,
    tracks: Tracks,
    bitmap: BitmapPreference,
) -> Prepared {
    let descriptor = describe(&server.media).await;
    let media_id = movie().id().to_string();
    server
        .downloads
        .prepare(PrepareRequest {
            principal: alice(),
            media_id: &media_id,
            media: &server.media,
            descriptor: &descriptor,
            class,
            capabilities: None,
            source,
            tracks,
            bitmap,
        })
        .await
        .unwrap()
}

/// The first audio track and the first two subtitles.
fn one_audio_two_subtitles() -> Tracks {
    Tracks::Chosen {
        audio: vec![0],
        subtitles: vec![0, 1],
    }
}

/// The first audio track alone.
fn first_audio() -> Tracks {
    Tracks::Chosen {
        audio: vec![0],
        subtitles: Vec::new(),
    }
}

/// Download `id`'s state once it has one of `done`'s, for at most 60 s.
async fn until(
    server: &TestServer,
    id: &str,
    done: impl Fn(&DownloadState) -> bool,
) -> DownloadState {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state = server
            .downloads
            .state(&alice(), id)
            .await
            .unwrap()
            .expect("the download is there");
        if done(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "the download stays {state:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Download `id`'s file once ready: where it is, and its SHA-256.
async fn ready(server: &TestServer, id: &str) -> (PathBuf, String) {
    let state = until(server, id, |s| {
        matches!(
            s,
            DownloadState::Ready { .. } | DownloadState::Failed { .. }
        )
    })
    .await;
    let DownloadState::Ready { sha256, .. } = state else {
        panic!("the download failed: {state:?}");
    };
    let read = server.downloads.open_read(&alice(), id).await.unwrap();
    (read.path.clone(), sha256)
}

/// What ffprobe says of `entries` in `file`, a line each.
fn probe(file: &Path, entries: &str) -> Vec<String> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("ffprobe runs");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim_end_matches(',').trim_matches('"').to_owned())
        .collect()
}

#[tokio::test]
async fn an_apple_download_of_one_audio_and_two_subtitles_is_an_mp4_with_exactly_those() {
    let server = server("downloads-apple-tracks", ServerOptions::default()).await;
    write_film(&server);
    let prepared = prepare(
        &server,
        DeviceClass::Apple,
        Source::Original,
        one_audio_two_subtitles(),
        BitmapPreference::Text,
    )
    .await;
    let (file, _) = ready(&server, &prepared.id).await;
    assert_eq!(
        (
            probe(&file, "format=format_name"),
            probe(&file, "stream=codec_name"),
            probe(&file, "stream_tags=language")
        ),
        (
            vec!["mov,mp4,m4a,3gp,3g2,mj2".to_owned()],
            ["h264", "aac", "mov_text", "mov_text"]
                .map(String::from)
                .to_vec(),
            ["und", "und", "eng", "fre"].map(String::from).to_vec()
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn a_linux_download_is_matroska() {
    let server = server("downloads-linux", ServerOptions::default()).await;
    write_film(&server);
    let prepared = prepare(
        &server,
        DeviceClass::Linux,
        Source::Original,
        one_audio_two_subtitles(),
        BitmapPreference::Text,
    )
    .await;
    let (file, _) = ready(&server, &prepared.id).await;
    assert_eq!(
        probe(&file, "stream=codec_name"),
        ["h264", "aac", "subrip", "subrip"]
    );
    server.stop().await;
}

/// Matroska with every track, for Linux: the film itself, byte for byte.
#[tokio::test]
async fn a_file_sent_as_it_is_is_the_film_itself() {
    let server = server("downloads-as-is", ServerOptions::default()).await;
    write_film(&server);
    let prepared = prepare(
        &server,
        DeviceClass::Linux,
        Source::Original,
        Tracks::All,
        BitmapPreference::Text,
    )
    .await;
    let (file, sha) = ready(&server, &prepared.id).await;
    let film = std::fs::read(&server.media).unwrap();
    let film_sha: String = <sha2::Sha256 as sha2::Digest>::digest(&film)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        (prepared.decision, file, sha),
        (Decision::AsIs, server.media.clone(), film_sha)
    );
    server.stop().await;
}

/// Prepared as it is for Linux with every track, and ready.
async fn ready_as_is(server: &TestServer) -> Prepared {
    let prepared = prepare(
        server,
        DeviceClass::Linux,
        Source::Original,
        Tracks::All,
        BitmapPreference::Text,
    )
    .await;
    ready(server, &prepared.id).await;
    assert_eq!(prepared.decision, Decision::AsIs);
    prepared
}

/// The media file rewritten with a byte more: another size and SHA-256.
fn change_media(server: &TestServer) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&server.media)
        .unwrap();
    file.write_all(&[0]).unwrap();
}

/// Review 2026-10-20, finding 4: a file sent as it is is the media file
/// itself; once that changed, it is not served under the size and
/// SHA-256 it had, and the download fails, saying why.
#[tokio::test]
async fn a_file_sent_as_it_is_whose_media_changed_is_not_read() {
    let server = server("downloads-as-is-changed", ServerOptions::default()).await;
    write_film(&server);
    let prepared = ready_as_is(&server).await;
    change_media(&server);
    let read = server.downloads.open_read(&alice(), &prepared.id).await;
    let state = server
        .downloads
        .state(&alice(), &prepared.id)
        .await
        .unwrap();
    assert!(
        matches!(read, Err(DownloadError::NotReady)),
        "{:?}",
        read.map(|r| r.size)
    );
    assert!(
        matches!(&state, Some(DownloadState::Failed { reason }) if reason.contains("changed")),
        "{state:?}"
    );
    server.stop().await;
}

/// The same, found at start.
#[tokio::test]
async fn a_file_sent_as_it_is_whose_media_changed_fails_at_start() {
    let server = server("downloads-as-is-restart", ServerOptions::default()).await;
    write_film(&server);
    let prepared = ready_as_is(&server).await;
    change_media(&server);
    let server = server.restart().await;
    let state = server
        .downloads
        .state(&alice(), &prepared.id)
        .await
        .unwrap();
    assert!(
        matches!(&state, Some(DownloadState::Failed { reason }) if reason.contains("changed")),
        "{state:?}"
    );
    server.stop().await;
}

/// Review 2026-10-20, finding 1: the sweep removes only the files the
/// area writes; another file in its directory stays.
#[tokio::test]
async fn a_file_the_area_did_not_write_is_kept() {
    let server = server("downloads-foreign-file", ServerOptions::default()).await;
    let stray = server.downloads.dir().join("Alien (1979).mkv");
    std::fs::write(&stray, b"not the area's").unwrap();
    server.downloads.sweep().await.unwrap();
    assert!(stray.exists(), "the sweep removed a file it did not write");
    server.stop().await;
}

/// The file written again, after its download was cancelled and asked
/// for anew, is the same bytes: a device's SHA-256 check holds.
#[tokio::test]
async fn a_file_written_twice_has_the_same_sha256() {
    let server = server("downloads-twice", ServerOptions::default()).await;
    write_film(&server);
    let ask = || {
        prepare(
            &server,
            DeviceClass::Apple,
            Source::Original,
            one_audio_two_subtitles(),
            BitmapPreference::Text,
        )
    };
    let first = ask().await;
    let (file, sha) = ready(&server, &first.id).await;
    assert!(server.downloads.cancel(&alice(), &first.id).await.unwrap());
    assert!(!file.exists(), "the file stayed after its download went");
    let again = ask().await;
    let (_, sha_again) = ready(&server, &again.id).await;
    assert_eq!(sha_again, sha);
    server.stop().await;
}

/// A remade stored video track (a GPU encode may not be the same bytes)
/// is another file: the device starts over rather than resuming.
#[tokio::test]
async fn a_remade_stored_video_track_gives_a_new_content_key() {
    let server = server("downloads-remade", ServerOptions::default()).await;
    write_film(&server);
    let ask = || {
        prepare(
            &server,
            DeviceClass::Apple,
            Source::Profile("Mobile".into()),
            first_audio(),
            BitmapPreference::Text,
        )
    };
    let first = ask().await;
    assert!(
        matches!(first.decision, Decision::Convert { .. }),
        "{first:?}"
    );
    ready(&server, &first.id).await;
    let copy = server
        .store
        .optimized_copies()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.target == CopyTarget::Profile("Mobile".into()))
        .unwrap();
    assert_eq!(server.pool().delete(&copy.id).await.unwrap(), Deleted::Now);
    let again = ask().await;
    assert_ne!(again.content_key, first.content_key);
    server.stop().await;
}

/// The movie with an English PGS track (subtitle 0), for a server that
/// reads pictures as text.
async fn with_pgs(test: &str) -> TestServer {
    let server = server(
        test,
        ServerOptions {
            ocr: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(4, "libx264", "aac");
    server.add_pgs(&[("Hello there", 500, 2_500)]);
    server
}

#[tokio::test]
async fn picture_subtitles_are_read_as_text_into_an_apple_download_with_text_chosen() {
    let server = with_pgs("downloads-ocr-text").await;
    let prepared = prepare(
        &server,
        DeviceClass::Apple,
        Source::Original,
        Tracks::All,
        BitmapPreference::Text,
    )
    .await;
    let (file, _) = ready(&server, &prepared.id).await;
    assert_eq!(
        (prepared.left_out, probe(&file, "stream=codec_name")),
        (
            Vec::new(),
            ["h264", "aac", "mov_text"].map(String::from).to_vec()
        )
    );
    server.stop().await;
}

#[tokio::test]
async fn picture_subtitles_are_left_out_of_an_apple_download_with_pictures_chosen() {
    let server = with_pgs("downloads-ocr-pictures").await;
    let prepared = prepare(
        &server,
        DeviceClass::Apple,
        Source::Original,
        Tracks::All,
        BitmapPreference::Pictures,
    )
    .await;
    let (file, _) = ready(&server, &prepared.id).await;
    assert_eq!(
        (
            prepared
                .left_out
                .iter()
                .map(|(at, _)| *at)
                .collect::<Vec<_>>(),
            probe(&file, "stream=codec_name")
        ),
        (vec![0], ["h264", "aac"].map(String::from).to_vec())
    );
    server.stop().await;
}

/// The movie's first audio track for a Linux device.
async fn linux_first_audio(server: &TestServer) -> Prepared {
    prepare(
        server,
        DeviceClass::Linux,
        Source::Original,
        first_audio(),
        BitmapPreference::Text,
    )
    .await
}

/// A restart keeps the id, and a ready file whose file is there is not
/// written again.
#[tokio::test]
async fn a_download_s_id_and_ready_file_survive_a_restart() {
    let server = server("downloads-restart", ServerOptions::default()).await;
    write_film(&server);
    let first = linux_first_audio(&server).await;
    let (file, _) = ready(&server, &first.id).await;
    let written = std::fs::metadata(&file).unwrap().modified().unwrap();
    let server = server.restart().await;
    let again = linux_first_audio(&server).await;
    assert_eq!(
        (
            again.id,
            matches!(again.state, DownloadState::Ready { .. }),
            std::fs::metadata(&file).unwrap().modified().unwrap()
        ),
        (first.id, true, written)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_file_is_removed_a_day_after_its_last_read() {
    let server = server("downloads-expiry", ServerOptions::default()).await;
    write_film(&server);
    let prepared = prepare(
        &server,
        DeviceClass::Linux,
        Source::Original,
        first_audio(),
        BitmapPreference::Text,
    )
    .await;
    let (file, _) = ready(&server, &prepared.id).await;
    server.clock.advance(Duration::from_secs(23 * 60 * 60));
    server.downloads.sweep().await.unwrap();
    let kept = file.exists();
    server.clock.advance(Duration::from_secs(2 * 60 * 60));
    server.downloads.sweep().await.unwrap();
    let state = server
        .downloads
        .state(&alice(), &prepared.id)
        .await
        .unwrap();
    assert_eq!(
        (
            kept,
            file.exists(),
            matches!(state, Some(DownloadState::Failed { .. }))
        ),
        (true, false, true)
    );
    server.stop().await;
}

/// With room for one file, a second waits until the first goes.
#[tokio::test]
async fn over_the_area_s_limit_a_new_file_waits() {
    let dir = fresh_dir("downloads-budget").await;
    let first_server = TestServer::start_with(
        dir.clone(),
        ServerOptions {
            real_media: true,
            ..ServerOptions::default()
        },
    )
    .await;
    write_film(&first_server);
    let film = std::fs::metadata(&first_server.media).unwrap().len();
    first_server.stop().await;
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            downloads_max_size: film + film / 2,
            ..ServerOptions::default()
        },
    )
    .await;
    let first = prepare(
        &server,
        DeviceClass::Linux,
        Source::Original,
        first_audio(),
        BitmapPreference::Text,
    )
    .await;
    ready(&server, &first.id).await;
    let second = prepare(
        &server,
        DeviceClass::Linux,
        Source::Original,
        one_audio_two_subtitles(),
        BitmapPreference::Text,
    )
    .await;
    // Written, a file this small takes well under this.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let waited = server.downloads.state(&alice(), &second.id).await.unwrap();
    assert!(server.downloads.cancel(&alice(), &first.id).await.unwrap());
    let then = until(&server, &second.id, |s| {
        matches!(
            s,
            DownloadState::Ready { .. } | DownloadState::Failed { .. }
        )
    })
    .await;
    assert_eq!(
        (waited, matches!(then, DownloadState::Ready { .. })),
        (Some(DownloadState::Waiting), true)
    );
    server.stop().await;
}
