//! Tracks kept from listening (PX-51): a playback of a track writes the
//! bytes its reads bring into a [`BlockWriter`], and once the whole file
//! is here it becomes an entry held by the cache (`listening`), with no
//! second transfer.
//!
//! What is kept is what was streamed: the file itself for a direct play
//! (range reads, in any order, as the player seeks), or the conversion's
//! output for a progressive one (in order, from its start; a seek starts
//! a new encode and nothing is kept of it). A track skipped part-way
//! leaves nothing: its part is removed with the playback. A part is
//! never gone on with after the client stops (the ranges are not saved):
//! the manager removes them all at its start.

use std::sync::Arc;

use spoolframe_protocol::PlaybackPlan;
use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks, FileTracks};
use spoolframe_types::{Container, Delivery, Item, MediaId, MediaTracks};

use super::store::{EntryKind, Hold, Store, kept_key};
use super::writer::BlockWriter;
use super::{DownloadPhase, LocalWatch, Shared, manager, music, now};
use crate::{Chunk, PlaybackOptions, Session};

/// What a track's playback keeps of it.
pub(crate) struct Keeper {
    shared: Arc<Shared>,
    session: Session,
    id: MediaId,
    /// The encode began at the track's start: its output is the whole
    /// track.
    from_start: bool,
    state: tokio::sync::Mutex<State>,
}

enum State {
    /// Nothing read yet.
    Idle,
    /// The file, as direct reads bring it.
    Direct {
        writer: BlockWriter,
        part: std::path::PathBuf,
        container: Container,
    },
    /// A conversion's output, from its start, in encode `epoch`.
    Progressive {
        writer: BlockWriter,
        part: std::path::PathBuf,
        container: Container,
        epoch: u32,
    },
    /// Kept, or given up on: nothing more is written.
    Over,
}

impl Keeper {
    /// A keeper of `id` played in `session` with `options`, when it is a
    /// track this device keeps: music downloads on, a downloads
    /// directory, and not here whole already, downloaded or kept (then
    /// only its last play is noted).
    pub(crate) async fn of(
        session: &Session,
        id: &MediaId,
        options: &PlaybackOptions,
    ) -> Option<Arc<Self>> {
        if id.kind() != spoolframe_types::MediaKind::Track || session.music_offered().is_err() {
            return None;
        }
        let shared = session.client.inner.downloads.clone();
        shared.store.as_ref()?;
        if shared.played(&session.server(), id).await {
            return None;
        }
        Some(Arc::new(Self {
            shared,
            session: session.clone(),
            id: id.clone(),
            from_start: options.start.is_none_or(|start| start.is_zero()),
            state: tokio::sync::Mutex::new(State::Idle),
        }))
    }

