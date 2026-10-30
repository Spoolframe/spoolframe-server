//! What an app works out about a download it shows, with no server: where
//! it plays from, what it may do with it, how far along it is, which
//! tracks its file holds, and a list kept in step with the events. Moved
//! here from the Linux and Apple apps, which each had their own.

use std::time::Duration;

use spoolframe_protocol::Selection;
use spoolframe_protocol::downloads::{DownloadFailure, DownloadState, DownloadTracks};
use spoolframe_types::MediaTracks;

use super::{DownloadEvent, DownloadPhase, LocalDownload};

/// What an app may offer for a download, by its phase
/// ([`LocalDownload::actions`]), in the order to show them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum DownloadAction {
    /// A finished one, from its file, with or without a server.
    Play,
    Pause,
    Resume,
    /// A failed one, again ([`super::Downloads::retry`]).
    Retry,
    /// One not finished stopped and taken off this device
    /// ([`super::Downloads::cancel`]).
    Cancel,
    /// A failed one off this device: a cancel, under another word.
    Dismiss,
    /// A finished one off this device ([`super::Downloads::delete`]),
    /// once the user confirms.
    Delete,
}

impl LocalDownload {
    /// The user watched it here, or the server said so as of the download
    /// or the last sync.
    pub fn watched_here(&self) -> bool {
        self.watch.watched == Some(true)
    }

    /// Where it plays from on its own Play: where the user stopped in it
    /// here; the start (`None`) once watched, or never begun, as
    /// [`spoolframe_types::PlaybackState::resume_from`] has it.
    pub fn resume_point(&self) -> Option<Duration> {
        match self.watched_here() {
            true => None,
            false => self.watch.position.filter(|p| !p.is_zero()),
        }
    }

    /// Where it plays from when its item page asked for `asked` (Resume at
    /// the server's place, or the start with `None`): while the server has
    /// not heard where the user got to here, that place wins
    /// ([`LocalDownload::resume_point`]); else the server's, which the page
    /// showed.
    pub fn resume_from(&self, asked: Option<Duration>) -> Option<Duration> {
        match (asked, self.watch.pending) {
            (None, _) => None,
            (Some(_), true) => self.resume_point(),
            (Some(at), false) => Some(at),
        }
    }

    /// A failed download worth trying again, by its cause: one short of
    /// room (once some is freed), a network failure, or a conversion that
    /// stopped. Not one whose source is gone. For `Other` or a cause this
    /// build does not know, by its size: the server sets it once the file
    /// is ready, so without one it failed while the server prepared it,
    /// and another try prepares the same thing the same way.
    pub fn can_retry(&self) -> bool {
        match &self.phase {
            DownloadPhase::Failed { cause, .. } => match cause {
                DownloadFailure::Other | DownloadFailure::Unknown => self.size.is_some(),
                cause => cause.suggested().is_some_and(|remedy| {
                    remedy != spoolframe_protocol::downloads::FailureRemedy::Remove
                }),
            },
            _ => false,
        }
    }

    /// What to offer for it now, in order.
    pub fn actions(&self) -> Vec<DownloadAction> {
        use DownloadAction::*;
        match &self.phase {
            DownloadPhase::Preparing { .. } => vec![Cancel],
            DownloadPhase::Transferring => vec![Pause, Cancel],
            DownloadPhase::Paused => vec![Resume, Cancel],
            DownloadPhase::Done => vec![Play, Delete],
            DownloadPhase::Failed { .. } if self.can_retry() => vec![Retry, Dismiss],
            DownloadPhase::Failed { .. } => vec![Dismiss],
        }
    }

    /// How far along it is, 0 to 1, while there is something to show: the
    /// server's conversion or writing while it prepares the file, then
    /// the bytes here of its size (or of the size the server expected).
    /// `None` when finished, failed, or with nothing known yet.
    pub fn progress(&self) -> Option<f64> {
        let percent = |p: u8| f64::from(p.min(100)) / 100.0;
        match &self.phase {
            DownloadPhase::Preparing {
                server: DownloadState::Converting { done_percent, .. },
            }
            | DownloadPhase::Preparing {
                server: DownloadState::Writing { done_percent },
            } => done_percent.map(percent),
            DownloadPhase::Preparing { .. } => None,
            DownloadPhase::Transferring | DownloadPhase::Paused => {
                let expected = self.expected_size.filter(|_| self.received > 0);
                let size = self.size.or(expected).filter(|s| *s > 0)?;
                Some((self.received as f64 / size as f64).min(1.0))
            }
            DownloadPhase::Done | DownloadPhase::Failed { .. } => None,
        }
    }

    /// The tracks in its file, as a player and its track menus list them:
    /// the item's tracks cut down to what the file carries, in the file's
    /// order. A place not in the item is skipped, never guessed at. Until
    /// the server says what the file is, what was asked for.
    pub fn file_tracks(&self) -> MediaTracks {
        let (audio, subtitles) = self.places();
        MediaTracks {
            audio: pick(&self.tracks.audio, &audio),
            subtitles: pick(&self.tracks.subtitles, &subtitles),
            ..self.tracks.clone()
        }
    }

