//! Playback from optimized copies (PX-37). When a playback's plan would
//! encode its video and a ready copy fits, the copy plays in the film's
//! place: directly, or remuxed, so a seek anywhere answers at once. When
//! none fits but a copy holds the picked audio converted already, that
//! track is taken from the copy and only the video is encoded live.
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
//! A copy's tracks are its file's (`encode::copy_args`): the video, then
//! every audio track in the file's order, then the embedded subtitles in
//! order, but teletext and closed captions. [`aligned`] puts the copy's
//! tracks at their source's positions, so a selection and a plan's track
//! numbers are the item's, as the client knows them, whatever is played.
//! A copy whose tracks do not line up with its source's, a language or a
//! title apart, is not used. Subtitle files beside the film are not in a
//! copy: they keep their positions and are served by their path, as for
//! the film. A picked track the copy left out plays from the film.
//!
//! ABR still wins: a ladder (PX-21) is not replaced by a copy. It may
//! take its audio from one.

use std::cmp::Reverse;

use spoolframe_media::{MediaDescriptor, SubtitleSource, VideoStream};
use spoolframe_protocol::{
    AudioAction, ConvertedAudio, ConvertedVideo, PlaybackPlan, Selection, SubtitleDelivery,
};

use crate::encode::copy_args::UNWRITABLE_TEXT;
use crate::planner::{Kind, default_audio};
use crate::store::optimized::CopyVideo;

/// The common picture heights, tallest first: how far a copy may be
/// under the encode it stands in for.
const HEIGHTS: [u32; 9] = [2160, 1440, 1080, 720, 540, 480, 360, 240, 144];

/// The common height next under `height`: a copy that short still stands
/// in for an encode `height` tall. 0 under the shortest.
pub fn height_below(height: u32) -> u32 {
    HEIGHTS.iter().copied().find(|&h| h < height).unwrap_or(0)
}

/// A copy's file described with its source's tracks at their positions.
#[derive(Debug, Clone)]
pub struct Aligned {
    pub descriptor: MediaDescriptor,
    /// For each of the source's subtitles, its position among the copy
    /// file's own: `None` for a file beside the film, or a track the copy
    /// left out (which keeps the source's entry, never to be played from
    /// the copy).
    in_file: Vec<Option<u32>>,
}

impl Aligned {
    /// Whether subtitle `picked` plays from the copy as it would from the
    /// film: a file beside it, or a track the copy holds. A client
    /// playing the copy directly finds an embedded track by its position
    /// in the file, so there it must be where it is in the film.
    fn holds(&self, picked: Option<u32>, direct: bool) -> bool {
        let Some(picked) = picked else {
            return true;
        };
        let Some(track) = self.descriptor.subtitles.get(picked as usize) else {
            return false;
        };
        match (
            &track.source,
            self.in_file.get(picked as usize).copied().flatten(),
        ) {
            (SubtitleSource::External { .. }, _) => true,
            (SubtitleSource::Embedded { .. }, Some(at)) => !direct || at == picked,
            (SubtitleSource::Embedded { .. }, None) => false,
        }
    }
}

