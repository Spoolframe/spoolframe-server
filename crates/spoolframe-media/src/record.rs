//! Whether a video stream's codec record holds its parameter sets.
//!
//! A muxer that writes its header before the first packet (ffmpeg's
//! segment muxer with one header file, for HLS of a copied video) writes
//! the record as the file has it. Some files keep their parameter sets only
//! in the stream, before each keyframe: an `hvcC` with no arrays (4K
//! WEB-DLs), an `avcC` with no SPS or PPS, or an MP4 track tagged `avc3` or
//! `hev1`, whose sets may change in the stream. A header written early from
//! such a record has none, and AVPlayer refuses the stream. Those are cut
//! as before, by the HLS muxer, which writes its header after the first
//! packets.

/// Whether `codec`'s record `extradata` (as ffprobe gives it, the MP4 or
/// Matroska record or Annex B) holds every parameter set a decoder starts
/// with: H.264's SPS and PPS, HEVC's VPS, SPS and PPS. `tag` is the
/// stream's fourcc as written (`avc1`, `hev1`; zeros in Matroska): `avc3`
/// and `hev1` say the sets are in the stream. False for any other codec.
pub(crate) fn holds_parameter_sets(codec: &str, tag: &[u8; 4], extradata: &[u8]) -> bool {
    if matches!(tag, b"avc3" | b"hev1") {
        return false;
    }
    let found = match codec {
        "h264" => nal_types(extradata, Codec::H264),
        "hevc" => nal_types(extradata, Codec::Hevc),
        _ => return false,
    };
    let Some(found) = found else {
        return false;
    };
    let needed: &[u8] = match codec {
        "h264" => &[7, 8],
        _ => &[32, 33, 34],
    };
    needed.iter().all(|t| found.contains(t))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    Hevc,
}

/// The NAL unit types a record holds, each with at least one non-empty
/// unit; `None` for a record that cannot be read.
fn nal_types(record: &[u8], codec: Codec) -> Option<Vec<u8>> {
    match record {
        [0, 0, 1, ..] | [0, 0, 0, 1, ..] => Some(annex_b_types(record, codec)),
        [1, ..] => match codec {
            Codec::H264 => avcc_types(record),
            Codec::Hevc => hvcc_types(record),
        },
        _ => None,
    }
}

fn nal_type(first: u8, codec: Codec) -> u8 {
    match codec {
        Codec::H264 => first & 0x1f,
        Codec::Hevc => (first >> 1) & 0x3f,
    }
}