    /// `selection`, of the item's tracks, as the same tracks of its file
    /// ([`LocalDownload::file_tracks`]): what a player picks among the
    /// file's streams. A track the file does not carry is the file's
    /// default audio (`None`), or subtitles off.
    pub fn file_selection(&self, selection: Selection) -> Selection {
        let (audio, subtitles) = self.places();
        let within = |places: &[u32], track: Option<u32>| {
            places
                .iter()
                .position(|&p| Some(p) == track)
                .and_then(|at| u32::try_from(at).ok())
        };
        Selection {
            audio: within(&kept(self.tracks.audio.len(), &audio), selection.audio),
            subtitle: within(
                &kept(self.tracks.subtitles.len(), &subtitles),
                selection.subtitle,
            ),
        }
    }

    /// The file's tracks to play: `asked` (of the item's tracks, as the
    /// player's menus or the user's choice for the item have them) as the
    /// file's; with nothing asked, the file's first audio and no
    /// subtitles, the way it was downloaded.
    pub fn pick_tracks(&self, asked: Option<Selection>) -> Selection {
        match asked {
            Some(asked) => self.file_selection(asked),
            None => Selection {
                audio: (!self.file_tracks().audio.is_empty()).then_some(0),
                subtitle: None,
            },
        }
    }

    /// The streams of its file to play for `asked`, as
    /// [`LocalDownload::pick_tracks`] picks them. Every subtitle in a
    /// download is a stream of the file, one the server had beside the
    /// film too: the file's place is the stream's, never counted past
    /// the item's external ones as [`crate::tracks::map_selection`] counts
    /// a direct play's.
    pub fn stream_picks(&self, asked: Option<Selection>) -> crate::tracks::StreamPicks {
        use crate::tracks::{StreamPicks, SubtitleHandling};
        let picked = self.pick_tracks(asked);
        StreamPicks {
            audio_stream: picked.audio.map(|a| a as usize),
            subtitle: picked.subtitle.map_or(SubtitleHandling::None, |s| {
                SubtitleHandling::Embedded(s as usize)
            }),
        }
    }

    /// The places, in the item's tracks, of the audio and subtitles in the
    /// file; until the server says what the file is, what was asked for.
    fn places(&self) -> (Vec<u32>, Vec<u32>) {
        let count = |n: usize| (0..u32::try_from(n).unwrap_or(u32::MAX)).collect();
        match (&self.file, self.chosen.normalized()) {
            (Some(file), _) => (file.tracks.audio.clone(), file.tracks.subtitles.clone()),
            (None, DownloadTracks::Chosen { audio, subtitles }) => (audio, subtitles),
            (None, _) => (
                count(self.tracks.audio.len()),
                count(self.tracks.subtitles.len()),
            ),
        }
    }
}

/// The places of `places` below `len`, in order: those the file's tracks
/// list.
fn kept(len: usize, places: &[u32]) -> Vec<u32> {
    places
        .iter()
        .copied()
        .filter(|&place| (place as usize) < len)
        .collect()
}

/// The tracks of `all` at `places`, in that order; a place past the end
/// is skipped.
fn pick<T: Clone>(all: &[T], places: &[u32]) -> Vec<T> {
    places
        .iter()
        .filter_map(|&place| all.get(place as usize).cloned())
        .collect()
}

/// What [`DownloadList::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum Applied {
    /// One listed changed, in place.
    Updated,
    Added,
    Removed,
    /// Nothing listed changed: the removal of one not listed, or an
    /// event that is not about one download.
    Nothing,
}

/// A list of downloads an app shows ([`super::Downloads::list`]), kept in
/// step with [`DownloadEvent`]s as the SDK's own list would be: oldest
/// first, a new one at the end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadList {
    pub downloads: Vec<LocalDownload>,
}

impl DownloadList {
    pub fn new(downloads: Vec<LocalDownload>) -> Self {
        Self { downloads }
    }

    /// `event` applied to the list.
    pub fn apply(&mut self, event: &DownloadEvent) -> Applied {
        match event {
            DownloadEvent::Changed(changed) => {
                match self.downloads.iter_mut().find(|d| d.key == changed.key) {
                    Some(listed) => {
                        *listed = changed.clone();
                        Applied::Updated
                    }
                    None => {
                        self.downloads.push(changed.clone());
                        Applied::Added
                    }
                }
            }
            DownloadEvent::Removed { key } => {
                match self.downloads.iter().position(|d| d.key == *key) {
                    Some(at) => {
                        self.downloads.remove(at);
                        Applied::Removed
                    }
                    None => Applied::Nothing,
                }
            }
            _ => Applied::Nothing,
        }
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
