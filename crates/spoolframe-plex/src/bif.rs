//! Plex's preview index of a part (P3-13): what it serves at
//! `/library/parts/{id}/indexes/sd`, for parts whose `indexes` says `sd`.
//! It is Roku's BIF ("Base Index Frames"); read on a real PMS (1.41):
//!
//! ```text
//! 0   8 bytes  magic 89 42 49 46 0D 0A 1A 0A
//! 8   u32 LE   version (0)
//! 12  u32 LE   number of frames
//! 16  u32 LE   timestamp multiplier in ms; 0 means 1000
//! 20  44 bytes reserved (zero)
//! 64  (frames + 1) × (u32 LE timestamp, u32 LE offset); the last entry's
//!     timestamp is 0xFFFFFFFF and its offset the end of the last image
//! …   the frames: JPEG, 320 px wide
//! ```
//!
//! That PMS writes one frame every 2 s (timestamps 0, 2, 4, … with the
//! multiplier 0): 3320 frames, 23.8 MB, for a 110-minute film.

use spoolframe_catalog::PreviewFrame;

const MAGIC: [u8; 8] = [0x89, b'B', b'I', b'F', 0x0D, 0x0A, 0x1A, 0x0A];
const HEADER: usize = 64;
const END: u32 = u32::MAX;

/// The frames of a BIF, in order of time.
pub fn frames(bif: &[u8]) -> Result<Vec<PreviewFrame>, String> {
    if bif.len() < HEADER || bif[..8] != MAGIC {
        return Err("not a BIF".into());
    }
    let word = |at: usize| -> Result<u32, String> {
        bif.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
            .ok_or_else(|| format!("cut short at byte {at}"))
    };
    let count = word(12)? as usize;
    let multiplier = match word(16)? {
        0 => 1000,
        ms => u64::from(ms),
    };
    // The index alone must fit, before anything is allocated for it.
    if HEADER + (count + 1) * 8 > bif.len() {
        return Err(format!("{count} frames do not fit in {} bytes", bif.len()));
    }
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let entry = HEADER + i * 8;
        let at = word(entry)?;
        if at == END {
            return Err(format!("frame {i} of {count} ends the index"));
        }
        let start = word(entry + 4)? as usize;
        let end = word(entry + 12)? as usize;
        let jpeg = bif
            .get(start..end)
            .ok_or_else(|| format!("frame {i} is outside the file ({start}..{end})"))?;
        frames.push(PreviewFrame {
            at_ms: u64::from(at) * multiplier,
            jpeg: jpeg.to_vec(),
        });
    }
    frames.sort_by_key(|f| f.at_ms);
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three frames of a real PMS's index (0 s, 2 s, 4 s), trimmed from
    /// the 3320 of its part.
    fn recorded() -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/index-sd-trimmed.bif",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn a_recorded_index_reads_as_jpegs_two_seconds_apart() {
        let frames = frames(&recorded()).unwrap();
        let times: Vec<_> = frames.iter().map(|f| f.at_ms).collect();
        assert_eq!(times, [0, 2000, 4000]);
        for frame in &frames {
            assert_eq!(frame.jpeg[..3], [0xFF, 0xD8, 0xFF], "a JPEG");
        }
        assert_eq!(frames[2].jpeg.len(), 6433);
    }

    #[test]
    fn a_multiplier_scales_the_timestamps() {
        let mut bif = recorded();
        bif[16..20].copy_from_slice(&500u32.to_le_bytes());
        let times: Vec<_> = frames(&bif).unwrap().iter().map(|f| f.at_ms).collect();
        assert_eq!(times, [0, 1000, 2000]);
    }

    #[test]
    fn what_is_not_a_whole_bif_is_refused() {
        let bif = recorded();
        assert!(frames(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(frames(&bif[..HEADER + 8]).is_err(), "index cut short");
        assert!(
            frames(&bif[..bif.len() - 1]).is_err(),
            "last frame cut short"
        );
        let mut many = bif.clone();
        many[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(frames(&many).is_err());
    }
}
