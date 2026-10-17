//! The bridge over a real connection, reading from an in-memory source.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HOST, RANGE, TRANSFER_ENCODING,
};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use spoolframe_hls_bridge::{
    Bridge, Chunk, Error, Kind, ManifestKind, MenuEntry, SEGMENT_MAX, Segment, Source, Stream,
    Subtitle,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// One answer for a segment: its pieces, then the answers a reopen gets.
#[derive(Clone, Default)]
struct Answer {
    pieces: Vec<Vec<u8>>,
    size: u64,
    complete: bool,
    epoch: u32,
    /// What each reopen after this answer gets, in order.
    rest: Vec<Answer>,
    /// The playback becomes another after this many pieces.
    change_after: Option<usize>,
}

impl Answer {
    /// A whole segment of `bytes`, in `pieces` pieces.
    fn whole(bytes: &[u8], pieces: usize) -> Self {
        let step = bytes.len().div_ceil(pieces).max(1);
        Self {
            pieces: bytes.chunks(step).map(<[u8]>::to_vec).collect(),
            size: bytes.len() as u64,
            complete: true,
            ..Self::default()
        }
    }
}

/// A segment's place: (variant, n).
type At = (u32, u32);

#[derive(Default)]
struct Fake {
    file: Vec<u8>,
    playlists: Mutex<HashMap<(ManifestKind, u32), String>>,
    /// What each open of a segment gets, in order; the last one again
    /// once the others are used.
    opens: Mutex<HashMap<At, VecDeque<Result<Answer, Error>>>>,
    /// The segments opened, as (variant, n).
    opened: Mutex<Vec<At>>,
    /// Segments read whole (the init segments): bytes and the size said.
    whole: Mutex<HashMap<At, (Vec<u8>, u64)>>,
    subtitles: Mutex<HashMap<u32, Result<Vec<u8>, Error>>>,
    changes: Option<watch::Sender<u32>>,
    replanned: AtomicBool,
}

#[derive(Clone)]
struct Src(Arc<Fake>);

impl Src {
    fn new(file: &[u8]) -> Self {
        Self(Arc::new(Fake {
            file: file.to_vec(),
            changes: Some(watch::channel(0).0),
            ..Fake::default()
        }))
    }

    fn playlist(&self, kind: ManifestKind, variant: u32, text: &str) {
        self.0.playlists.lock().insert((kind, variant), text.into());
    }

    fn opens(&self, variant: u32, n: u32, answers: Vec<Result<Answer, Error>>) {
        self.0.opens.lock().insert((variant, n), answers.into());
    }

    fn change(&self) {
        if let Some(changes) = &self.0.changes {
            changes.send_modify(|n| *n += 1);
        }
    }
}

struct FakeSegment {
    source: Src,
    pieces: VecDeque<Vec<u8>>,
    given: usize,
    sent: u64,
    size: u64,
    complete: bool,
    epoch: u32,
    rest: VecDeque<Answer>,
    change_after: Option<usize>,
}

impl FakeSegment {
    fn new(source: Src, answer: Answer, rest: VecDeque<Answer>) -> Self {
        Self {
            source,
            sent: answer.pieces.iter().map(|p| p.len() as u64).sum(),
            pieces: answer.pieces.into(),
            given: 0,
            size: answer.size,
            complete: answer.complete,
            epoch: answer.epoch,
            rest,
            change_after: answer.change_after,
        }
    }
}

impl Segment for FakeSegment {
    fn sent(&self) -> u64 {
        self.sent
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn complete(&self) -> bool {
        self.complete
    }
    fn epoch(&self) -> u32 {
        self.epoch
    }
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.change_after == Some(self.given) {
            self.source.change();
        }
        self.given += 1;
        Ok(self.pieces.pop_front())
    }
    async fn reopen(&self, _: u64) -> Result<Self, Error> {
        let mut rest = self.rest.clone();
        let Some(answer) = rest.pop_front() else {
            return Err(Error::Other("no more answers".into()));
        };
        let mut more: VecDeque<Answer> = answer.rest.clone().into();
        more.extend(rest);
        Ok(Self::new(self.source.clone(), answer, more))
    }
}

impl Source for Src {
    type Snapshot = u32;
    type Segment = FakeSegment;

    fn changes(&self) -> watch::Receiver<u32> {
        self.0.changes.as_ref().expect("made with one").subscribe()
    }

    fn replanned(&self) -> bool {
        self.0.replanned.load(Ordering::Relaxed)
    }

    async fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, Error> {
        let start = (offset as usize).min(self.0.file.len());
        let end = (start + length as usize).min(self.0.file.len());
        Ok(self.0.file[start..end].to_vec())
    }

    async fn manifest(&self, kind: ManifestKind, variant: u32) -> Result<String, Error> {
        self.0
            .playlists
            .lock()
            .get(&(kind, variant))
            .cloned()
            .ok_or_else(|| Error::Other("no such playlist".into()))
    }

    async fn read_segment(
        &self,
        variant: u32,
        n: u32,
        offset: u64,
        length: u64,
    ) -> Result<Chunk, Error> {
        let whole = self.0.whole.lock();
        let Some((bytes, size)) = whole.get(&(variant, n)) else {
            return Err(Error::Other("no such segment".into()));
        };
        let start = (offset as usize).min(bytes.len());
        let end = (start + length as usize).min(bytes.len());
        Ok(Chunk {
            bytes: bytes[start..end].to_vec(),
            size: *size,
            complete: true,
            epoch: 0,
        })
    }

    async fn segment_stream(
        &self,
        variant: u32,
        n: u32,
        _: u64,
        _: u64,
    ) -> Result<FakeSegment, Error> {
        self.0.opened.lock().push((variant, n));
        let answer = {
            let mut opens = self.0.opens.lock();
            let Some(queue) = opens.get_mut(&(variant, n)) else {
                return Err(Error::Other("no such segment".into()));
            };
            if queue.len() > 1 {
                queue.pop_front().expect("not empty")
            } else {
                queue
                    .front()
                    .cloned()
                    .ok_or_else(|| Error::Other("no answers".into()))?
            }
        }?;
        let rest = answer.rest.clone().into();
        Ok(FakeSegment::new(self.clone(), answer, rest))
    }

    async fn read_subtitle_segment(&self, n: u32) -> Result<Subtitle, Error> {
        let bytes = self
            .0
            .subtitles
            .lock()
            .get(&n)
            .cloned()
            .unwrap_or_else(|| Err(Error::Other("no such subtitle".into())))?;
        Ok(Subtitle { bytes })
    }
}

/// A bridge on 127.0.0.1, and its address.
async fn bridge() -> (Bridge<Src>, SocketAddr) {
    let bridge = Bridge::new();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binds loopback");
    let addr = listener.local_addr().expect("bound");
    let serving = bridge.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    (bridge, addr)
}

fn hls(bridge: &Bridge<Src>, source: &Src) -> String {
    bridge.register(Stream::new(
        source.clone(),
        Kind::hls(),
        None,
        Default::default(),
    ))
}

struct Fetched {
    status: StatusCode,
    headers: HeaderMap,
    /// The body, or why it ended short.
    body: Result<Bytes, String>,
}

impl Fetched {
    fn text(&self) -> String {
        String::from_utf8_lossy(self.body.as_ref().expect("a whole body")).into_owned()
    }
    fn header(&self, name: hyper::header::HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn fetch(addr: SocketAddr, method: Method, path: &str, range: Option<&str>) -> Fetched {
    let socket = TcpStream::connect(addr).await.expect("connects");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .expect("handshake");
    tokio::spawn(connection);
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, addr.to_string());
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    let response = sender
        .send_request(request.body(Empty::<Bytes>::new()).expect("a request"))
        .await
        .expect("an answer");
    let (head, body) = response.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .map_err(|e| e.to_string());
    Fetched {
        status: head.status,
        headers: head.headers,
        body,
    }
}

/// Whether a GET of `path` fails part-way: no answer, or a body cut short.
async fn ends_short(addr: SocketAddr, path: &str) -> bool {
    let socket = TcpStream::connect(addr).await.expect("connects");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .expect("handshake");
    tokio::spawn(connection);
    let request = Request::get(path)
        .header(HOST, addr.to_string())
        .body(Empty::<Bytes>::new())
        .expect("a request");
    match sender.send_request(request).await {
        Err(_) => true,
        Ok(response) => response.into_body().collect().await.is_err(),
    }
}

async fn get(addr: SocketAddr, path: &str) -> Fetched {
    fetch(addr, Method::GET, path, None).await
}

#[tokio::test]
async fn an_unknown_token_is_not_found() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let token = hls(&bridge, &source);
    assert_eq!(
        get(addr, &format!("/{token}/media.m3u8")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get(addr, "/0123456789abcdef/media.m3u8").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(addr, "/").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unregistered_token_is_not_found() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let token = hls(&bridge, &source);
    assert!(bridge.unregister(&token).is_some());
    assert_eq!(
        get(addr, &format!("/{token}/media.m3u8")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_get_and_head_are_answered() {
    let (bridge, addr) = bridge().await;
    let token = hls(&bridge, &Src::new(b""));
    let answer = fetch(addr, Method::POST, &format!("/{token}/media.m3u8"), None).await;
    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
}

fn direct(bridge: &Bridge<Src>, file: &[u8]) -> String {
    bridge.register(Stream::new(
        Src::new(file),
        Kind::direct(file.len() as u64, "video/mp4"),
        None,
        Default::default(),
    ))
}

#[tokio::test]
async fn a_file_is_served_whole_and_in_ranges() {
    let (bridge, addr) = bridge().await;
    let token = direct(&bridge, b"0123456789");
    let path = format!("/{token}/file");

    let whole = get(addr, &path).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.text(), "0123456789");
    assert_eq!(whole.header(ACCEPT_RANGES), Some("bytes"));
    assert_eq!(whole.header(CONTENT_TYPE), Some("video/mp4"));

    let part = fetch(addr, Method::GET, &path, Some("bytes=2-5")).await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.header(CONTENT_RANGE), Some("bytes 2-5/10"));
    assert_eq!(part.text(), "2345");

    let tail = fetch(addr, Method::GET, &path, Some("bytes=-3")).await;
    assert_eq!(tail.text(), "789");

    let past = fetch(addr, Method::GET, &path, Some("bytes=10-")).await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.header(CONTENT_RANGE), Some("bytes */10"));

    let malformed = fetch(addr, Method::GET, &path, Some("bytes=0-1,3-4")).await;
    assert_eq!(malformed.status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn head_says_the_length_and_sends_no_body() {
    let (bridge, addr) = bridge().await;
    let token = direct(&bridge, b"0123456789");
    let head = fetch(
        addr,
        Method::HEAD,
        &format!("/{token}/file"),
        Some("bytes=4-"),
    )
    .await;
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.header(CONTENT_LENGTH), Some("6"));
    assert_eq!(head.body.as_deref().map(<[u8]>::len), Ok(0));

    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\nseg1.m4s\n");
    let token = hls(&bridge, &source);
    let head = fetch(addr, Method::HEAD, &format!("/{token}/media.m3u8"), None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header(CONTENT_LENGTH), Some("17"));
    assert_eq!(head.body.as_deref().map(<[u8]>::len), Ok(0));
}

#[tokio::test]
async fn a_playlist_of_its_own_resources_is_served_as_is() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    let media = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.0,\nseg1.m4s\n#EXT-X-ENDLIST\n";
    source.playlist(ManifestKind::Media, 0, media);
    source.playlist(
        ManifestKind::Media,
        2,
        "#EXTM3U\n#EXT-X-MAP:URI=\"v2/init.mp4\"\nv2/seg1.m4s\n",
    );
    let token = hls(&bridge, &source);
    let answer = get(addr, &format!("/{token}/media.m3u8")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.header(CONTENT_TYPE),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(answer.text(), media);
    let rung = get(addr, &format!("/{token}/v2/media.m3u8")).await;
    assert_eq!(rung.status, StatusCode::OK);
    assert!(rung.text().contains("v2/seg1.m4s"));
}

#[tokio::test]
async fn a_playlist_naming_another_host_is_refused() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(
        ManifestKind::Media,
        0,
        "#EXTM3U\n#EXTINF:6.0,\nhttps://evil.example/seg1.m4s\n",
    );
    source.playlist(
        ManifestKind::Multivariant,
        0,
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"x\",URI=\"http://evil.example/s.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n",
    );
    source.playlist(
        ManifestKind::Media,
        1,
        "#EXTM3U\n#EXT-X-MAP:URI=\"//evil.example/init.mp4\"\n",
    );
    let token = hls(&bridge, &source);
    for name in ["media.m3u8", "master.m3u8", "v1/media.m3u8"] {
        let answer = get(addr, &format!("/{token}/{name}")).await;
        assert_eq!(answer.status, StatusCode::BAD_GATEWAY, "{name}");
        assert!(!answer.text().contains("evil"), "{name}");
    }
}

#[tokio::test]
async fn an_oversized_segment_is_refused() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(
        0,
        3,
        vec![Ok(Answer {
            pieces: vec![vec![0; 16]],
            size: SEGMENT_MAX + 1,
            complete: true,
            ..Answer::default()
        })],
    );
    source
        .0
        .whole
        .lock()
        .insert((0, 0), (vec![0; 16], SEGMENT_MAX + 1));
    let token = hls(&bridge, &source);
    let segment = get(addr, &format!("/{token}/seg3.m4s")).await;
    assert_eq!(segment.status, StatusCode::BAD_GATEWAY);
    let init = get(addr, &format!("/{token}/init.mp4")).await;
    assert_eq!(init.status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_whole_segment_promises_its_length() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(0, 1, vec![Ok(Answer::whole(b"segment-one", 3))]);
    let token = hls(&bridge, &source);
    let answer = get(addr, &format!("/{token}/seg1.m4s")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header(CONTENT_LENGTH), Some("11"));
    assert_eq!(answer.header(CONTENT_TYPE), Some("video/iso.segment"));
    assert_eq!(answer.text(), "segment-one");
}

#[tokio::test]
async fn a_segment_still_encoding_is_sent_chunked() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    let first = Answer {
        pieces: vec![b"ab".to_vec(), b"cd".to_vec()],
        size: 4,
        complete: false,
        rest: vec![
            // Nothing more written yet: the source waited.
            Answer::default(),
            Answer {
                pieces: vec![b"ef".to_vec()],
                size: 6,
                complete: true,
                ..Answer::default()
            },
        ],
        ..Answer::default()
    };
    source.opens(0, 4, vec![Ok(first)]);
    let token = hls(&bridge, &source);
    let answer = get(addr, &format!("/{token}/seg4.m4s")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header(CONTENT_LENGTH), None);
    assert_eq!(answer.header(TRANSFER_ENCODING), Some("chunked"));
    assert_eq!(answer.text(), "abcdef");
}

#[tokio::test]
async fn a_segment_whose_encode_restarts_while_opening_is_opened_again() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(
        0,
        2,
        vec![
            Err(Error::Restarted { epoch: 1 }),
            Err(Error::Resumed { at: Duration::ZERO }),
            Ok(Answer::whole(b"after", 1)),
        ],
    );
    let token = hls(&bridge, &source);
    let answer = get(addr, &format!("/{token}/seg2.m4s")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.text(), "after");
    assert_eq!(source.0.opened.lock().len(), 3);
}

#[tokio::test]
async fn a_segment_that_keeps_restarting_is_given_up() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(0, 2, vec![Err(Error::Restarted { epoch: 1 })]);
    let token = hls(&bridge, &source);
    let answer = get(addr, &format!("/{token}/seg2.m4s")).await;
    assert_eq!(answer.status, StatusCode::BAD_GATEWAY);
    // The first open and eight restarts.
    assert_eq!(source.0.opened.lock().len(), 9);
}

