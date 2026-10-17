use super::*;

const SRT: &str = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello, there.\r\n\r\n\
                   2\r\n00:01:02,250 --> 00:01:04,000 X1:10 X2:20\r\nSecond --> line\r\n\r\n";

#[test]
fn srt_cues_are_read_with_crlf_and_coordinates() {
    let cues = cues(SRT.as_bytes(), TextFormat::Srt).unwrap();
    let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
    assert_eq!(times, [(1000, 2500), (62_250, 64_000)]);
}

#[test]
fn webvtt_cues_may_leave_out_the_hours_and_carry_settings() {
    let vtt = "WEBVTT\n\nNOTE a note\n\n01:02.500 --> 01:03.000 align:start\nHi\n\n\
               1:00:00.000 --> 1:00:01.250\nLater\n";
    let cues = cues(vtt.as_bytes(), TextFormat::WebVtt).unwrap();
    let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
    assert_eq!(times, [(62_500, 63_000), (3_600_000, 3_601_250)]);
}

const ASS: &str = "[Script Info]\nTitle: x\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n\
                   [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hello, there, friend\n\
                   Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,not a cue\n\
                   Dialogue: 0,1:02:03.45,1:02:04.00,Default,,0,0,0,,{\\i1}Later{\\i0}\n";

#[test]
fn ass_dialogue_times_are_read_by_the_format_line() {
    let cues = cues(ASS.as_bytes(), TextFormat::Ass).unwrap();
    let times: Vec<_> = cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
    assert_eq!(times, [(1000, 2500), (3_723_450, 3_724_000)]);
    // Another order of fields, as SSA files have it.
    let reordered =
        "[Events]\nFormat: Marked, End, Start, Text\nDialogue: Marked=0,0:00:09.00,0:00:08.00,Hi\n";
    let read = super::cues(reordered.as_bytes(), TextFormat::Ass).unwrap();
    assert_eq!((read[0].start_ms, read[0].end_ms), (8000, 9000));
}

#[test]
fn a_file_without_cues_is_unreadable() {
    assert!(matches!(
        cues(b"just some text\n", TextFormat::Srt),
        Err(SyncError::Unreadable(_))
    ));
    assert!(matches!(
        cues(b"\xff\xfe1\x00", TextFormat::Srt),
        Err(SyncError::Unreadable(_))
    ));
}

#[test]
fn a_shift_changes_only_the_times() {
    // Latin-1 text, which is not UTF-8: copied as it is.
    let mut srt = SRT.as_bytes().to_vec();
    srt.extend_from_slice(b"3\n00:02:00,000 --> 00:02:01,000\nCaf\xe9\n");
    let shifted = shift(&srt, TextFormat::Srt, 1500).unwrap();
    let expected = String::from_utf8_lossy(&srt)
        .replace(
            "00:00:01,000 --> 00:00:02,500",
            "00:00:02,500 --> 00:00:04,000",
        )
        .replace(
            "00:01:02,250 --> 00:01:04,000",
            "00:01:03,750 --> 00:01:05,500",
        )
        .replace(
            "00:02:00,000 --> 00:02:01,000",
            "00:02:01,500 --> 00:02:02,500",
        );
    assert_eq!(String::from_utf8_lossy(&shifted), expected);
    assert!(shifted.ends_with(b"Caf\xe9\n"));
}

#[test]
fn a_shift_before_the_start_stops_at_zero() {
    let back = shift(SRT.as_bytes(), TextFormat::Srt, -1500).unwrap();
    let cues = cues(&back, TextFormat::Srt).unwrap();
    assert_eq!((cues[0].start_ms, cues[0].end_ms), (0, 1000));
}

#[test]
fn ass_and_webvtt_are_written_in_their_own_form() {
    let shifted = shift(ASS.as_bytes(), TextFormat::Ass, 250).unwrap();
    let text = String::from_utf8(shifted).unwrap();
    assert!(
        text.contains("Dialogue: 0,0:00:01.25,0:00:02.75,Default,,0,0,0,,Hello, there, friend")
    );
    assert!(text.contains("Comment: 0,0:00:05.00"), "{text}");
    let vtt = shift(
        b"WEBVTT\n\n01:02.500 --> 01:03.000 align:start\nHi\n",
        TextFormat::WebVtt,
        1000,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(vtt).unwrap(),
        "WEBVTT\n\n00:01:03.500 --> 00:01:04.000 align:start\nHi\n"
    );
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + (self.next() % (high - low) as u64) as i64
    }
}