    fn part(&self, store: &Store) -> std::path::PathBuf {
        let key = kept_key(&self.session.server(), &self.id);
        // One playback's part: two playbacks of the same track do not
        // write over each other.
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).expect("the OS gives random bytes");
        let nonce = u64::from_le_bytes(nonce);
        store
            .listening_parts()
            .join(format!("{key}-{nonce:016x}.part"))
    }

    /// A direct read of `plan`'s file of `size` bytes brought `bytes` at
    /// `offset`.
    pub(crate) async fn direct(&self, plan: &PlaybackPlan, size: u64, offset: u64, bytes: &[u8]) {
        let PlaybackPlan::Direct { container, .. } = plan else {
            return;
        };
        let mut state = self.state.lock().await;
        if matches!(*state, State::Idle) {
            *state = match self.open(size).await {
                Some((writer, part)) => State::Direct {
                    writer,
                    part,
                    container: *container,
                },
                None => State::Over,
            };
        }
        let State::Direct { writer, .. } = &mut *state else {
            return;
        };
        if let Err(e) = writer.write_at(offset, bytes).await {
            tracing::debug!(error = %e, "a track is not kept from listening");
            self.give_up(&mut state).await;
            return;
        }
        if writer.is_complete() {
            self.keep(&mut state).await;
        }
    }

    /// A progressive read of `source` brought `chunk` at `offset`.
    pub(crate) async fn chunk(&self, source: &crate::Playback, offset: u64, chunk: &Chunk) {
        let PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container,
            ..
        } = &source.plan
        else {
            return;
        };
        let mut state = self.state.lock().await;
        if matches!(*state, State::Idle) {
            // Only an encode from the track's start, read from its first
            // byte, is the whole track.
            *state = match (self.from_start && source.epoch == 0 && offset == 0)
                .then_some(())
                .map(|()| self.open(u64::MAX))
            {
                Some(open) => match open.await {
                    Some((writer, part)) => State::Progressive {
                        writer,
                        part,
                        container: *container,
                        epoch: chunk.epoch,
                    },
                    None => State::Over,
                },
                None => State::Over,
            };
        }
        let State::Progressive { writer, epoch, .. } = &mut *state else {
            return;
        };
        // A seek: another encode, from elsewhere.
        if chunk.epoch != *epoch {
            self.give_up(&mut state).await;
            return;
        }
        let mut done = writer.write_at(offset, &chunk.bytes).await;
        if done.is_ok() && chunk.complete {
            done = writer.set_size(chunk.size);
        }
        if let Err(e) = done {
            tracing::debug!(error = %e, "a track is not kept from listening");
            self.give_up(&mut state).await;
            return;
        }
        if writer.is_complete() {
            self.keep(&mut state).await;
        }
    }

    async fn open(&self, size: u64) -> Option<(BlockWriter, std::path::PathBuf)> {
        let store = self.shared.store.as_ref()?;
        let part = self.part(store);
        let opened = async {
            tokio::fs::create_dir_all(store.listening_parts()).await?;
            BlockWriter::open(&part, size, 0).await
        };
        match opened.await {
            Ok(writer) => Some((writer, part)),
            Err(e) => {
                tracing::info!(error = %e, "a track is not kept from listening");
                None
            }
        }
    }

    async fn give_up(&self, state: &mut State) {
        if let State::Direct { part, .. } | State::Progressive { part, .. } = &*state {
            let _ = tokio::fs::remove_file(part).await;
        }
        *state = State::Over;
    }

    /// The whole file is here: it becomes an entry of the cache, with the
    /// track's details and artwork, and the cache is brought under its
    /// cap.
    async fn keep(&self, state: &mut State) {
        let (writer, part, container, direct) = match std::mem::replace(state, State::Over) {
            State::Direct {
                writer,
                part,
                container,
            } => (writer, part, container, true),
            State::Progressive {
                writer,
                part,
                container,
                ..
            } => (writer, part, container, false),
            State::Idle | State::Over => return,
        };
        if let Err(e) = self.kept(writer, &part, container, direct).await {
            tracing::info!(id = %self.id, error = %e, "a track played whole is not kept");
            let _ = tokio::fs::remove_file(&part).await;
        }
    }

    async fn kept(
        &self,
        writer: BlockWriter,
        part: &std::path::Path,
        container: Container,
        direct: bool,
    ) -> Result<(), crate::SdkError> {
        let shared = &self.shared;
        let store = shared.store()?;
        let session = &self.session;
        let server = session.server();
        let user = session.user().clone();
        // Downloaded or kept meanwhile: that stands for it.
        if shared.played(&server, &self.id).await {
            return Ok(());
        }
        let size = {
            let mut writer = writer;
            writer.sync().await.map_err(super::disk)?;
            std::fs::metadata(part).map_err(super::disk)?.len()
        };
        let detail = session.item(&self.id).await?;
        let Item::Track(track) = &detail.item else {
            return Ok(());
        };
        let (album, artist) = tokio::join!(
            session.item(&track.album_id),
            session.item(&track.artist_id)
        );
        let (album, artist) = (album.ok().map(|d| d.item), artist.ok().map(|d| d.item));
        let key = kept_key(&server, &self.id);
        let mut m = Store::manifest(
            EntryKind::Track,
            key.clone(),
            server,
            user.user_id,
            user.username,
            detail.item.clone(),
            MediaTracks::default(),
            // The file as it is when it played direct; a conversion's
            // output is of no quality a download asks for.
            if direct {
                DownloadSource::Original
            } else {
                DownloadSource::Unknown
            },
            DownloadTracks::All,
            LocalWatch::default(),
            now(),
        );
        m.holds = vec![Hold::listening()];
        m.album = album;
        m.artist = artist;
        m.file = Some(DownloadFile {
            container,
            tracks: FileTracks {
                audio: vec![0],
                subtitles: Vec::new(),
            },
        });
        m.size = Some(size);
        m.offset = size;
        // Not hashed: only a download of the same size needs it, and
        // `music::kept_is` hashes it then.
        m.phase = DownloadPhase::Done;
        m.last_played_at = Some(now());
        let mut art = shared.kept_art.lock().clone();
        m.artwork = manager::fetch_artwork(session, store, &m, &mut art).await;
        shared.kept_art.lock().extend(art);
        let media = store.media(&m).expect("a kept track has its container");
        {
            let _writing = shared.manifests.lock().await;
            std::fs::create_dir_all(store.dir(&m)).map_err(super::disk)?;
            std::fs::rename(part, &media).map_err(super::disk)?;
            store.write(&m).map_err(super::disk)?;
        }
        tracing::debug!(key, size, "a track played whole is kept");
        shared.emit(&m);
        music::evict(shared, Some(&key)).await
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        // Skipped part-way: what was written goes.
        if let State::Direct { part, .. } | State::Progressive { part, .. } = &*self.state.get_mut()
        {
            let _ = std::fs::remove_file(part);
        }
    }
}
