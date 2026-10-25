//! A video stream's RFC 6381 codec string from its codec record: the
//! profile, tier and level a client's `video_codecs` are judged against.
//!
//! The record is what ffprobe dumps as `extradata`: an `avcC`, `hvcC` or
//! `av1C` from MP4 or Matroska, or Annex B parameter sets from MPEG-TS,
//! whose SPS starts with the same fields. The string is the one a copy
//! is written as (`avc1`, `hvc1`: `encode` tags an HEVC copy `hvc1`).

use spoolframe_types::{CodecString, Tier};

/// The string of `codec`'s stream (ffprobe's name) with record
/// `extradata`; `None` for a codec without one here, or a record that
/// cannot be read.
pub(crate) fn of_record(codec: &str, extradata: &[u8]) -> Option<CodecString> {
    match codec {
        "h264" => avc(extradata),
        "hevc" => hevc(extradata),
        "av1" => av1(extradata),
        _ => None,
    }
}

/// `avcC`'s bytes 1 to 3, or the SPS's after its NAL header: the profile,
/// the constraint flags and the level.
fn avc(record: &[u8]) -> Option<CodecString> {
    let fields = match record {
        [1, fields @ ..] => fields.get(..3)?.to_vec(),
        _ => unescaped(
            annex_b_unit(record, |first| first & 0x1f == 7)?.get(1..)?,
            3,
        ),
    };
    let [profile, constraints, level] = fields[..] else {
        return None;
    };
    Some(CodecString::Avc {
        in_band: false,
        profile,
        constraints,
        level,
    })
}

/// `general_profile_tier_level`'s 12 bytes: in an `hvcC` from byte 1, in
/// an SPS after its two-byte NAL header and one byte of ids.
fn hevc(record: &[u8]) -> Option<CodecString> {
    let ptl = match record {
        [1, fields @ ..] => fields.get(..12)?.to_vec(),
        _ => unescaped(
            annex_b_unit(record, |first| (first >> 1) & 0x3f == 33)?.get(3..)?,
            12,
        ),
    };
    if ptl.len() != 12 {
        return None;
    }
    let mut constraints = [0; 6];
    constraints.copy_from_slice(&ptl[5..11]);
    Some(CodecString::Hevc {
        in_band: false,
        profile_space: ptl[0] >> 6,
        profile: ptl[0] & 0x1f,
        // Flag j is the j-th bit read; the string writes them the other
        // way round, flag 0 lowest.
        compatibility: u32::from_be_bytes([ptl[1], ptl[2], ptl[3], ptl[4]]).reverse_bits(),
        tier: if ptl[0] & 0x20 != 0 {
            Tier::High
        } else {
            Tier::Main
        },
        level: ptl[11],
        constraints,
    })
}

/// `av1C`: a marker and version byte (0x81), then `seq_profile` (3 bits)
/// and `seq_level_idx_0` (5), then `seq_tier_0`, `high_bitdepth` and
/// `twelve_bit`.
fn av1(record: &[u8]) -> Option<CodecString> {
    let [0x81, profile_level, flags, ..] = *record else {
        return None;
    };
    let profile = profile_level >> 5;
    let high_bitdepth = flags & 0x40 != 0;
    let twelve_bit = flags & 0x20 != 0;
    Some(CodecString::Av1 {
        profile,
        level: profile_level & 0x1f,
        tier: if flags & 0x80 != 0 {
            Tier::High
        } else {
            Tier::Main
        },
        bit_depth: match (high_bitdepth, profile == 2 && twelve_bit) {
            (true, true) => 12,
            (true, false) => 10,
            (false, _) => 8,
        },
    })
}

/// The first unit after a start code whose first byte `wanted` takes, to
/// the next start code.
fn annex_b_unit(record: &[u8], wanted: impl Fn(u8) -> bool) -> Option<&[u8]> {
    let starts: Vec<usize> = record
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(at, _)| at + 3)
        .collect();
    starts.iter().enumerate().find_map(|(i, &start)| {
        let first = *record.get(start)?;
        if !wanted(first) {
            return None;
        }
        // A four-byte start code's leading zero belongs to the next one.
        let end = starts.get(i + 1).map_or(record.len(), |&next| next - 3);
        record.get(start..end)
    })
}