#[tokio::test]
async fn a_segment_whose_playback_changes_part_way_ends_short() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    let mut answer = Answer::whole(b"abcdef", 3);
    answer.change_after = Some(1);
    source.opens(0, 5, vec![Ok(answer)]);
    let token = hls(&bridge, &source);
    // Cut before or after the headers went out, as hyper flushes them:
    // either way the player never gets a whole segment.
    assert!(ends_short(addr, &format!("/{token}/seg5.m4s")).await);
    assert!(
        bridge
            .notes(&token)
            .iter()
            .any(|n| n.contains("identity changed")),
        "the playback log says why"
    );
}

#[tokio::test]
async fn a_ladder_rung_reads_its_own_variant() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(2, 7, vec![Ok(Answer::whole(b"rung-two", 1))]);
    source
        .0
        .whole
        .lock()
        .insert((2, 0), (b"init-two".to_vec(), 8));
    let token = hls(&bridge, &source);
    assert_eq!(
        get(addr, &format!("/{token}/v2/seg7.m4s")).await.text(),
        "rung-two"
    );
    let init = get(addr, &format!("/{token}/v2/init.mp4")).await;
    assert_eq!(init.header(CONTENT_TYPE), Some("video/mp4"));
    assert_eq!(init.text(), "init-two");
    assert_eq!(*source.0.opened.lock(), vec![(2, 7)]);
    assert_eq!(
        get(addr, &format!("/{token}/v2/master.m3u8")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(addr, &format!("/{token}/vx/seg7.m4s")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_master_playlist_names_the_subtitle_menu() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(
        ManifestKind::Multivariant,
        0,
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",URI=\"subtitles.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"subs\"\nmedia.m3u8\n",
    );
    source.playlist(
        ManifestKind::Media,
        0,
        "#EXTM3U\n#EXTINF:4.0,\nseg1.m4s\n#EXTINF:2.5,\nseg2.m4s\n",
    );
    let token = bridge.register(Stream::new(
        source.clone(),
        Kind::hls(),
        Some(0),
        Default::default(),
    ));
    let menu = vec![
        MenuEntry {
            index: 0,
            name: "English".into(),
            language: Some("en".into()),
            forced: false,
        },
        MenuEntry {
            index: 1,
            name: "French \"CC\"".into(),
            language: Some("fr".into()),
            forced: true,
        },
    ];
    assert!(bridge.set_menu(&token, menu));
    assert!(!bridge.set_menu("nope", vec![]));
    let master = get(addr, &format!("/{token}/master.m3u8")).await.text();
    assert!(master.contains(
        "NAME=\"English\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles.m3u8\",LANGUAGE=\"en\""
    ), "{master}");
    assert!(master.contains(
        "NAME=\"French 'CC'\",DEFAULT=NO,AUTOSELECT=NO,FORCED=YES,URI=\"subtitles-1.m3u8\",LANGUAGE=\"fr\""
    ), "{master}");
    // An option per entry, in the menu's order, and no closed captions
    // to make one more.
    let options: Vec<&str> = master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-MEDIA:TYPE=SUBTITLES"))
        .collect();
    assert_eq!(options.len(), 2, "{master}");
    assert!(options[0].contains("NAME=\"English\""), "{master}");
    assert!(
        master.contains("SUBTITLES=\"subs\",CLOSED-CAPTIONS=NONE\nmedia.m3u8"),
        "{master}"
    );
    // The track not playing: no cues, as long as the media.
    let other = get(addr, &format!("/{token}/subtitles-1.m3u8"))
        .await
        .text();
    assert!(other.contains("#EXTINF:6.500,\nempty.vtt"), "{other}");
    let empty = get(addr, &format!("/{token}/empty.vtt")).await;
    assert_eq!(empty.header(CONTENT_TYPE), Some("text/vtt"));
    assert!(empty.text().starts_with("WEBVTT"));
}

/// Thirteen tracks, the twelfth (a downloaded file) playing: thirteen
/// options in the menu's order, the twelfth the server's rendition and the
/// default, over a ladder of two variants; and no closed captions, which
/// AVPlayer would list as a fourteenth.
#[tokio::test]
async fn a_menu_of_n_tracks_is_n_options_with_the_playing_one_in_its_place() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(
        ManifestKind::Multivariant,
        0,
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"OpenSubtitles\",\
         DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles.m3u8\",LANGUAGE=\"en\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=2,SUBTITLES=\"subs\"\nv0/media.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,SUBTITLES=\"subs\"\nv1/media.m3u8\n",
    );
    let token = bridge.register(Stream::new(
        source.clone(),
        Kind::hls(),
        Some(11),
        Default::default(),
    ));
    let menu = (0..13)
        .map(|index| MenuEntry {
            index,
            name: format!("Track {index}"),
            language: Some("en".into()),
            forced: false,
        })
        .collect();
    assert!(bridge.set_menu(&token, menu));
    let master = get(addr, &format!("/{token}/master.m3u8")).await.text();
    let options: Vec<&str> = master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-MEDIA:TYPE=SUBTITLES"))
        .collect();
    assert_eq!(options.len(), 13, "{master}");
    for (index, option) in options.iter().enumerate() {
        assert!(
            option.contains(&format!("NAME=\"Track {index}\",")),
            "{option}"
        );
        let playing = index == 11;
        assert_eq!(option.contains("DEFAULT=YES"), playing, "{option}");
        assert_eq!(
            option.contains("URI=\"subtitles.m3u8\""),
            playing,
            "{option}"
        );
    }
    let variants: Vec<&str> = master
        .lines()
        .filter(|l| l.starts_with("#EXT-X-STREAM-INF:"))
        .collect();
    assert_eq!(variants.len(), 2, "{master}");
    for variant in variants {
        assert!(variant.ends_with(",CLOSED-CAPTIONS=NONE"), "{variant}");
    }
}

#[tokio::test]
async fn subtitle_segments_are_served_or_asked_for_again() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.0.subtitles.lock().extend([
        (1, Ok(b"WEBVTT\n\ncue".to_vec())),
        (2, Err(Error::NotReady { done_percent: 0 })),
    ]);
    let token = hls(&bridge, &source);
    let one = get(addr, &format!("/{token}/subs1.vtt")).await;
    assert_eq!(one.header(CONTENT_TYPE), Some("text/vtt"));
    assert_eq!(one.text(), "WEBVTT\n\ncue");
    assert_eq!(
        get(addr, &format!("/{token}/subs2.vtt")).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn a_stopped_or_ended_playback_is_gone_and_says_why() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(0, 1, vec![Err(Error::Stopped)]);
    source.opens(0, 2, vec![Err(Error::Expired)]);
    let token = hls(&bridge, &source);
    assert!(!bridge.stopped(&token) && !bridge.expired(&token));
    assert_eq!(
        get(addr, &format!("/{token}/seg1.m4s")).await.status,
        StatusCode::GONE
    );
    assert!(bridge.stopped(&token));
    assert!(!bridge.expired(&token));
    assert_eq!(
        get(addr, &format!("/{token}/seg2.m4s")).await.status,
        StatusCode::GONE
    );
    assert!(bridge.expired(&token));
}

#[tokio::test]
async fn the_playback_log_and_replan_are_the_tokens_own() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.opens(0, 1, vec![Ok(Answer::whole(b"x", 1))]);
    let token = hls(&bridge, &source);
    get(addr, &format!("/{token}/seg1.m4s")).await;
    let notes = bridge.notes(&token);
    assert!(notes.iter().any(|n| n.contains("v0/seg1")), "{notes:?}");
    assert!(bridge.notes(&token).is_empty(), "taken once");
    assert!(!bridge.replanned(&token));
    source.0.replanned.store(true, Ordering::Relaxed);
    assert!(bridge.replanned(&token));
    assert!(!bridge.replanned("nope"));
}

