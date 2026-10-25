//! RFC 6381 codec strings (`hvc1.2.4.L153.B0`, `dvh1.08.06`): a video
//! stream's codec with its profile, tier and level, as a player asks
//! about it (Apple: `AVURLAsset.isPlayableExtendedMIMEType`).
//!
//! A client lists the strings it plays
//! ([`ClientCapabilities::video_codecs`](crate::ClientCapabilities::video_codecs)),
//! and the server judges a source's own string against them: a source is
//! played when a listed string [covers](CodecString::covers) it. The forms
//! read are those of ISO/IEC 14496-15 Annex E (`avc1`, `hvc1`), the AV1
//! codec ISOBMFF binding (`av01`) and Dolby's (`dvh1`, `dva1`, `dav1`).

use std::fmt;
use std::str::FromStr;

use crate::VideoCodec;

/// HEVC's and AV1's tier: a high-tier stream may use more bits at its
/// level than a main-tier decoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Main,
    High,
}

/// One video codec string, read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecString {
    /// `avc1.PPCCLL` (`avc3` with `in_band`): `profile_idc`, the
    /// constraint flags byte and `level_idc`, in hex.
    Avc {
        in_band: bool,
        profile: u8,
        constraints: u8,
        level: u8,
    },
    /// `hvc1.[A-C]P.F.TL.CC…` (`hev1` with `in_band`): profile space and
    /// `general_profile_idc`; the 32 compatibility flags as the string
    /// writes them (bit-reversed, in hex); tier and `general_level_idc`
    /// (30 times the level: 153 is 5.1); then up to six constraint bytes,
    /// those written in hex, the trailing zero ones left out.
    Hevc {
        in_band: bool,
        profile_space: u8,
        profile: u8,
        compatibility: u32,
        tier: Tier,
        level: u8,
        constraints: [u8; 6],
    },
    /// `av01.P.LLT.DD`: `seq_profile`, `seq_level_idx_0`, `seq_tier_0` and
    /// the bit depth. The optional fields after it are read past and not
    /// kept.
    Av1 {
        profile: u8,
        level: u8,
        tier: Tier,
        bit_depth: u8,
    },
    /// `dvh1.PP.LL` (`dvhe` with `in_band`) over HEVC, `dva1`/`dvav` over
    /// H.264, `dav1` over AV1: Dolby Vision's profile and level.
    DolbyVision {
        base: VideoCodec,
        in_band: bool,
        profile: u8,
        level: u8,
    },
}

/// Why a string is not a codec string this module reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a video codec string: {0}")]
pub struct CodecStringError(String);

impl CodecString {
    /// Whether a decoder that plays `self` plays `source`: the same codec
    /// (the sample entry aside: `hvc1` and `hev1` are one decoder), a
    /// profile it decodes, and a tier and level at most its own; for AV1
    /// the same bit depth. A profile decodes itself, and a few decode
    /// others by their standard: HEVC Main 10 decodes Main; H.264's High
    /// profiles decode Main, the ones under them and any stream whose
    /// constraint flags say it is Main. Compatibility and constraint flags
    /// are otherwise not compared.
    pub fn covers(&self, source: &CodecString) -> bool {
        use CodecString::*;
        match (*self, *source) {
            (
                Avc { profile, level, .. },
                Avc {
                    profile: theirs,
                    constraints,
                    level: their_level,
                    ..
                },
            ) => avc_decodes(profile, theirs, constraints) && their_level <= level,
            (
                Hevc {
                    profile_space,
                    profile,
                    tier,
                    level,
                    ..
                },
                Hevc {
                    profile_space: their_space,
                    profile: theirs,
                    tier: their_tier,
                    level: their_level,
                    ..
                },
            ) => {
                profile_space == their_space
                    && (profile == theirs || (profile == HEVC_MAIN_10 && theirs == HEVC_MAIN))
                    && their_tier <= tier
                    && their_level <= level
            }
            (
                Av1 {
                    profile,
                    level,
                    tier,
                    bit_depth,
                },
                Av1 {
                    profile: theirs,
                    level: their_level,
                    tier: their_tier,
                    bit_depth: their_depth,
                },
            ) => {
                profile == theirs
                    && bit_depth == their_depth
                    && their_tier <= tier
                    && their_level <= level
            }
            (
                DolbyVision {
                    base,
                    profile,
                    level,
                    ..
                },
                DolbyVision {
                    base: their_base,
                    profile: theirs,
                    level: their_level,
                    ..
                },
            ) => base == their_base && profile == theirs && their_level <= level,
            _ => false,
        }
    }

