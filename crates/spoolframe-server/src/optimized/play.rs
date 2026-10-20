//! Playback from optimized copies (PX-37). When a playback's plan would
//! encode its video and a ready copy fits, the copy plays in the film's
//! place, remuxed, so a seek anywhere answers at once. When none fits but
//! a copy holds the picked audio converted already, that track is taken
//! from it and only the video is encoded live.
//!
//! A copy is its media file and its stored tracks (the track store, the
//! user's decision 2026-10-20). It is described as the film is, with each
//! stored track in its source stream's place ([`built`]): the stored
//! video for the film's, a stored audio track for the one it was made
//! from, and every other stream the film's own. So a selection and a
//! plan's track numbers are the item's, as the client knows them,
//! whatever is played. Its container is none a client plays
//! ([`COPY_CONTAINER`]): there is no one file to read by range, and the
//! planner remuxes it, the film and its tracks read together.
//!
//! A copy fits a plan that encodes the video when:
//!
//! - its range is the plan's: SDR when the plan tone maps an HDR film or
//!   the film is SDR, the film's own HDR when the plan keeps it;
//! - it is no shorter than the common height next under the encode's
//!   ([`height_below`]): not much worse than what would be encoded;
//! - planned again with the copy as the film, for the same device,
//!   quality and link, nothing encodes its video and there is no ladder:
//!   the device decodes its codec, takes its size and range, and its
//!   bitrate is under the quality's cap (P6-06) and the link's.
//!
//! Of the copies that fit, the tallest plays, then the one at the highest
//! bitrate.
//!
//! ABR still wins: a ladder (PX-21) is not replaced by a copy. It may
//! take its audio from one.

use std::cmp::Reverse;

use spoolframe_media::{MediaDescriptor, VideoStream};
use spoolframe_protocol::{AudioAction, ConvertedAudio, ConvertedVideo, PlaybackPlan, Selection};

use crate::planner::{Kind, default_audio};
use crate::store::optimized::CopyVideo;

/// The container a copy is described in: none a client plays as it is,
/// so a copy is always remuxed, never read by range.
pub const COPY_CONTAINER: &str = "spoolframe-tracks";

/// The common picture heights, tallest first: how far a copy may be
/// under the encode it stands in for.
const HEIGHTS: [u32; 9] = [2160, 1440, 1080, 720, 540, 480, 360, 240, 144];

/// The common height next under `height`: a copy that short still stands
/// in for an encode `height` tall. 0 under the shortest.
pub fn height_below(height: u32) -> u32 {
    HEIGHTS.iter().copied().find(|&h| h < height).unwrap_or(0)
}

/// A copy of `source` described as the planner sees it: `video` (its
/// stored video file's description) in the film's video's place, and
/// each of `audio` (a source stream index and its stored track's
/// description) in that audio track's place, its language, title, flags
/// and role the source track's. A stored stream is stream 0 of its file.
/// `None` where a stored file has no stream of its kind, or stands in for
/// a stream the film does not have.
pub fn built(
    source: &MediaDescriptor,
    video: Option<&MediaDescriptor>,
    audio: &[(u32, &MediaDescriptor)],
) -> Option<MediaDescriptor> {
    let mut copy = MediaDescriptor {
        container: COPY_CONTAINER.to_owned(),
        ..source.clone()
    };
    if let Some(stored) = video {
        source.video.as_ref()?;
        copy.video = Some(stored.video.clone()?);
        copy.bit_rate = stored.bit_rate.or(source.bit_rate);
    }
    for (stream, stored) in audio {
        let at = source.audio.iter().position(|a| a.index == *stream)?;
        let own = &source.audio[at];
        let track = stored.audio.first()?;
        copy.audio[at] = spoolframe_media::AudioStream {
            language: own.language.clone(),
            title: own.title.clone(),
            default: own.default,
            role: own.role,
            ..track.clone()
        };
    }
    Some(copy)
}

/// Whether a copy of `copied` video stands in for `plan`'s encode of the
/// film's `source` video, before it is planned again: the same range, and
/// no shorter than the height next under the encode's.
fn close(plan: ConvertedVideo, source: &VideoStream, copied: &VideoStream) -> bool {
    let ConvertedVideo::Encode {
        height, tone_map, ..
    } = plan
    else {
        return false;
    };
    let sdr_out = tone_map || source.hdr.is_none();
    copied.hdr.is_none() == sdr_out && copied.height >= height_below(height)
}