/// Planned again (Fulcrum, 2026-10-17: a ladder became one 480p encode
/// while AVPlayer was still on the ladder's playlist): the old token
/// answers 410 for every HLS resource, never the new plan's playlists or
/// segments, and the new plan's token plays.
#[tokio::test]
async fn a_replanned_stream_s_old_url_is_gone_and_the_new_one_plays() {
    let (bridge, addr) = bridge().await;
    let old = Src::new(b"");
    old.playlist(
        ManifestKind::Multivariant,
        0,
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv0/media.m3u8\n",
    );
    old.opens(
        0,
        1,
        vec![
            Ok(Answer::whole(b"ladder", 1)),
            Ok(Answer::whole(b"one encode", 1)),
        ],
    );
    let token = hls(&bridge, &old);
    let before = get(addr, &format!("/{token}/v0/seg1.m4s")).await;
    assert_eq!(
        (before.status, before.text()),
        (StatusCode::OK, "ladder".into())
    );

    old.0.replanned.store(true, Ordering::Relaxed);
    for name in [
        "master.m3u8",
        "v0/media.m3u8",
        "v0/init.mp4",
        "v0/seg1.m4s",
        "v1/seg1.m4s",
        "subs1.vtt",
    ] {
        assert_eq!(
            get(addr, &format!("/{token}/{name}")).await.status,
            StatusCode::GONE,
            "{name}"
        );
    }
    let notes = bridge.notes(&token);
    assert!(
        notes.iter().any(|n| n.contains("after a re-plan")),
        "{notes:?}"
    );

    let new = Src::new(b"");
    new.opens(0, 1, vec![Ok(Answer::whole(b"one encode", 1))]);
    let renewed = hls(&bridge, &new);
    let after = get(addr, &format!("/{renewed}/seg1.m4s")).await;
    assert_eq!(
        (after.status, after.text()),
        (StatusCode::OK, "one encode".into())
    );
}

