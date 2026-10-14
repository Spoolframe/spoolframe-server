//! The bridge reads no environment (PX-49): what the app's bridge took from
//! `SPOOLFRAME_MASTER_REWRITE` and `SPOOLFRAME_BRIDGE_LOG` is in
//! `BridgeOptions` now. A test binary of its own, as it sets the variable
//! for the whole process before anything else runs.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::header::HOST;
use hyper_util::rt::TokioIo;
use spoolframe_hls_bridge::{
    Bridge, Chunk, Error, Kind, ManifestKind, Segment, Source, Stream, Subtitle,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n";

/// A playback with a master playlist and nothing else.
#[derive(Clone)]
struct Master(Arc<watch::Sender<()>>);

struct NoSegment;

impl Segment for NoSegment {
    fn sent(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }
    fn complete(&self) -> bool {
        true
    }
    fn epoch(&self) -> u32 {
        0
    }
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }
    async fn reopen(&self, _: u64) -> Result<Self, Error> {
        Err(Error::NotFound)
    }
}

impl Source for Master {
    type Snapshot = ();
    type Segment = NoSegment;

    fn changes(&self) -> watch::Receiver<()> {
        self.0.subscribe()
    }
    fn replanned(&self) -> bool {
        false
    }
    async fn read(&self, _: u64, _: u64) -> Result<Vec<u8>, Error> {
        Err(Error::NotFound)
    }
    async fn manifest(&self, kind: ManifestKind, _: u32) -> Result<String, Error> {
        match kind {
            ManifestKind::Multivariant => Ok(MASTER.into()),
            _ => Err(Error::NotFound),
        }
    }
    async fn read_segment(&self, _: u32, _: u32, _: u64, _: u64) -> Result<Chunk, Error> {
        Err(Error::NotFound)
    }
    async fn segment_stream(&self, _: u32, _: u32, _: u64, _: u64) -> Result<NoSegment, Error> {
        Err(Error::NotFound)
    }
    async fn read_subtitle_segment(&self, _: u32) -> Result<Subtitle, Error> {
        Err(Error::NotFound)
    }
}

async fn master(bridge: Bridge<Master>) -> String {
    let token = bridge.register(Stream::new(
        Master(Arc::new(watch::channel(()).0)),
        Kind::hls(),
        None,
        Default::default(),
    ));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
    let addr: SocketAddr = listener.local_addr().expect("bound");
    tokio::spawn(async move { bridge.serve(listener).await });
    let socket = TcpStream::connect(addr).await.expect("connects");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .expect("handshake");
    tokio::spawn(connection);
    let request = Request::get(format!("/{token}/master.m3u8"))
        .header(HOST, addr.to_string())
        .body(Empty::<Bytes>::new())
        .expect("a request");
    let body = sender
        .send_request(request)
        .await
        .expect("an answer")
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test]
async fn the_environment_rewrites_nothing_and_the_options_do() {
    // SAFETY: set before the runtime's other threads read anything, and
    // this binary has no other test.
    unsafe {
        std::env::set_var("SPOOLFRAME_MASTER_REWRITE", "BANDWIDTH>WIDTH");
        std::env::set_var("SPOOLFRAME_BRIDGE_LOG", "1");
    }
    assert_eq!(master(Bridge::new()).await, MASTER, "off by default");
    let rewriting = spoolframe_hls_bridge::BridgeOptions {
        master_rewrite: Some("BANDWIDTH>WIDTH".into()),
        ..Default::default()
    };
    assert_eq!(
        master(Bridge::with_options(rewriting)).await,
        MASTER.replace("BANDWIDTH", "WIDTH"),
        "as the options say"
    );
}