/// Whether a ready copy made as `video` may stand in for `plan` of
/// `source`, by its row alone, before its file is described: one that may
/// pass [`close`] (it is never taller than its row says), or any when the
/// plan encodes its audio, which a copy may hold converted already.
pub fn worth_describing(plan: &PlaybackPlan, source: &MediaDescriptor, video: &CopyVideo) -> bool {
    let PlaybackPlan::Converted {
        video: planned,
        audio,
        ..
    } = plan
    else {
        return false;
    };
    let audio_encodes = matches!(
        audio,
        Some(ConvertedAudio {
            action: AudioAction::Encode { .. },
            ..
        })
    );
    let video_may_fit = match (planned, source.video.as_ref()) {
        (
            Some(ConvertedVideo::Encode {
                height, tone_map, ..
            }),
            Some(film),
        ) => {
            let sdr_film = film.hdr.is_none();
            (video.tone_map || sdr_film) == (*tone_map || sdr_film)
                && video.height >= height_below(*height)
        }
        _ => false,
    };
    audio_encodes || video_may_fit
}

/// What a playback plays.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    /// The film, as it was planned.
    Source,
    /// Copy `copy` (its position in the copies given) in the film's
    /// place, remuxed by `plan`, of `descriptor`: its [`built`] one.
    Copy {
        copy: usize,
        plan: PlaybackPlan,
        descriptor: MediaDescriptor,
    },
    /// The film's video, encoded, with copy `copy`'s stored picked audio
    /// track as it is there: `descriptor` is the film's, that track the
    /// stored one's.
    Audio {
        copy: usize,
        plan: PlaybackPlan,
        descriptor: MediaDescriptor,
    },
}

/// What plays `selection` of `source`, which was planned as `plan`, given
/// its ready `copies` (each described by [`built`]) and `replan`, which
/// plans a descriptor for the same device, quality and link.
pub fn choose(
    source: &MediaDescriptor,
    plan: &PlaybackPlan,
    selection: Selection,
    copies: &[&MediaDescriptor],
    replan: impl Fn(&MediaDescriptor, Selection) -> PlaybackPlan,
) -> Choice {
    let PlaybackPlan::Converted {
        video,
        audio,
        ladder,
        ..
    } = plan
    else {
        return Choice::Source;
    };
    // The film's default, which a copy's own flags need not repeat.
    let selection = Selection {
        audio: selection
            .audio
            .or_else(|| default_audio(source).map(|i| i as u32)),
        ..selection
    };
    if let (Some(encode @ ConvertedVideo::Encode { .. }), Some(film), true) =
        (*video, source.video.as_ref(), ladder.is_empty())
    {
        let mut fitting: Vec<(usize, &MediaDescriptor)> = copies
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, copy)| {
                copy.video
                    .as_ref()
                    .is_some_and(|copied| close(encode, film, copied))
            })
            .collect();
        fitting.sort_by_key(|(_, copy)| {
            let height = copy.video.as_ref().map_or(0, |v| v.height);
            Reverse((height, copy.bit_rate.unwrap_or(0)))
        });
        for (at, copy) in fitting {
            let planned = replan(copy, selection);
            if plays_copy(&planned) {
                return Choice::Copy {
                    copy: at,
                    plan: planned,
                    descriptor: copy.clone(),
                };
            }
        }
    }
    if let Some(ConvertedAudio {
        index,
        action: AudioAction::Encode { .. },
    }) = *audio
    {
        for (at, copy) in copies.iter().enumerate() {
            let Some(track) = copy.audio.get(index as usize) else {
                continue;
            };
            // The copy keeps the film's own track: nothing to gain, and no
            // film to clone and plan again.
            if source
                .audio
                .get(index as usize)
                .is_none_or(|own| own.codec == track.codec)
            {
                continue;
            }
            let mut mixed = source.clone();
            mixed.audio[index as usize] = track.clone();
            let planned = replan(&mixed, selection);
            // Still a conversion of the film: one that would play the
            // film as it is now plays its own audio, not the copy's.
            if let PlaybackPlan::Converted {
                audio:
                    Some(ConvertedAudio {
                        action: AudioAction::Copy { .. },
                        ..
                    }),
                ..
            } = planned
            {
                return Choice::Audio {
                    copy: at,
                    plan: planned,
                    descriptor: mixed,
                };
            }
        }
    }
    Choice::Source
}

/// Whether `plan` plays a copy with no video encode and no ladder: a
/// remux of it. (A copy is never played directly: it is no one file.)
fn plays_copy(plan: &PlaybackPlan) -> bool {
    match plan {
        PlaybackPlan::Converted {
            video: Some(ConvertedVideo::Copy { .. }),
            ladder,
            ..
        } => ladder.is_empty() && Kind::of(plan) != Kind::Unsupported,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