/// The types of the units after each start code.
fn annex_b_types(record: &[u8], codec: Codec) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < record.len() {
        if record[i..i + 3] == [0, 0, 1] {
            types.push(nal_type(record[i + 3], codec));
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

/// A big-endian `u16` at `at`.
fn u16_at(record: &[u8], at: usize) -> Option<usize> {
    let bytes = record.get(at..at + 2)?;
    Some(usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
}

/// `count` length-prefixed units from `at`: whether any is non-empty, and
/// where the next byte is.
fn units(record: &[u8], mut at: usize, count: usize) -> Option<(bool, usize)> {
    let mut any = false;
    for _ in 0..count {
        let len = u16_at(record, at)?;
        record.get(at + 2..at + 2 + len)?;
        any |= len > 0;
        at += 2 + len;
    }
    Some((any, at))
}

/// ISO 14496-15 `AVCDecoderConfigurationRecord`: five bytes, the SPS
/// count (low five bits), the SPSs, the PPS count, the PPSs.
fn avcc_types(record: &[u8]) -> Option<Vec<u8>> {
    let mut types = Vec::new();
    let sps = usize::from(*record.get(5)? & 0x1f);
    let (any, at) = units(record, 6, sps)?;
    if any {
        types.push(7);
    }
    let pps = usize::from(*record.get(at)?);
    let (any, _) = units(record, at + 1, pps)?;
    if any {
        types.push(8);
    }
    Some(types)
}

/// ISO 14496-15 `HEVCDecoderConfigurationRecord`: 22 bytes, the array
/// count, then each array's type (low six bits), unit count and units.
fn hvcc_types(record: &[u8]) -> Option<Vec<u8>> {
    let mut types = Vec::new();
    let arrays = *record.get(22)?;
    let mut at = 23;
    for _ in 0..arrays {
        let kind = *record.get(at)? & 0x3f;
        let count = u16_at(record, at + 1)?;
        let (any, next) = units(record, at + 3, count)?;
        if any {
            types.push(kind);
        }
        at = next;
    }
    Some(types)
}

/// The bytes of ffprobe's `-show_data` hex dump: lines of an offset, up to
/// eight groups of four hex digits, and the bytes as text after two
/// spaces. `None` for anything else.
pub(crate) fn hex_dump_bytes(dump: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for line in dump.lines().filter(|l| !l.trim().is_empty()) {
        let (_, rest) = line.split_once(": ")?;
        let hex = rest.split("  ").next()?;
        for group in hex.split_whitespace() {
            if group.len() % 2 != 0 {
                return None;
            }
            for pair in group.as_bytes().chunks(2) {
                let pair = std::str::from_utf8(pair).ok()?;
                bytes.push(u8::from_str_radix(pair, 16).ok()?);
            }
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZEROS: &[u8; 4] = &[0; 4];

    /// x264's `avcC` for a 360p stream: one SPS, one PPS.
    const AVCC: [u8; 37] = [
        0x01, 0x42, 0xc0, 0x0c, 0xff, 0xe1, 0x00, 0x16, 0x67, 0x42, 0xc0, 0x0c, 0xda, 0x05, 0x06,
        0x7e, 0x7c, 0x04, 0x40, 0x00, 0x00, 0xfa, 0x40, 0x00, 0x2e, 0xe0, 0x03, 0xc5, 0x0a, 0xa8,
        0x01, 0x00, 0x04, 0x68, 0xce, 0x0f, 0xc8,
    ];

    /// An `hvcC` of its 22-byte head and `arrays`: (type, units).
    fn hvcc(arrays: &[(u8, &[&[u8]])]) -> Vec<u8> {
        let mut record = vec![1];
        record.extend([0; 21]);
        record.push(arrays.len() as u8);
        for (kind, units) in arrays {
            record.push(0x80 | kind);
            record.extend((units.len() as u16).to_be_bytes());
            for unit in *units {
                record.extend((unit.len() as u16).to_be_bytes());
                record.extend(*unit);
            }
        }
        record
    }

    #[test]
    fn an_avcc_with_its_sps_and_pps_holds_them() {
        assert!(holds_parameter_sets("h264", ZEROS, &AVCC));
    }

    #[test]
    fn an_avcc_without_sps_or_pps_does_not() {
        // No SPS, no PPS: five bytes, then counts of 0.
        let empty = [0x01, 0x42, 0xc0, 0x0c, 0xff, 0xe0, 0x00];
        assert!(!holds_parameter_sets("h264", ZEROS, &empty));
        // The SPS alone.
        let mut no_pps = AVCC[..30].to_vec();
        no_pps.push(0);
        assert!(!holds_parameter_sets("h264", ZEROS, &no_pps));
    }

    #[test]
    fn an_hvcc_with_vps_sps_and_pps_holds_them() {
        let record = hvcc(&[
            (32, &[&[0x40, 1]]),
            (33, &[&[0x42, 1]]),
            (34, &[&[0x44, 1]]),
        ]);
        assert!(holds_parameter_sets("hevc", ZEROS, &record));
    }

    /// Some 4K WEB-DLs: an `hvcC` with no arrays, the sets in the stream.
    #[test]
    fn an_hvcc_with_no_arrays_does_not() {
        assert!(!holds_parameter_sets("hevc", ZEROS, &hvcc(&[])));
        // Arrays named, with no unit in them.
        let empty = hvcc(&[(32, &[]), (33, &[]), (34, &[])]);
        assert!(!holds_parameter_sets("hevc", ZEROS, &empty));
        // No PPS.
        let no_pps = hvcc(&[(32, &[&[0x40, 1]]), (33, &[&[0x42, 1]])]);
        assert!(!holds_parameter_sets("hevc", ZEROS, &no_pps));
    }

    #[test]
    fn a_record_cut_short_does_not() {
        let record = hvcc(&[
            (32, &[&[0x40, 1]]),
            (33, &[&[0x42, 1]]),
            (34, &[&[0x44, 1]]),
        ]);
        assert!(!holds_parameter_sets(
            "hevc",
            ZEROS,
            &record[..record.len() - 1]
        ));
        assert!(!holds_parameter_sets("h264", ZEROS, &AVCC[..20]));
        assert!(!holds_parameter_sets("h264", ZEROS, &[]));
    }

    /// An MPEG-TS stream's record: Annex B, as ffmpeg reads it from the
    /// stream.
    #[test]
    fn annex_b_with_every_set_holds_them() {
        let hevc = [
            0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 1, 0x42, 1, 0xbb, 0, 0, 0, 1, 0x44, 1, 0xcc,
        ];
        assert!(holds_parameter_sets("hevc", ZEROS, &hevc));
        assert!(!holds_parameter_sets("hevc", ZEROS, &hevc[..14]));
        let h264 = [0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb];
        assert!(holds_parameter_sets("h264", ZEROS, &h264));
    }

    /// `avc3` and `hev1` tracks may change their sets in the stream.
    #[test]
    fn an_in_band_tag_does_not_whatever_the_record() {
        assert!(!holds_parameter_sets("h264", b"avc3", &AVCC));
        let record = hvcc(&[
            (32, &[&[0x40, 1]]),
            (33, &[&[0x42, 1]]),
            (34, &[&[0x44, 1]]),
        ]);
        assert!(!holds_parameter_sets("hevc", b"hev1", &record));
        assert!(holds_parameter_sets("hevc", b"hvc1", &record));
    }

    #[test]
    fn other_codecs_do_not() {
        assert!(!holds_parameter_sets("av1", ZEROS, &AVCC));
    }

    #[test]
    fn ffprobe_s_hex_dump_is_read_back_to_its_bytes() {
        let dump = "\n00000000: 0142 c00c ffe1 0016 6742 c00c da05 067e  .B......gB.....~\n\
                    00000010: 7c04 4000 00fa 4000 2ee0 03c5 0aa8 0100  |.@...@.........\n\
                    00000020: 0468 ce0f c8                             .h...\n";
        assert_eq!(hex_dump_bytes(dump).as_deref(), Some(&AVCC[..]));
        // The text after the hex may hold two spaces of its own.
        let spaced = "00000000: 4142 2020                                AB  \n";
        assert_eq!(hex_dump_bytes(spaced), Some(vec![0x41, 0x42, 0x20, 0x20]));
        assert_eq!(hex_dump_bytes("not a dump"), None);
        assert_eq!(hex_dump_bytes("00000000: 0g"), None);
    }
}