/// A film's dialogue as a subtitle has it: 40 minutes of cues, enough
/// for alass, short enough for an unoptimized test build.
fn film(seed: u64) -> Vec<Span> {
    let mut rng = Rng(seed);
    let mut at = 5_000;
    let mut cues = Vec::new();
    while at < 40 * 60 * 1000 {
        let length = rng.range(800, 5_000);
        cues.push(Span {
            start_ms: at,
            end_ms: at + length,
        });
        let gap = if rng.range(0, 30) == 0 {
            rng.range(15_000, 60_000)
        } else {
            rng.range(150, 6_000)
        };
        at += length + gap;
    }
    cues
}

/// The speech a voice detector finds where `cues` are: each line's
/// speech a little inside its cue and in pieces, a tenth of the cues with
/// none (signs, songs), and speech no cue has between them.
fn speech_of(cues: &[Span], seed: u64) -> Vec<Span> {
    let mut rng = Rng(seed);
    let mut spans = Vec::new();
    for (i, cue) in cues.iter().enumerate() {
        if rng.range(0, 10) == 0 {
            continue;
        }
        let mut at = cue.start_ms + rng.range(0, 300);
        let end = cue.end_ms - rng.range(0, 400);
        while at < end {
            let piece = rng.range(300, 1_500);
            spans.push(Span {
                start_ms: at,
                end_ms: (at + piece).min(end),
            });
            at += piece + rng.range(80, 300);
        }
        if let Some(next) = cues.get(i + 1)
            && rng.range(0, 5) == 0
            && next.start_ms - cue.end_ms > 1_500
        {
            let start = cue.end_ms + 500;
            spans.push(Span {
                start_ms: start,
                end_ms: start + rng.range(200, (next.start_ms - start - 300).max(201)),
            });
        }
    }
    spans
}

/// Another language's subtitle of the same release: the same lines, each
/// timed a little differently.
fn other_language(cues: &[Span], seed: u64) -> Vec<Span> {
    let mut rng = Rng(seed);
    cues.iter()
        .map(|c| Span {
            start_ms: c.start_ms + rng.range(-120, 120),
            end_ms: c.end_ms + rng.range(-200, 200),
        })
        .collect()
}

fn mapped(cues: &[Span], f: impl Fn(i64) -> i64) -> Vec<Span> {
    cues.iter()
        .map(|c| Span {
            start_ms: f(c.start_ms),
            end_ms: f(c.end_ms),
        })
        .collect()
}

/// `wrong`'s cues aligned to `reference`, and where each of those that
/// did not come back within 0.2 s of where it belongs (`right`), start
/// and end, belongs.
fn misplaced(reference: &[Span], wrong: &[Span], right: &[Span]) -> (Alignment, Vec<Span>) {
    let alignment = align(reference, wrong).unwrap();
    assert!(alignment.certain(), "score {}", alignment.score);
    let retiming = Retiming {
        factor: alignment.factor,
        deltas: alignment.deltas.clone(),
    };
    let off = wrong
        .iter()
        .zip(right)
        .enumerate()
        .filter(|(i, (cue, want))| {
            let (start, end) = (
                retiming.apply(*i, cue.start_ms),
                retiming.apply(*i, cue.end_ms),
            );
            (start - want.start_ms).abs() > 200 || (end - want.end_ms).abs() > 200
        })
        .map(|(_, (_, want))| *want)
        .collect();
    (alignment, off)
}

/// Every one of `wrong`'s cues back within 0.2 s of where it belongs.
fn assert_synced(reference: &[Span], wrong: &[Span], right: &[Span]) -> Alignment {
    let (alignment, off) = misplaced(reference, wrong, right);
    assert_eq!(off, [], "cues not back in place");
    alignment
}

/// The two references the ticket names: an embedded subtitle in another
/// language, and the speech in the audio.
fn references(cues: &[Span]) -> [Vec<Span>; 2] {
    [other_language(cues, 7), speech_of(cues, 11)]
}

