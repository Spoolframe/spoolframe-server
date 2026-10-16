//! Reading a picture's header: the format and the size, before anything
//! is decoded. The decodes themselves, with real ffmpeg, are in
//! `tests/it/picture.rs`.

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/pictures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// CRC-32 (ISO-HDLC), as PNG chunks carry.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PNG of its signature and a header saying `width` × `height` RGBA,
/// and nothing to decode: what a decompression bomb's first bytes say.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut chunk = b"IHDR".to_vec();
    chunk.extend(width.to_be_bytes());
    chunk.extend(height.to_be_bytes());
    chunk.extend([8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(13u32.to_be_bytes());
    png.extend(&chunk);
    png.extend(crc32(&chunk).to_be_bytes());
    png
}

#[test]
fn each_format_says_its_size() {
    for (name, format, size) in [
        ("photo.heic", Format::Heif, (96, 64)),
        ("lossy.webp", Format::Webp, (64, 96)),
        ("lossless.webp", Format::Webp, (80, 60)),
        ("tall.png", Format::Png, (60, 80)),
        ("exif-gps.jpg", Format::Jpeg, (96, 64)),
    ] {
        let header = check(&fixture(name)).unwrap();
        assert_eq!(
            (header.format, (header.width, header.height)),
            (format, size),
            "{name}"
        );
    }
}

/// The extended WebP header's canvas: 24-bit sizes less one.
#[test]
fn an_extended_webp_says_its_canvas() {
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    webp.extend(10u32.to_le_bytes());
    webp.extend([0x10, 0, 0, 0]);
    webp.extend(&(4999u32.to_le_bytes()[..3]));
    webp.extend(&(2999u32.to_le_bytes()[..3]));
    let header = header(&webp).unwrap();
    assert_eq!((header.width, header.height), (5000, 3000));
}

/// A picture of 50 megapixels is refused by its header alone: nothing
/// past it is there to decode.
#[test]
fn fifty_megapixels_are_refused_by_the_header() {
    let refused = check(&png_header(10_000, 5_000)).unwrap_err();
    assert!(
        matches!(
            refused,
            PictureError::TooManyPixels {
                width: 10_000,
                height: 5_000
            }
        ),
        "{refused}"
    );
}

#[test]
fn forty_megapixels_are_taken() {
    assert!(check(&png_header(8_000, 5_000)).is_ok());
}

/// 25 MB is refused by its length, whatever it holds.
#[test]
fn twenty_five_megabytes_are_refused_before_their_header_is_read() {
    let mut big = fixture("tall.png");
    big.resize(25 << 20, 0);
    assert!(matches!(
        check(&big),
        Err(PictureError::TooManyBytes { bytes }) if bytes == 25 << 20
    ));
}

#[test]
fn what_is_not_a_picture_is_unreadable() {
    for bytes in [
        b"%PDF-1.7\n".to_vec(),
        b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
        b"GIF89a\x01\x00\x01\x00".to_vec(),
        Vec::new(),
    ] {
        assert!(
            matches!(check(&bytes), Err(PictureError::Unreadable(_))),
            "{bytes:?}"
        );
    }
}

/// An AVIF is HEIF too, but not HEVC: not taken.
#[test]
fn an_avif_is_not_a_heic() {
    let mut heic = fixture("photo.heic");
    heic[8..12].copy_from_slice(b"avif");
    // Its compatible brands: mif1, heic, miaf.
    heic[20..24].copy_from_slice(b"avis");
    assert!(matches!(check(&heic), Err(PictureError::Unreadable(_))));
}

/// Cut short anywhere, a header is unreadable, never a panic or a size
/// read from past the end.
#[test]
fn a_header_cut_short_is_unreadable() {
    for name in [
        "photo.heic",
        "lossy.webp",
        "lossless.webp",
        "tall.png",
        "exif-gps.jpg",
    ] {
        let whole = fixture(name);
        let at = match name {
            // Its `meta` box runs past the end.
            "photo.heic" => 64,
            "tall.png" => 20,
            "exif-gps.jpg" => 100,
            _ => 24,
        };
        assert!(
            matches!(header(&whole[..at]), Err(PictureError::Unreadable(_))),
            "{name}"
        );
    }
}

/// The largest extent among an HEIF's items: the grid's, not a tile's.
#[test]
fn a_heic_grid_s_size_is_the_largest_extent() {
    fn sized(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend(kind);
        b.extend(body);
        b
    }
    let ispe = |w: u32, h: u32| {
        let mut body = vec![0; 4];
        body.extend(w.to_be_bytes());
        body.extend(h.to_be_bytes());
        sized(b"ispe", &body)
    };
    let ipco = sized(b"ipco", &[ispe(512, 512), ispe(4032, 3024)].concat());
    let iprp = sized(b"iprp", &ipco);
    let meta = sized(b"meta", &[vec![0; 4], iprp].concat());
    let ftyp = sized(b"ftyp", b"heic\0\0\0\0mif1heic");
    let file = [ftyp, meta].concat();
    let header = check(&file).unwrap();
    assert_eq!((header.width, header.height), (4032, 3024));
}

#[test]
fn a_ppm_is_read_as_a_square_only() {
    let square = ppm(b"P6\n2 2\n255\n\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c".to_vec());
    assert_eq!(
        square,
        Some(Square {
            side: 2,
            rgb: (1..=12).collect()
        })
    );
    assert_eq!(
        ppm(b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06".to_vec()),
        None
    );
    assert_eq!(ppm(b"P6\n2 2\n255\n\x01".to_vec()), None);
    assert_eq!(ppm(b"P5\n1 1\n255\n\x01".to_vec()), None);
}