    /// The codec it is a string of; Dolby Vision's is its own base layer's.
    pub fn codec(&self) -> VideoCodec {
        match self {
            Self::Avc { .. } => VideoCodec::H264,
            Self::Hevc { .. } => VideoCodec::Hevc,
            Self::Av1 { .. } => VideoCodec::Av1,
            Self::DolbyVision { base, .. } => *base,
        }
    }
}

const HEVC_MAIN: u8 = 1;
const HEVC_MAIN_10: u8 = 2;

/// H.264's Main profile, by `profile_idc`.
const AVC_MAIN: u8 = 77;
/// High, High 10, High 4:2:2 and High 4:4:4 Predictive: each decodes the
/// ones before it (H.264 A.2.4 to A.2.7).
const AVC_HIGH: [u8; 4] = [100, 110, 122, 244];
/// `constraint_set1_flag`: the stream keeps to Main's constraints, as
/// constrained baseline does.
const AVC_MAIN_CONSTRAINED: u8 = 0x40;

/// Whether an H.264 decoder of `profile` decodes a stream of `source`
/// with `constraints`.
fn avc_decodes(profile: u8, source: u8, constraints: u8) -> bool {
    let high = |p: u8| AVC_HIGH.iter().position(|&h| h == p);
    if profile == source {
        return true;
    }
    let main_or_above = profile == AVC_MAIN || high(profile).is_some();
    if main_or_above && constraints & AVC_MAIN_CONSTRAINED != 0 {
        return true;
    }
    match (high(profile), high(source)) {
        (Some(ours), Some(theirs)) => theirs <= ours,
        (Some(_), None) => source == AVC_MAIN,
        _ => false,
    }
}

impl FromStr for CodecString {
    type Err = CodecStringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || CodecStringError(s.to_owned());
        let mut parts = s.split('.');
        let fourcc = parts.next().ok_or_else(bad)?;
        let parts: Vec<&str> = parts.collect();
        let parsed = match fourcc {
            "avc1" | "avc3" => avc(fourcc == "avc3", &parts),
            "hvc1" | "hev1" => hevc(fourcc == "hev1", &parts),
            "av01" => av1(&parts),
            "dvh1" | "dvhe" => dolby_vision(VideoCodec::Hevc, fourcc == "dvhe", &parts),
            "dva1" | "dvav" => dolby_vision(VideoCodec::H264, fourcc == "dvav", &parts),
            "dav1" => dolby_vision(VideoCodec::Av1, false, &parts),
            _ => None,
        };
        parsed.ok_or_else(bad)
    }
}

/// `PPCCLL`, six hex digits.
fn avc(in_band: bool, parts: &[&str]) -> Option<CodecString> {
    let [hex] = parts else { return None };
    if hex.len() != 6 {
        return None;
    }
    let byte = |at: usize| hex_byte(hex.get(at..at + 2)?);
    Some(CodecString::Avc {
        in_band,
        profile: byte(0)?,
        constraints: byte(2)?,
        level: byte(4)?,
    })
}