#[tokio::test]
async fn the_same_bridge_serves_again_on_a_new_listener() {
    let bridge = Bridge::new();
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let token = hls(&bridge, &source);
    for _ in 0..2 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
        let addr = listener.local_addr().expect("bound");
        let serving = bridge.clone();
        let task = tokio::spawn(async move { serving.serve(listener).await });
        assert_eq!(
            get(addr, &format!("/{token}/media.m3u8")).await.status,
            StatusCode::OK
        );
        task.abort();
    }
}

#[tokio::test]
async fn each_request_is_the_tokens_last() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let token = hls(&bridge, &source);
    let registered = bridge.last_request(&token).expect("from its registration");
    assert!(bridge.last_request("nope").is_none());
    tokio::time::sleep(Duration::from_millis(20)).await;
    get(addr, &format!("/{token}/media.m3u8")).await;
    let asked = bridge.last_request(&token).expect("still served");
    assert!(asked > registered, "{asked:?} after {registered:?}");
    // A wrong token is no request of this one's.
    tokio::time::sleep(Duration::from_millis(20)).await;
    get(addr, "/0123456789abcdef/media.m3u8").await;
    assert_eq!(bridge.last_request(&token), Some(asked));
    bridge.unregister(&token);
    assert!(bridge.last_request(&token).is_none());
}

