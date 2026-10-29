//! Which of the streams a player has are the tracks the viewer picked.
//!
//! A [`Selection`] counts positions in the item's [`MediaTracks`] lists.
//! A direct play's file has the same streams in the same order, external
//! subtitles aside. A converted output holds only what was picked: one
//! audio, and the subtitle when the server copied or converted it into
//! the output.

use spoolframe_protocol::{
    ConvertedSubtitle, PlannedSubtitle, PlaybackPlan, Selection, SubtitleAction, SubtitleDelivery,
};
use spoolframe_types::MediaTracks;

/// The streams to pick in the player ([`map_selection`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPicks {
    /// The audio stream to pick, counted among the file's audio streams;
    /// `None` leaves the player's own.
    pub audio_stream: Option<usize>,
    pub subtitle: SubtitleHandling,
}

/// How the picked subtitle reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleHandling {
    /// In the picture already: the player shows none of its own, or it
    /// would show twice.
    Burned,
    /// The output's subtitle stream, the only one it has.
    InOutput,
    /// A file to fetch whole (`ManagedPlayback::subtitle_file`):
    /// an external one, or one the server reads as text.
    AsFile,
    /// The file's own subtitle stream, counted among its subtitle
    /// streams.
    Embedded(usize),
    /// No subtitle.
    None,
}

/// The streams that carry `selection` in what `plan` plays.
pub fn map_selection(
    tracks: &MediaTracks,
    plan: &PlaybackPlan,
    selection: Selection,
) -> StreamPicks {
    match plan {
        PlaybackPlan::Direct {
            audio, subtitle, ..
        } => StreamPicks {
            audio_stream: selection
                .audio
                .or(audio.map(|a| a.index))
                .map(|a| a as usize),
            subtitle: direct_subtitle(tracks, subtitle.as_ref(), selection.subtitle),
        },
        PlaybackPlan::Converted {
            audio, subtitle, ..
        } => StreamPicks {
            audio_stream: audio.is_some().then_some(0),
            subtitle: subtitle.map_or(SubtitleHandling::None, |s| converted_subtitle(&s)),
        },
        PlaybackPlan::Unsupported { .. } | PlaybackPlan::Unknown => StreamPicks {
            audio_stream: None,
            subtitle: SubtitleHandling::None,
        },
    }
}

fn direct_subtitle(
    tracks: &MediaTracks,
    planned: Option<&PlannedSubtitle>,
    picked: Option<u32>,
) -> SubtitleHandling {
    let Some(n) = picked else {
        return SubtitleHandling::None;
    };
    let Some(track) = tracks.subtitles.get(n as usize) else {
        return SubtitleHandling::None;
    };
    if !track.external {
        let embedded = tracks.subtitles[..n as usize]
            .iter()
            .filter(|t| !t.external)
            .count();
        return SubtitleHandling::Embedded(embedded);
    }
    // An external one is a file only when the plan sends it as one.
    match planned {
        Some(PlannedSubtitle {
            index,
            delivery: SubtitleDelivery::External,
            ..
        }) if *index == n => SubtitleHandling::AsFile,
        _ => SubtitleHandling::None,
    }
}

fn converted_subtitle(subtitle: &ConvertedSubtitle) -> SubtitleHandling {
    match subtitle.action {
        SubtitleAction::BurnIn { .. } => SubtitleHandling::Burned,
        SubtitleAction::Copy { .. } | SubtitleAction::Convert { .. } => SubtitleHandling::InOutput,
        SubtitleAction::Ocr { .. } => SubtitleHandling::AsFile,
        SubtitleAction::Unknown => SubtitleHandling::None,
    }
}

/// Whether the plan's subtitle comes as a file to fetch: a direct play's
/// external one, or one the server reads as text for a converted
/// playback.
pub fn subtitle_comes_as_file(plan: &PlaybackPlan) -> bool {
    matches!(
        plan,
        PlaybackPlan::Direct {
            subtitle: Some(PlannedSubtitle {
                delivery: SubtitleDelivery::External,
                ..
            }),
            ..
        } | PlaybackPlan::Converted {
            subtitle: Some(ConvertedSubtitle {
                action: SubtitleAction::Ocr { .. },
                ..
            }),
            ..
        }
    )
}