fn hevc(in_band: bool, parts: &[&str]) -> Option<CodecString> {
    let [profile, compatibility, tier_level, constraints @ ..] = parts else {
        return None;
    };
    if constraints.len() > 6 {
        return None;
    }
    let (profile_space, profile) = match profile.as_bytes().first()? {
        b'A' => (1, &profile[1..]),
        b'B' => (2, &profile[1..]),
        b'C' => (3, &profile[1..]),
        _ => (0, *profile),
    };
    let tier = match tier_level.as_bytes().first()? {
        b'L' => Tier::Main,
        b'H' => Tier::High,
        _ => return None,
    };
    let mut bytes = [0; 6];
    for (byte, hex) in bytes.iter_mut().zip(constraints) {
        *byte = hex_byte(hex)?;
    }
    if compatibility.is_empty() || compatibility.len() > 8 {
        return None;
    }
    Some(CodecString::Hevc {
        in_band,
        profile_space,
        profile: decimal(profile)?,
        compatibility: u32::from_str_radix(compatibility, 16).ok()?,
        tier,
        level: decimal(&tier_level[1..])?,
        constraints: bytes,
    })
}

/// `P.LLT.DD`, then up to six optional fields, read past.
fn av1(parts: &[&str]) -> Option<CodecString> {
    let [profile, level_tier, depth, rest @ ..] = parts else {
        return None;
    };
    if rest.len() > 6 || profile.len() != 1 || level_tier.len() != 3 || depth.len() != 2 {
        return None;
    }
    let tier = match level_tier.as_bytes()[2] {
        b'M' => Tier::Main,
        b'H' => Tier::High,
        _ => return None,
    };
    Some(CodecString::Av1 {
        profile: decimal(profile)?,
        level: decimal(&level_tier[..2])?,
        tier,
        bit_depth: decimal(depth)?,
    })
}

/// `PP.LL`, two decimal digits each.
fn dolby_vision(base: VideoCodec, in_band: bool, parts: &[&str]) -> Option<CodecString> {
    let [profile, level] = parts else { return None };
    if profile.len() != 2 || level.len() != 2 {
        return None;
    }
    Some(CodecString::DolbyVision {
        base,
        in_band,
        profile: decimal(profile)?,
        level: decimal(level)?,
    })
}

/// Decimal digits only (no sign, no space), as a byte.
fn decimal(digits: &str) -> Option<u8> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// One or two hex digits.
fn hex_byte(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

impl fmt::Display for CodecString {
    /// The string's own form, hex in upper case, as Apple writes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tier = |tier: Tier| match tier {
            Tier::Main => 'L',
            Tier::High => 'H',
        };
        match *self {
            Self::Avc {
                in_band,
                profile,
                constraints,
                level,
            } => write!(
                f,
                "{}.{profile:02X}{constraints:02X}{level:02X}",
                if in_band { "avc3" } else { "avc1" }
            ),
            Self::Hevc {
                in_band,
                profile_space,
                profile,
                compatibility,
                tier: t,
                level,
                constraints,
            } => {
                let space = match profile_space {
                    1 => "A",
                    2 => "B",
                    3 => "C",
                    _ => "",
                };
                write!(
                    f,
                    "{}.{space}{profile}.{compatibility:X}.{}{level}",
                    if in_band { "hev1" } else { "hvc1" },
                    tier(t)
                )?;
                let kept = constraints
                    .iter()
                    .rposition(|&b| b != 0)
                    .map_or(0, |i| i + 1);
                for byte in &constraints[..kept] {
                    write!(f, ".{byte:X}")?;
                }
                Ok(())
            }
            Self::Av1 {
                profile,
                level,
                tier: t,
                bit_depth,
            } => {
                let t = match t {
                    Tier::Main => 'M',
                    Tier::High => 'H',
                };
                write!(f, "av01.{profile}.{level:02}{t}.{bit_depth:02}")
            }
            Self::DolbyVision {
                base,
                in_band,
                profile,
                level,
            } => {
                let fourcc = match (base, in_band) {
                    (VideoCodec::H264, false) => "dva1",
                    (VideoCodec::H264, true) => "dvav",
                    (VideoCodec::Av1, _) => "dav1",
                    (_, false) => "dvh1",
                    (_, true) => "dvhe",
                };
                write!(f, "{fourcc}.{profile:02}.{level:02}")
            }
        }
    }
}