/// The first `most` bytes of `unit` with its emulation prevention bytes
/// (the 3 of each `00 00 03`) taken out.
fn unescaped(unit: &[u8], most: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(most);
    let mut zeros = 0;
    for &byte in unit {
        if bytes.len() == most {
            break;
        }
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        bytes.push(byte);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(codec: &str, record: &[u8]) -> Option<String> {
        of_record(codec, record).map(|s| s.to_string())
    }

    /// x265's `hvcC` for Main 10, high tier, level 5.1: its head, as
    /// ffmpeg writes it into Matroska.
    const HVCC_HEAD: [u8; 23] = [
        0x01, 0x22, 0x20, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x99, 0xf0, 0x00,
        0xfc, 0xfd, 0xfa, 0xfa, 0x00, 0x00, 0x0f, 0x04,
    ];

    #[test]
    fn an_hvcc_says_its_profile_tier_and_level() {
        assert_eq!(
            string("hevc", &HVCC_HEAD).as_deref(),
            Some("hvc1.2.4.H153.90")
        );
        let mut main = HVCC_HEAD;
        // Main, main tier, compatible with Main and Main 10; level 4.
        main[1] = 0x01;
        main[2] = 0x60;
        main[6] = 0xb0;
        main[12] = 120;
        assert_eq!(string("hevc", &main).as_deref(), Some("hvc1.1.6.L120.B0"));
        assert_eq!(string("hevc", &HVCC_HEAD[..12]), None, "cut short");
    }

    /// The same stream in MPEG-TS: its SPS, with emulation prevention
    /// bytes in its profile, tier and level.
    #[test]
    fn an_annex_b_sps_says_the_same() {
        let annex_b = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x22, 0x20, 0x00, 0x00,
            0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0x95, 0x98, 0x09,
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x22, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xa0, 0x14, 0x20,
        ];
        assert_eq!(
            string("hevc", &annex_b).as_deref(),
            Some("hvc1.2.4.H153.90")
        );
        // The VPS alone: no SPS to read.
        assert_eq!(string("hevc", &annex_b[..28]), None);
    }

    #[test]
    fn an_avcc_and_an_h264_sps_say_profile_constraints_and_level() {
        let avcc = [0x01, 0x4d, 0x40, 0x1f, 0xff, 0xe1, 0x00, 0x17, 0x67, 0x4d];
        assert_eq!(string("h264", &avcc).as_deref(), Some("avc1.4D401F"));
        let sps = [
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xac, 0, 0, 1, 0x68, 0xee,
        ];
        assert_eq!(string("h264", &sps).as_deref(), Some("avc1.640033"));
        assert_eq!(string("h264", &[0x01, 0x4d]), None);
    }

    #[test]
    fn an_av1c_says_profile_level_tier_and_depth() {
        // Main, level 5.1 (13), main tier, 10-bit.
        assert_eq!(
            string("av1", &[0x81, 0x0d, 0x4c, 0x00]).as_deref(),
            Some("av01.0.13M.10")
        );
        // Main, level 4.0, 8-bit; high tier.
        assert_eq!(
            string("av1", &[0x81, 0x08, 0x8c, 0x00]).as_deref(),
            Some("av01.0.08H.08")
        );
        // Professional, 12-bit.
        assert_eq!(
            string("av1", &[0x81, 0x4d, 0x60, 0x00]).as_deref(),
            Some("av01.2.13M.12")
        );
        assert_eq!(string("av1", &[0x0a, 0x0b]), None, "an OBU, not av1C");
    }

    #[test]
    fn another_codec_has_none() {
        assert_eq!(string("vp9", &HVCC_HEAD), None);
    }
}
