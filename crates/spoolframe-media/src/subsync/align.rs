//! A subtitle's cues aligned to a reference (PX-03) with alass's
//! algorithm, as alass-cli runs it: the frame-rate ratio first, by the
//! best constant shift each candidate ratio gets, then the alignment with
//! splits at that ratio, which fixes a shift, a stretch and cut scenes at
//! once. alass aligns spans, not words, so the reference may be another
//! language's subtitle or the speech in the audio.

use alass_core::{NoProgressHandler, TimeDelta, TimePoint, TimeSpan};

use super::{Span, SyncError};

/// alass's split penalty: alass-cli's default, 7 in its 0 to 1000 scale.
/// Lower splits more readily.
const SPLIT_PENALTY: f64 = 7.0;

/// alass's speed optimization: alass-cli's default.
const SPEED_OPTIMIZATION: f64 = 1.0;

/// The frame rates a release is made at; a subtitle timed to one played
/// against another runs fast or slow by their ratio.
const FRAME_RATES: [f64; 5] = [23.976, 24.0, 25.0, 29.97, 30.0];

/// The score below which an alignment is not used (`Uncertain`). Measured
/// on generated films (this module's tests): a subtitle aligned to
/// another subtitle of its film scores about 0.95, to its film's speech
/// about 0.5 (speech is in pieces, and a tenth of the lines have none);
/// one aligned to another film's under 0.1. Not yet measured on real
/// films: the first to tune when one syncs wrongly or is held back.
pub const MIN_SCORE: f64 = 0.25;

/// What an alignment found.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// How each cue moves after `factor`, ms; one per cue.
    pub deltas: Vec<i64>,
    /// The frame-rate ratio: every time is multiplied by it first.
    pub factor: f64,
    /// The most common delta: the shift of most of the subtitle, ms.
    pub offset_ms: i64,
    /// Where the delta changes between one cue and the next: cuts.
    pub splits: u32,
    /// How well the aligned cues fall on the reference, 0 to 1: 0 is no
    /// better than chance, 1 every cue on it. Below [`MIN_SCORE`] the
    /// alignment is not to be used.
    pub score: f64,
}

impl Alignment {
    pub fn certain(&self) -> bool {
        self.score >= MIN_SCORE
    }
}

/// `subtitle`'s cues aligned to `reference`. alass runs in this thread,
/// for seconds on a film; a panic in it (a bug a strange subtitle found)
/// is an error, not the caller's panic.
pub fn align(reference: &[Span], subtitle: &[Span]) -> Result<Alignment, SyncError> {
    if reference.is_empty() {
        return Err(SyncError::NoReference);
    }
    if subtitle.is_empty() {
        return Err(SyncError::Unreadable("the subtitle has no cues".into()));
    }
    std::panic::catch_unwind(|| align_unchecked(reference, subtitle))
        .map_err(|_| SyncError::Failed("the aligner failed on this subtitle".into()))
}

fn spans(spans: &[Span], factor: f64) -> Vec<TimeSpan> {
    spans
        .iter()
        .map(|s| {
            let start = (s.start_ms as f64 * factor).round() as i64;
            let end = ((s.end_ms as f64 * factor).round() as i64).max(start);
            TimeSpan::new(TimePoint::from(start), TimePoint::from(end))
        })
        .collect()
}

fn align_unchecked(reference: &[Span], subtitle: &[Span]) -> Alignment {
    let reference_spans = spans(reference, 1.0);
    let mut ratios = vec![1.0];
    for a in FRAME_RATES {
        for b in FRAME_RATES {
            if a != b {
                ratios.push(a / b);
            }
        }
    }
    let (factor, _) = ratios
        .into_iter()
        .map(|ratio| {
            let (_, score) = alass_core::align_nosplit(
                &reference_spans,
                &spans(subtitle, ratio),
                alass_core::overlap_scoring,
                NoProgressHandler,
            );
            (ratio, score)
        })
        // The first best: 1.0 wins a tie.
        .fold(
            (1.0, f64::MIN),
            |best, this| {
                if this.1 > best.1 { this } else { best }
            },
        );
    let scaled = spans(subtitle, factor);
    let (deltas, _) = alass_core::align(
        &reference_spans,
        &scaled,
        SPLIT_PENALTY,
        Some(SPEED_OPTIMIZATION),
        alass_core::standard_scoring,
        NoProgressHandler,
    );
    let deltas: Vec<i64> = deltas.iter().map(TimeDelta::as_i64).collect();
    let splits = deltas.windows(2).filter(|w| w[0] != w[1]).count() as u32;
    let aligned: Vec<Span> = scaled
        .iter()
        .zip(&deltas)
        .map(|(s, d)| Span {
            start_ms: s.start().as_i64() + d,
            end_ms: s.end().as_i64() + d,
        })
        .collect();
    Alignment {
        offset_ms: most_common(&deltas),
        score: score(reference, &aligned),
        deltas,
        factor,
        splits,
    }
}

/// The delta most cues have.
fn most_common(deltas: &[i64]) -> i64 {
    let mut counts = std::collections::HashMap::new();
    for d in deltas {
        *counts.entry(*d).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(d, n)| (*n, -d.abs()))
        .map_or(0, |(d, _)| d)
}

/// How much better than chance `aligned` falls on `reference`, 0 to 1:
/// the share of the cues' time that is on the reference, against the
/// share of the time they span that the reference covers, which is what
/// cues put down anywhere would get.
pub(crate) fn score(reference: &[Span], aligned: &[Span]) -> f64 {
    let reference = union(reference);
    let aligned = union(aligned);
    let (Some(first), Some(last)) = (aligned.first(), aligned.last()) else {
        return 0.0;
    };
    let cue_time: i64 = aligned.iter().map(Span::len).sum();
    if cue_time == 0 {
        return 0.0;
    }
    let on = overlap(&reference, &aligned) as f64 / cue_time as f64;
    let window = Span {
        start_ms: first.start_ms,
        end_ms: last.end_ms,
    };
    let chance = overlap(&reference, &[window]) as f64 / window.len().max(1) as f64;
    if chance >= 1.0 {
        return 0.0;
    }
    ((on - chance) / (1.0 - chance)).clamp(0.0, 1.0)
}

/// `spans` sorted, with those that overlap or touch made one.
fn union(spans: &[Span]) -> Vec<Span> {
    let mut sorted: Vec<Span> = spans.iter().filter(|s| !s.is_empty()).copied().collect();
    sorted.sort_by_key(|s| s.start_ms);
    let mut out: Vec<Span> = Vec::with_capacity(sorted.len());
    for s in sorted {
        match out.last_mut() {
            Some(last) if s.start_ms <= last.end_ms => last.end_ms = last.end_ms.max(s.end_ms),
            _ => out.push(s),
        }
    }
    out
}

/// The time two sorted, non-overlapping lists have in common.
fn overlap(a: &[Span], b: &[Span]) -> i64 {
    let (mut i, mut j, mut total) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        let start = a[i].start_ms.max(b[j].start_ms);
        let end = a[i].end_ms.min(b[j].end_ms);
        total += (end - start).max(0);
        if a[i].end_ms < b[j].end_ms {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}