/// On the wire as its string.
impl serde::Serialize for CodecString {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for CodecString {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(s: &str) -> CodecString {
        s.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    /// What a first-generation Apple TV 4K (A10X, tvOS 26.6) answered
    /// `isPlayableExtendedMIMEType` for each string: its yes answers are
    /// its list, and every string is judged against that list as the
    /// device judged it.
    const APPLE_TV_4K_A10X: [(&str, bool); 9] = [
        ("hvc1.2.4.L153.B0", true),
        ("hvc1.2.4.L186.B0", true),
        ("hvc1.2.4.L999.B0", false),
        ("dvh1.05.06", true),
        ("dvh1.08.06", true),
        ("dvh1.07.06", false),
        ("dvhe.07.06", false),
        ("av01.0.13M.10", true),
        // Not one it was asked about: past its highest HEVC level.
        ("hvc1.2.4.L189.B0", false),
    ];

    fn played(list: &[CodecString], s: &str) -> bool {
        s.parse::<CodecString>()
            .is_ok_and(|source| list.iter().any(|listed| listed.covers(&source)))
    }

    #[test]
    fn the_apple_tv_s_answers_are_its_list_s() {
        let list: Vec<CodecString> = APPLE_TV_4K_A10X
            .iter()
            .filter(|(_, yes)| *yes)
            .map(|(s, _)| read(s))
            .collect();
        for (s, yes) in APPLE_TV_4K_A10X {
            assert_eq!(played(&list, s), yes, "{s}");
        }
    }

    /// Level 99.9 is no HEVC level (`general_level_idc` is a byte): not a
    /// string, so neither a source nor a listed one.
    #[test]
    fn hevc_level_999_is_no_string() {
        assert!("hvc1.2.4.L999.B0".parse::<CodecString>().is_err());
    }

    #[test]
    fn a_list_at_level_5_1_does_not_play_6_2_and_one_at_6_2_does() {
        let at_153 = [read("hvc1.2.4.L153.B0")];
        assert!(!played(&at_153, "hvc1.2.4.L186.B0"));
        assert!(played(&at_153, "hvc1.2.4.L150.B0"));
        let at_186 = [read("hvc1.2.4.L186.B0")];
        assert!(played(&at_186, "hvc1.2.4.L153.B0"));
    }

    /// Dolby Vision 7, a UHD Blu-ray's, is not 8 or 5: no list of those
    /// plays it, whichever sample entry names it.
    #[test]
    fn dolby_vision_7_is_its_own_profile() {
        let list = [read("dvh1.05.06"), read("dvh1.08.06")];
        assert!(!played(&list, "dvh1.07.06"));
        assert!(!played(&list, "dvhe.07.06"));
        assert!(played(&list, "dvhe.08.06"), "dvhe is dvh1's decoder");
        assert!(played(&list, "dvh1.08.05"));
        assert!(!played(&list, "dvh1.08.09"));
        // Dolby Vision over AV1 is not over HEVC.
        assert!(!played(&list, "dav1.10.06"));
    }

    #[test]
    fn a_high_tier_stream_needs_a_high_tier_decoder() {
        let main = [read("hvc1.2.4.L153.B0")];
        assert!(!played(&main, "hvc1.2.4.H153.B0"));
        let high = [read("hvc1.2.4.H153.B0")];
        assert!(played(&high, "hvc1.2.4.L153.B0"));
        assert!(played(&high, "hvc1.2.4.H150.90"));
    }

