//! Shared by the fuzz targets: how one fuzz input becomes the packets of a
//! subtitle track.
//!
//! A track is a list of packets, each with a time. An input is read as
//! records of `u16 LE length, u32 LE pts in ms, that many bytes`, until it
//! runs out; a record cut short ends the list. The seeds in `seeds/` are
//! tracks ffmpeg encoded, written this way by `examples/seeds.rs`.

use spoolframe_media::bitmap::Packet;

/// Split `input` into packets: see the module docs.
pub fn packets(mut input: &[u8]) -> Vec<Packet> {
    let mut packets = Vec::new();
    while input.len() >= 6 {
        let len = usize::from(u16::from_le_bytes([input[0], input[1]]));
        let pts_ms = u64::from(u32::from_le_bytes([input[2], input[3], input[4], input[5]]));
        input = &input[6..];
        let Some(data) = input.get(..len) else {
            break;
        };
        packets.push(Packet {
            pts_ms,
            duration_ms: None,
            data: data.to_vec(),
        });
        input = &input[len..];
    }
    packets
}

/// `input` as a VobSub track: a `u16 LE` length and that much `.idx`
/// header, then packets as [`packets`] reads them.
pub fn vobsub(input: &[u8]) -> Option<(&[u8], Vec<Packet>)> {
    let len = usize::from(u16::from_le_bytes([*input.first()?, *input.get(1)?]));
    let header = input.get(2..2 + len)?;
    Some((header, packets(&input[2 + len..])))
}

/// The inverse of [`packets`], for writing seeds.
pub fn write_packets(packets: &[Packet]) -> Vec<u8> {
    let mut out = Vec::new();
    for packet in packets {
        let len = u16::try_from(packet.data.len()).expect("a packet under 64 KiB");
        out.extend_from_slice(&len.to_le_bytes());
        let pts = u32::try_from(packet.pts_ms).unwrap_or(u32::MAX);
        out.extend_from_slice(&pts.to_le_bytes());
        out.extend_from_slice(&packet.data);
    }
    out
}