/// `copy`'s tracks at `source`'s positions, or `None` where they do not
/// line up: no video in either, another count of audio tracks or of
/// embedded subtitles kept, or a language or title apart.
pub fn aligned(source: &MediaDescriptor, copy: &MediaDescriptor) -> Option<Aligned> {
    copy.video.as_ref()?;
    source.video.as_ref()?;
    let same =
        |a: (&Option<String>, &Option<String>), b: (&Option<String>, &Option<String>)| a == b;
    if source.audio.len() != copy.audio.len()
        || !source
            .audio
            .iter()
            .zip(&copy.audio)
            .all(|(s, c)| same((&s.language, &s.title), (&c.language, &c.title)))
    {
        return None;
    }
    let mut copied = copy
        .subtitles
        .iter()
        .filter(|t| matches!(t.source, SubtitleSource::Embedded { .. }))
        .enumerate();
    let mut subtitles = Vec::with_capacity(source.subtitles.len());
    let mut in_file = Vec::with_capacity(source.subtitles.len());
    for track in &source.subtitles {
        match track.source {
            SubtitleSource::External { .. } => {
                subtitles.push(track.clone());
                in_file.push(None);
            }
            SubtitleSource::Embedded { .. } if UNWRITABLE_TEXT.contains(&track.codec.as_str()) => {
                subtitles.push(track.clone());
                in_file.push(None);
            }
            SubtitleSource::Embedded { .. } => {
                let (at, kept) = copied.next()?;
                if !same(
                    (&track.language, &track.title),
                    (&kept.language, &kept.title),
                ) {
                    return None;
                }
                subtitles.push(kept.clone());
                in_file.push(Some(at as u32));
            }
        }
    }
    if copied.next().is_some() {
        return None;
    }
    Some(Aligned {
        descriptor: MediaDescriptor {
            subtitles,
            ..copy.clone()
        },
        in_file,
    })
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
    /// place, by `plan`, of `descriptor`: the copy's tracks at the film's
    /// positions.
    Copy {
        copy: usize,
        plan: PlaybackPlan,
        descriptor: MediaDescriptor,
    },
    /// The film's video, encoded, with copy `copy`'s picked audio track as
    /// it is there: `descriptor` is the film's, that track the copy's.
    Audio {
        copy: usize,
        plan: PlaybackPlan,
        descriptor: MediaDescriptor,
    },
}

/// What plays `selection` of `source`, which was planned as `plan`, given
/// its ready `copies` (each its file's descriptor) and `replan`, which
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
    let lined_up: Vec<(usize, Aligned)> = copies
        .iter()
        .enumerate()
        .filter_map(|(at, copy)| Some((at, aligned(source, copy)?)))
        .collect();
    if let (Some(encode @ ConvertedVideo::Encode { .. }), Some(film), true) =
        (*video, source.video.as_ref(), ladder.is_empty())
    {
        let mut fitting: Vec<&(usize, Aligned)> = lined_up
            .iter()
            .filter(|(_, a)| {
                a.descriptor
                    .video
                    .as_ref()
                    .is_some_and(|copied| close(encode, film, copied))
            })
            .collect();
        fitting.sort_by_key(|(_, a)| {
            let height = a.descriptor.video.as_ref().map_or(0, |v| v.height);
            Reverse((height, a.descriptor.bit_rate.unwrap_or(0)))
        });
        for (at, a) in fitting {
            let planned = replan(&a.descriptor, selection);
            if plays_copy(&planned, a, selection) {
                return Choice::Copy {
                    copy: *at,
                    plan: planned,
                    descriptor: a.descriptor.clone(),
                };
            }
        }
    }
    if let Some(ConvertedAudio {
        index,
        action: AudioAction::Encode { .. },
    }) = *audio
    {
        for (at, a) in &lined_up {
            let Some(track) = a.descriptor.audio.get(index as usize) else {
                continue;
            };
            // The copy kept the film's own track: nothing to gain, and no
            // film to clone and plan again.
            if source
                .audio
                .get(index as usize)
                .is_some_and(|own| own.codec == track.codec)
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
                    copy: *at,
                    plan: planned,
                    descriptor: mixed,
                };
            }
        }
    }
    Choice::Source
}

/// Whether `plan`, of copy `a`, plays it with no video encode and no
/// ladder, its picked subtitle where the client finds it.
fn plays_copy(plan: &PlaybackPlan, a: &Aligned, selection: Selection) -> bool {
    match plan {
        PlaybackPlan::Direct { subtitle, .. } => {
            let embedded = subtitle.is_some_and(|s| s.delivery == SubtitleDelivery::Embedded);
            a.holds(selection.subtitle, embedded)
        }
        PlaybackPlan::Converted {
            video: Some(ConvertedVideo::Copy { .. }),
            ladder,
            ..
        } => {
            ladder.is_empty()
                && Kind::of(plan) != Kind::Unsupported
                && a.holds(selection.subtitle, false)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