    /// HEVC Main 10 decoders decode Main; not the other way round.
    #[test]
    fn hevc_main_10_plays_main() {
        assert!(played(&[read("hvc1.2.4.L153.B0")], "hvc1.1.6.L120.90"));
        assert!(!played(&[read("hvc1.1.6.L153.B0")], "hvc1.2.4.L120.B0"));
        // Range extensions are their own profile.
        assert!(!played(&[read("hvc1.2.4.L153.B0")], "hvc1.4.10.L153.B0"));
        // As is another profile space.
        assert!(!played(&[read("hvc1.2.4.L153.B0")], "hvc1.A2.4.L153.B0"));
        // `hev1` is `hvc1`'s decoder.
        assert!(played(&[read("hev1.2.4.L153.B0")], "hvc1.2.4.L153.B0"));
    }

    /// H.264 High up to 5.2 plays Main and constrained baseline under it,
    /// not plain Baseline nor High 10.
    #[test]
    fn h264_high_plays_main_and_constrained_baseline() {
        let high = [read("avc1.640034")];
        assert!(played(&high, "avc1.640028"));
        assert!(played(&high, "avc1.4D401F"), "Main 3.1");
        assert!(played(&high, "avc1.42E01E"), "constrained baseline 3.0");
        assert!(!played(&high, "avc1.42001E"), "baseline 3.0");
        assert!(!played(&high, "avc1.6E0028"), "High 10 4.0");
        assert!(!played(&high, "avc1.640035"), "High 5.3");
        assert!(played(&[read("avc1.6E0033")], "avc1.640028"));
        assert!(played(&[read("avc1.42001E")], "avc1.42001E"));
    }

    #[test]
    fn av1_is_judged_at_its_own_bit_depth() {
        let list = [read("av01.0.13M.10")];
        assert!(played(&list, "av01.0.12M.10"));
        assert!(!played(&list, "av01.0.12M.08"), "8-bit is listed apart");
        assert!(!played(&list, "av01.0.16M.10"));
        assert!(!played(&list, "av01.0.13H.10"));
        assert!(!played(&list, "av01.1.13M.10"), "High profile");
        // The optional fields are read past.
        assert!(played(&list, "av01.0.08M.10.0.110.09.16.09.0"));
    }

    #[test]
    fn no_codec_covers_another() {
        assert!(!read("hvc1.2.4.L186.B0").covers(&read("av01.0.08M.10")));
        assert!(!read("dvh1.08.06").covers(&read("hvc1.2.4.L153.B0")));
        assert!(!read("hvc1.2.4.L186.B0").covers(&read("dvh1.08.06")));
    }

    #[test]
    fn strings_read_back_as_they_are_written() {
        for s in [
            "hvc1.2.4.L153.B0",
            "hvc1.1.6.L93.B0",
            "hev1.A1.60000000.H120.B0.23",
            "hvc1.2.4.L120",
            "avc1.640028",
            "avc3.4D401F",
            "av01.0.13M.10",
            "dvh1.08.06",
            "dvhe.05.09",
            "dva1.09.05",
            "dav1.10.04",
        ] {
            assert_eq!(read(s).to_string(), s);
        }
    }

    #[test]
    fn a_codec_string_is_its_string_in_json() {
        let s = read("dvh1.08.06");
        assert_eq!(serde_json::to_string(&s).unwrap(), r#""dvh1.08.06""#);
        assert_eq!(
            serde_json::from_str::<CodecString>(r#""dvh1.08.06""#).unwrap(),
            s
        );
        assert!(serde_json::from_str::<CodecString>(r#""dvh1""#).is_err());
    }

    #[test]
    fn what_is_not_a_codec_string_is_refused() {
        for s in [
            "",
            "hevc",
            "hvc1",
            "hvc1.2.4",
            "hvc1.2.4.153.B0",
            "hvc1.2.4.X153.B0",
            "hvc1.2.4.L153.B0.0.0.0.0.0.0",
            "hvc1.2.123456789.L153",
            "hvc1.2.4.L-1",
            "avc1.6400",
            "avc1.64002G",
            "avc1.66.30",
            "av01.0.13.10",
            "av01.0.13X.10",
            "dvh1.8.6",
            "dvh1.08",
            "mp4a.40.2",
        ] {
            assert!(s.parse::<CodecString>().is_err(), "{s}");
        }
    }
}
