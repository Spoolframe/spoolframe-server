//! The playbacks a bridge serves, by token: the app's global `STREAMS` and
//! the functions over it, as a value (PX-49).

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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
    /// The file as it is: `size` bytes of `mime`.
    Direct { size: u64, mime: &'static str },
    /// The server's HLS output, at `epoch` (a far seek restarts the encode
    /// and moves it on).
    Hls { epoch: Mutex<u32> },
}

pub struct Stream<S> {
    /// The playback's reads.
    pub playback: S,
    pub retired: tokio::sync::watch::Sender<bool>,
    pub kind: Kind,
    /// The subtitle the server plays, if any.
    pub subtitle: Option<u32>,
    /// The item's subtitle tracks, to name in the master playlist.
    pub menu: Mutex<Option<Vec<MenuEntry>>>,
    /// The cue-less subtitle playlist the tracks not playing share, made
    /// once from the media playlist's length.
    pub placeholder: OnceCell<Vec<u8>>,
    /// A read found the server's admin had stopped the playback (P3-15).
    pub stopped: AtomicBool,
    /// A read found the server had ended the playback (expired, or ended
    /// for a newer connection's): the app starts it again where it was.
    pub expired: AtomicBool,
    /// What happened to the playback behind the player, for the app's
    /// playback log: shared by every path it's served under.
    pub notes: Arc<Mutex<Vec<String>>>,
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
        }
    }

    pub(crate) async fn cancelled(&self) {
        let mut retired = self.retired.subscribe();
        let _ = retired.wait_for(|retired| *retired).await;
    }
}

/// The playbacks served, by token. Clones share them, so the same bridge
/// can be served on a new listener after the old one stops.
pub struct Bridge<S> {
    pub(crate) streams: Arc<Mutex<HashMap<String, Arc<Stream<S>>>>>,
}

impl<S> Clone for Bridge<S> {
    fn clone(&self) -> Self {
        Self { streams: self.streams.clone() }
    }
}

impl<S: Source> Default for Bridge<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Source> Bridge<S> {
    pub fn new() -> Self {
        Self { streams: Default::default() }
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
            .is_some_and(|s| s.expired.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether the server stopped `token`'s playback (its admin did, P3-15):
    /// the player says so rather than fail as if the network had.
    pub fn stopped(&self, token: &str) -> bool {
        self.streams
            .lock()
            .expect("never poisoned")
            .get(token)
            .is_some_and(|s| s.stopped.load(std::sync::atomic::Ordering::Relaxed))
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

    /// `token`'s playback, while it's served.
    pub fn playback(&self, token: &str) -> Option<S>
    where
        S: Clone,
    {
        self.streams.lock().expect("never poisoned").get(token).map(|s| s.playback.clone())
    }

    pub fn unregister(&self, token: &str) -> Option<Arc<Stream<S>>> {
        let stream = self.streams.lock().expect("never poisoned").remove(token);
        if let Some(stream) = &stream { stream.retired.send_replace(true); }
        stream
    }
}
