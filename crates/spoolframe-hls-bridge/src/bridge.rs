//! The playbacks a bridge serves, by token: the app's global `STREAMS` and
//! the functions over it, as a value (PX-49).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::net::TcpListener;
use tokio::sync::OnceCell;

use crate::source::Source;

/// A subtitle track as the system player's menu names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    /// Its index among the item's subtitles, as [`Stream::subtitle`] names
    /// the one playing.
    pub index: u32,
    /// As the menu shows it.
    pub name: String,
    /// BCP 47 ("en"), when known.
    pub language: Option<String>,
    pub forced: bool,
}

/// What a playback's path serves.
pub enum Kind {
    /// The file as it is: `size` bytes of `mime` (made at run time by the
    /// server, from the file's container).
    Direct { size: u64, mime: Cow<'static, str> },
    /// The server's HLS output, at `epoch` (a far seek restarts the encode
    /// and moves it on).
    Hls { epoch: Mutex<u32> },
}

impl Kind {
    /// The file as it is: `size` bytes of `mime`.
    pub fn direct(size: u64, mime: impl Into<Cow<'static, str>>) -> Self {
        Self::Direct { size, mime: mime.into() }
    }

    /// HLS, from the first encode.
    pub fn hls() -> Self {
        Self::Hls { epoch: Mutex::new(0) }
    }
}

/// How a bridge behaves (PX-49): what the app's bridge read from its
/// environment. The crate reads none; `Default` turns both off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BridgeOptions {
    /// Log, through `tracing` at INFO, each playlist reference refused and
    /// each failed read's name: for debugging a server's playlists. The
    /// app's `SPOOLFRAME_BRIDGE_LOG=1`.
    pub log: bool,
    /// `"from>to;from>to"`: replace text in the master playlist, to find
    /// which attribute a player refuses. A debugging switch that nothing
    /// shipped sets; the app's `SPOOLFRAME_MASTER_REWRITE`.
    pub master_rewrite: Option<String>,
}

/// One playback as a bridge serves it under a token.
pub struct Stream<S> {
    /// The playback's reads.
    pub(crate) playback: S,
    pub(crate) retired: tokio::sync::watch::Sender<bool>,
    pub(crate) kind: Kind,
    /// The subtitle the server plays, if any.
    pub(crate) subtitle: Option<u32>,
    /// The item's subtitle tracks, to name in the master playlist.
    pub(crate) menu: Mutex<Option<Vec<MenuEntry>>>,
    /// The cue-less subtitle playlist the tracks not playing share, made
    /// once from the media playlist's length.
    pub(crate) placeholder: OnceCell<Vec<u8>>,
    /// A read found the server's admin had stopped the playback (P3-15).
    pub(crate) stopped: AtomicBool,
    /// A read found the server had ended the playback (expired, or ended
    /// for a newer connection's): the app starts it again where it was.
    pub(crate) expired: AtomicBool,
    /// What happened to the playback behind the player, for the app's
    /// playback log: shared by every path it's served under.
    pub(crate) notes: Arc<Mutex<Vec<String>>>,
    /// When its token was last asked for anything, or registered: what
    /// the server expires an idle token by (PX-49).
    pub(crate) last_request: Mutex<Instant>,
}

impl<S> Stream<S> {
    /// `playback` to serve as `kind`, not retired, with no menu yet.
    pub fn new(playback: S, kind: Kind, subtitle: Option<u32>, notes: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            playback,
            retired: tokio::sync::watch::channel(false).0,
            kind,
            subtitle,
            menu: Mutex::new(None),
            placeholder: OnceCell::new(),
            stopped: AtomicBool::new(false),
            expired: AtomicBool::new(false),
            notes,
            last_request: Mutex::new(Instant::now()),
        }
    }

    /// The playback's reads.
    pub fn playback(&self) -> &S {
        &self.playback
    }

    /// What its path serves.
    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    /// The subtitle the server plays, if any.
    pub fn subtitle(&self) -> Option<u32> {
        self.subtitle
    }

    /// Its playback log, shared by every path it's served under.
    pub fn notes(&self) -> &Arc<Mutex<Vec<String>>> {
        &self.notes
    }

    /// A read found the server's admin had stopped the playback (P3-15).
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// A read found the server had ended the playback.
    pub fn expired(&self) -> bool {
        self.expired.load(Ordering::Relaxed)
    }

    /// Answer no more of its requests: those under way end, `410`.
    pub fn retire(&self) {
        self.retired.send_replace(true);
    }

    /// Whether it was retired.
    pub fn retired(&self) -> bool {
        *self.retired.borrow()
    }

    pub(crate) async fn cancelled(&self) {
        let mut retired = self.retired.subscribe();
        let _ = retired.wait_for(|retired| *retired).await;
    }

    /// A request for it came now.
    pub(crate) fn asked(&self) {
        *self.last_request.lock().expect("never poisoned") = Instant::now();
    }
}

/// What [`Bridge::on_request`] calls, with the token of each request for
/// a playback served.
pub type RequestHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The playbacks served, by token. Clones share them, so the same bridge
/// can be served on a new listener after the old one stops.
pub struct Bridge<S> {
    pub(crate) streams: Arc<Mutex<HashMap<String, Arc<Stream<S>>>>>,
    pub(crate) options: Arc<BridgeOptions>,
    pub(crate) on_request: Option<RequestHook>,
}