#[test]
fn a_shifted_subtitle_is_synced_to_within_a_fifth_of_a_second() {
    let cues = film(1);
    let shifted = mapped(&cues, |t| t + 3_200);
    for reference in references(&cues) {
        let alignment = assert_synced(&reference, &shifted, &cues);
        assert_eq!(alignment.factor, 1.0);
        assert!(
            (alignment.offset_ms + 3_200).abs() <= 200,
            "{}",
            alignment.offset_ms
        );
    }
}

#[test]
fn a_stretched_subtitle_is_synced_with_its_frame_rate_ratio() {
    let cues = film(2);
    let stretched = mapped(&cues, |t| (t as f64 * 25.0 / 23.976).round() as i64);
    for reference in references(&cues) {
        let alignment = assert_synced(&reference, &stretched, &cues);
        assert!(
            (alignment.factor - 23.976 / 25.0).abs() < 1e-9,
            "{}",
            alignment.factor
        );
    }
}

#[test]
fn a_subtitle_with_a_scene_cut_is_synced_with_a_split() {
    // The seed of the most lines off against speech, of twelve tried.
    let cues = film(9);
    // The other release lacks 20 s in the middle: its lines there are
    // gone, and every line after is 20 s early.
    let middle = cues[cues.len() / 2].start_ms;
    let (kept, right): (Vec<Span>, Vec<Span>) = cues
        .iter()
        .filter(|c| c.start_ms < middle || c.start_ms >= middle + 20_000)
        .map(|c| {
            let early = if c.start_ms >= middle { 20_000 } else { 0 };
            (
                Span {
                    start_ms: c.start_ms - early,
                    end_ms: c.end_ms - early,
                },
                *c,
            )
        })
        .unzip();
    let [subtitle, speech] = references(&cues);
    let alignment = assert_synced(&subtitle, &kept, &right);
    assert!(alignment.splits >= 1, "{}", alignment.splits);
    // Against speech, a line or two next to the cut may be put on the
    // wrong side of it: the cut scene's own speech is in the film, and a
    // line moved there falls on speech as well as where it belongs. Every
    // line further than a scene from the cut is back in place.
    let (alignment, off) = misplaced(&speech, &kept, &right);
    assert!(alignment.splits >= 1, "{}", alignment.splits);
    assert!(
        off.len() * 50 <= kept.len(),
        "{} of {} off",
        off.len(),
        kept.len()
    );
    for cue in off {
        assert!(
            (cue.start_ms - middle).abs() <= 60_000,
            "{cue:?} is off, a minute from the cut at {middle}"
        );
    }
}

#[test]
fn a_subtitle_of_another_film_is_uncertain() {
    let other = film(99);
    for seed in [4, 5] {
        let cues = film(seed);
        for reference in references(&cues) {
            let alignment = align(&reference, &other).unwrap();
            assert!(
                !alignment.certain(),
                "seed {seed}: score {}",
                alignment.score
            );
        }
    }
}

#[test]
fn nothing_to_align_to_is_an_error() {
    assert_eq!(align(&[], &film(1)), Err(SyncError::NoReference));
}

#[test]
fn silence_has_no_speech() {
    let mut detector = SpeechDetector::new();
    detector.push(&vec![0; 16_000 * 5]);
    assert!(detector.finish().is_empty());
}

/// Samples split anywhere between feeds, a byte apart even, are read as
/// one stream: the same spans as one feed of it all.
#[test]
fn samples_cut_between_feeds_are_joined() {
    let mut rng = Rng(5);
    let bytes: Vec<u8> = (0..16_000 * 4)
        .flat_map(|i: i64| {
            // A tone that comes and goes, with a little noise.
            let on = (i / 8_000) % 2 == 0;
            let tone = if on {
                ((i as f64 * 0.07).sin() * 8_000.0) as i16
            } else {
                0
            };
            (tone + rng.range(-50, 50) as i16).to_le_bytes()
        })
        .collect();
    let mut whole = SpeechDetector::new();
    whole.push_bytes(&bytes);
    let mut pieces = SpeechDetector::new();
    for piece in bytes.chunks(777) {
        pieces.push_bytes(piece);
    }
    assert_eq!(pieces.finish(), whole.finish());
}