#[tokio::test]
async fn a_files_type_may_be_made_at_run_time() {
    let (bridge, addr) = bridge().await;
    let mime = format!("video/{}", "x-matroska");
    let token = bridge.register(Stream::new(
        Src::new(b"mkv"),
        Kind::direct(3, mime),
        None,
        Default::default(),
    ));
    let file = get(addr, &format!("/{token}/file")).await;
    assert_eq!(file.header(CONTENT_TYPE), Some("video/x-matroska"));
    assert_eq!(file.text(), "mkv");
}

#[tokio::test]
async fn a_missing_resource_is_not_found_and_a_retired_stream_says_so() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.0.subtitles.lock().insert(4, Err(Error::NotFound));
    let token = hls(&bridge, &source);
    assert_eq!(
        get(addr, &format!("/{token}/subs4.vtt")).await.status,
        StatusCode::NOT_FOUND
    );
    let stream = bridge.unregister(&token).expect("served");
    assert!(stream.retired());
    assert_eq!(stream.subtitle(), None);
    assert!(!stream.stopped() && !stream.expired());
}

#[tokio::test]
async fn a_token_the_caller_made_is_served_once_and_must_be_one_segment() {
    let (bridge, addr) = bridge().await;
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let stream = || Stream::new(source.clone(), Kind::hls(), None, Default::default());
    assert!(bridge.register_as("made-by_the_caller0", stream()));
    assert!(
        !bridge.register_as("made-by_the_caller0", stream()),
        "taken"
    );
    for bad in ["", "a/b", "a b", "a?b", "..", "%2e"] {
        assert!(!bridge.register_as(bad, stream()), "{bad:?}");
    }
    assert_eq!(
        get(addr, "/made-by_the_caller0/media.m3u8").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_request_hook_hears_each_served_token_before_its_answer() {
    let heard: Arc<Mutex<Vec<String>>> = Arc::default();
    let hearing = heard.clone();
    let bridge = Bridge::new().on_request(move |token| hearing.lock().push(token.to_owned()));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
    let addr = listener.local_addr().expect("bound");
    let serving = bridge.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    let source = Src::new(b"");
    source.playlist(ManifestKind::Media, 0, "#EXTM3U\n");
    let token = hls(&bridge, &source);
    get(addr, &format!("/{token}/media.m3u8")).await;
    get(addr, &format!("/{token}/nothing-here")).await;
    get(addr, "/0123456789abcdef/media.m3u8").await;
    assert_eq!(*heard.lock(), vec![token.clone(), token]);
}