impl<S> Clone for Bridge<S> {
    fn clone(&self) -> Self {
        Self { streams: self.streams.clone(), options: self.options.clone(), on_request: self.on_request.clone() }
    }
}

impl<S: Source> Default for Bridge<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Source> Bridge<S> {
    /// A bridge with the default options: no debug log, no rewrite.
    pub fn new() -> Self {
        Self::with_options(BridgeOptions::default())
    }

    /// A bridge that behaves as `options` say.
    pub fn with_options(options: BridgeOptions) -> Self {
        Self { streams: Default::default(), options: Arc::new(options), on_request: None }
    }

    /// This bridge calling `hook` with the token of each request for a
    /// playback it serves, as the request comes and before it is answered:
    /// a server times its tokens by its own clock (PX-49). Set it before
    /// the bridge is cloned or served; clones made before do not call it.
    pub fn on_request(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_request = Some(Arc::new(hook));
        self
    }

    /// When `token` was last asked for anything (or registered), while it
    /// is served: a server expires a token idle too long by it (PX-49).
    pub fn last_request(&self, token: &str) -> Option<Instant> {
        let stream = self.streams.lock().expect("never poisoned").get(token).cloned();
        stream.map(|s| *s.last_request.lock().expect("never poisoned"))
    }

    /// Answer every connection `listener` takes. Ends when accepting fails
    /// again and again (the socket is gone, taken back while the app was
    /// suspended): serve the bridge again on a new listener then.
    pub async fn serve(&self, listener: TcpListener) {
        crate::http::accept(self.clone(), listener).await
    }

    /// Answer one connection's requests, HTTP/1.1 kept alive.
    pub async fn serve_connection<I>(&self, io: I) -> Result<(), hyper::Error>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let bridge = self.clone();
        hyper::server::conn::http1::Builder::new()
            .serve_connection(
                io,
                hyper::service::service_fn(move |request| crate::http::answer(bridge.clone(), request)),
            )
            .await
    }

    /// The answer to one request, `/<token>/<name>`, for a caller that runs
    /// its own HTTP server. The request's body is never read.
    pub async fn answer<B>(&self, request: hyper::Request<B>) -> hyper::Response<crate::Body> {
        match crate::http::answer(self.clone(), request).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    /// Whether `token`'s playback was planned again since it was served:
    /// serve the new plan for the player to switch to.
    pub fn replanned(&self, token: &str) -> bool {
        self.streams
            .lock()
            .expect("never poisoned")
            .get(token)
            .is_some_and(|s| s.playback.replanned())
    }

    /// What happened to `token`'s playback behind the player since last asked.
    pub fn notes(&self, token: &str) -> Vec<String> {
        let stream = self.streams.lock().expect("never poisoned").get(token).cloned();
        stream.map(|s| std::mem::take(&mut *s.notes.lock().expect("never poisoned"))).unwrap_or_default()
    }

    /// Whether the server ended `token`'s playback under the player: start
    /// another at the same place.
    pub fn expired(&self, token: &str) -> bool {
        self.streams
            .lock()
            .expect("never poisoned")
            .get(token)
            .is_some_and(|s| s.expired())
    }

    /// Whether the server stopped `token`'s playback (its admin did, P3-15):
    /// the player says so rather than fail as if the network had.
    pub fn stopped(&self, token: &str) -> bool {
        self.streams
            .lock()
            .expect("never poisoned")
            .get(token)
            .is_some_and(|s| s.stopped())
    }

    /// Name `entries` in `token`'s master playlist; false if there's no such
    /// playback.
    pub fn set_menu(&self, token: &str, entries: Vec<MenuEntry>) -> bool {
        let Some(stream) = self.streams.lock().expect("never poisoned").get(token).cloned() else { return false };
        *stream.menu.lock().expect("never poisoned") = Some(entries);
        true
    }

    /// Serve `stream` under a new token; the token, for its URL.
    pub fn register(&self, stream: Stream<S>) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the OS has randomness");
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.streams.lock().expect("never poisoned").insert(token.clone(), Arc::new(stream));
        token
    }

    /// Serve `stream` under `token`, one the caller made (the server makes
    /// its own as it makes session tokens, PX-49): unguessable, and a
    /// single path segment. False, and nothing served, if `token` is
    /// served already or is not one segment of URL-safe characters.
    pub fn register_as(&self, token: &str, stream: Stream<S>) -> bool {
        let safe = !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !safe {
            return false;
        }
        let mut streams = self.streams.lock().expect("never poisoned");
        if streams.contains_key(token) {
            return false;
        }
        streams.insert(token.to_owned(), Arc::new(stream));
        true
    }

    /// `token`'s playback, while it's served.
    pub fn playback(&self, token: &str) -> Option<S>
    where
        S: Clone,
    {
        self.streams.lock().expect("never poisoned").get(token).map(|s| s.playback.clone())
    }

    /// Serve `token` no more: its requests under way end, `410`, and later
    /// ones are `404`. The stream it served, if it was.
    pub fn unregister(&self, token: &str) -> Option<Arc<Stream<S>>> {
        let stream = self.streams.lock().expect("never poisoned").remove(token);
        if let Some(stream) = &stream { stream.retire(); }
        stream
    }
}