/// The subtitle tracks a menu offers that what plays now does not carry:
/// picking one asks the server for it
/// ([`crate::ManagedPlayback::change_subtitle`]). A direct play's file
/// has every embedded track, so only external ones, but the one the plan
/// sends already. A converted output has only the subtitle it was
/// planned with, so every other.
pub fn offered_but_not_in_stream(tracks: &MediaTracks, plan: &PlaybackPlan) -> Vec<u32> {
    let all = 0..tracks.subtitles.len() as u32;
    match plan {
        PlaybackPlan::Direct { subtitle, .. } => {
            let sent = subtitle
                .filter(|s| s.delivery == SubtitleDelivery::External)
                .map(|s| s.index);
            all.filter(|&n| tracks.subtitles[n as usize].external && Some(n) != sent)
                .collect()
        }
        PlaybackPlan::Converted { subtitle, .. } => {
            let carried = subtitle.map(|s| s.index);
            all.filter(|&n| Some(n) != carried).collect()
        }
        PlaybackPlan::Unsupported { .. } | PlaybackPlan::Unknown => all.collect(),
    }
}

/// The tracks to ask for to show subtitle `track`: the audio that plays
/// now, or the server's choice where none was known.
pub fn subtitle_change(playing: Option<Selection>, track: u32) -> Selection {
    Selection {
        audio: playing.and_then(|s| s.audio),
        subtitle: Some(track),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_protocol::AudioAction;
    use spoolframe_protocol::{ConvertedAudio, PlannedAudio};
    use spoolframe_types::{
        AudioCodec, AudioRole, AudioTrack, Container, Delivery, SubtitleFormat, SubtitleRole,
        SubtitleTrack,
    };

    fn subtitle(external: bool) -> SubtitleTrack {
        SubtitleTrack {
            language: Some("eng".into()),
            title: None,
            forced: false,
            default: false,
            external,
            role: SubtitleRole::default(),
            timing: None,
            download: None,
        }
    }

    fn audio() -> AudioTrack {
        AudioTrack {
            language: Some("eng".into()),
            title: None,
            channels: Some(2),
            default: false,
            role: AudioRole::Main,
        }
    }

    /// Two audio tracks; subtitles: embedded, external, embedded.
    fn tracks() -> MediaTracks {
        MediaTracks {
            audio: vec![audio(), audio()],
            subtitles: vec![subtitle(false), subtitle(true), subtitle(false)],
            ..MediaTracks::default()
        }
    }

    fn direct(subtitle: Option<PlannedSubtitle>) -> PlaybackPlan {
        PlaybackPlan::Direct {
            container: Container::Matroska,
            video: None,
            audio: Some(PlannedAudio {
                index: 1,
                codec: AudioCodec::Aac,
            }),
            subtitle,
            source_codec: None,
        }
    }

    fn external(index: u32) -> Option<PlannedSubtitle> {
        Some(PlannedSubtitle {
            index,
            format: SubtitleFormat::Srt,
            delivery: SubtitleDelivery::External,
        })
    }

    fn converted_with(action: Option<SubtitleAction>, audio: bool) -> PlaybackPlan {
        PlaybackPlan::Converted {
            delivery: Delivery::Progressive,
            container: Container::Matroska,
            video: None,
            audio: audio.then_some(ConvertedAudio {
                index: 1,
                action: AudioAction::Copy {
                    codec: AudioCodec::Aac,
                },
            }),
            subtitle: action.map(|action| ConvertedSubtitle { index: 2, action }),
            reasons: Vec::new(),
            ladder: Vec::new(),
            source_codec: None,
        }
    }

    fn pick(audio: Option<u32>, subtitle: Option<u32>) -> Selection {
        Selection { audio, subtitle }
    }

    fn subtitle_of(plan: &PlaybackPlan, track: Option<u32>) -> SubtitleHandling {
        map_selection(&tracks(), plan, pick(None, track)).subtitle
    }

    #[test]
    fn a_direct_play_picks_the_audio_asked_for() {
        let picks = map_selection(&tracks(), &direct(None), pick(Some(0), None));
        assert_eq!(picks.audio_stream, Some(0));
    }

    #[test]
    fn a_direct_play_with_no_audio_asked_for_picks_the_servers() {
        let picks = map_selection(&tracks(), &direct(None), Selection::default());
        assert_eq!(picks.audio_stream, Some(1));
    }

    #[test]
    fn an_embedded_subtitle_is_counted_among_the_files_own() {
        assert_eq!(
            subtitle_of(&direct(None), Some(0)),
            SubtitleHandling::Embedded(0)
        );
        // The external one before it is not in the file.
        assert_eq!(
            subtitle_of(&direct(None), Some(2)),
            SubtitleHandling::Embedded(1)
        );
    }

    #[test]
    fn an_external_subtitle_the_plan_sends_comes_as_a_file() {
        assert_eq!(
            subtitle_of(&direct(external(1)), Some(1)),
            SubtitleHandling::AsFile
        );
    }

    #[test]
    fn an_external_subtitle_the_plan_does_not_send_is_not_shown() {
        assert_eq!(subtitle_of(&direct(None), Some(1)), SubtitleHandling::None);
    }

    #[test]
    fn a_subtitle_past_the_track_list_is_not_shown() {
        assert_eq!(subtitle_of(&direct(None), Some(9)), SubtitleHandling::None);
    }

    #[test]
    fn no_subtitle_asked_for_shows_none() {
        assert_eq!(
            subtitle_of(&direct(external(1)), None),
            SubtitleHandling::None
        );
    }

    /// A subtitle the plan burns into the picture is neither fetched nor
    /// shown by the player: it would show twice.
    #[test]
    fn a_burned_in_subtitle_is_not_drawn_again() {
        let plan = converted_with(
            Some(SubtitleAction::BurnIn {
                format: SubtitleFormat::Pgs,
            }),
            true,
        );
        assert_eq!(subtitle_of(&plan, Some(2)), SubtitleHandling::Burned);
        assert!(!subtitle_comes_as_file(&plan));
    }

    #[test]
    fn a_copied_subtitle_is_shown_from_the_output() {
        let plan = converted_with(
            Some(SubtitleAction::Copy {
                format: SubtitleFormat::Srt,
            }),
            true,
        );
        assert_eq!(subtitle_of(&plan, Some(2)), SubtitleHandling::InOutput);
        assert!(!subtitle_comes_as_file(&plan));
    }

    #[test]
    fn a_converted_subtitle_is_shown_from_the_output() {
        let plan = converted_with(
            Some(SubtitleAction::Convert {
                from: SubtitleFormat::Srt,
                to: SubtitleFormat::WebVtt,
            }),
            true,
        );
        assert_eq!(subtitle_of(&plan, Some(2)), SubtitleHandling::InOutput);
    }

    #[test]
    fn a_subtitle_read_as_text_is_fetched_as_a_file() {
        let plan = converted_with(
            Some(SubtitleAction::Ocr {
                from: SubtitleFormat::Pgs,
                to: SubtitleFormat::Srt,
            }),
            true,
        );
        assert_eq!(subtitle_of(&plan, Some(2)), SubtitleHandling::AsFile);
        assert!(subtitle_comes_as_file(&plan));
    }

    #[test]
    fn a_converted_output_has_its_one_audio_or_none() {
        let with = map_selection(&tracks(), &converted_with(None, true), pick(Some(1), None));
        assert_eq!(with.audio_stream, Some(0));
        let without = map_selection(&tracks(), &converted_with(None, false), pick(Some(1), None));
        assert_eq!(without.audio_stream, None);
        assert_eq!(without.subtitle, SubtitleHandling::None);
    }

    #[test]
    fn an_external_file_on_a_direct_play_is_fetched() {
        assert!(subtitle_comes_as_file(&direct(external(1))));
        assert!(!subtitle_comes_as_file(&direct(None)));
    }

    #[test]
    fn a_direct_play_needs_the_server_only_for_external_subtitles() {
        assert_eq!(offered_but_not_in_stream(&tracks(), &direct(None)), [1]);
        assert!(offered_but_not_in_stream(&tracks(), &direct(external(1))).is_empty());
    }

    #[test]
    fn a_converted_output_needs_the_server_for_every_other_subtitle() {
        let plan = converted_with(
            Some(SubtitleAction::Copy {
                format: SubtitleFormat::Srt,
            }),
            true,
        );
        assert_eq!(offered_but_not_in_stream(&tracks(), &plan), [0, 1]);
        assert_eq!(
            offered_but_not_in_stream(&tracks(), &converted_with(None, true)),
            [0, 1, 2]
        );
    }

    #[test]
    fn a_new_subtitle_keeps_the_audio_that_plays() {
        assert_eq!(
            subtitle_change(Some(pick(Some(1), Some(0))), 4),
            pick(Some(1), Some(4))
        );
    }

    #[test]
    fn with_no_audio_known_the_server_chooses_it() {
        assert_eq!(subtitle_change(None, 4), pick(None, Some(4)));
    }
}
