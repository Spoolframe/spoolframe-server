//! Covers kept from uploads, with real ffmpeg; the ids; the mosaic and
//! the tiles.

use std::collections::HashSet;

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../spoolframe-media/tests/fixtures/pictures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn covers(test: &str) -> (Covers, spoolframe_test_dir::TestDir) {
    let dir = spoolframe_test_dir::TestDir::new(&format!("spoolframe-covers-{test}")).await;
    (Covers::new(dir.as_ref(), Tool::unconfined("ffmpeg")), dir)
}

/// The JPEG's segments before its picture data: each marker's byte.
fn markers(jpeg: &[u8]) -> Vec<u8> {
    let mut found = Vec::new();
    let mut at = 2;
    while at + 4 <= jpeg.len() && jpeg[at] == 0xff {
        let marker = jpeg[at + 1];
        found.push(marker);
        if marker == 0xda {
            break;
        }
        let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        at += 2 + length;
    }
    found
}

/// The upload's EXIF block, with its camera and GPS position, is not in
/// the cover kept: it is the server's own JPEG of the pixels.
#[tokio::test]
async fn no_exif_or_gps_survives() {
    let (covers, _dir) = covers("exif").await;
    let upload = fixture("exif-gps.jpg");
    assert!(upload.windows(4).any(|w| w == b"Exif"));
    let id = covers.ingest(&upload).await.unwrap();
    let kept = covers.read(&id).await.unwrap();
    assert!(!kept.windows(4).any(|w| w == b"Exif"));
    assert!(!kept.windows(13).any(|w| w == b"SpoolframeCam"));
    // No APP1 (EXIF, XMP) or APP2 (ICC) segment at all.
    let segments = markers(&kept);
    assert!(
        !segments.contains(&0xe1) && !segments.contains(&0xe2),
        "{segments:x?}"
    );
    let picture = image::load_from_memory(&kept).unwrap();
    assert_eq!((picture.width(), picture.height()), (64, 64));
}

/// A large photo is kept at most 2048 pixels a side, square.
#[tokio::test]
async fn a_large_photo_is_kept_at_most_2048_square() {
    let (covers, dir) = covers("large").await;
    let path = dir.join("wide.jpg");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=3000x2500",
        ])
        .args(["-frames:v", "1"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let id = covers.ingest(&std::fs::read(&path).unwrap()).await.unwrap();
    let kept = image::load_from_memory(&covers.read(&id).await.unwrap()).unwrap();
    assert_eq!((kept.width(), kept.height()), (2048, 2048));
}

/// An iPhone's HEIC is taken, by ffmpeg confined as the server runs it:
/// the whole state directory hidden but for the upload's own.
#[tokio::test]
async fn a_heic_is_kept_as_a_jpeg() {
    let dir = spoolframe_test_dir::TestDir::new("spoolframe-covers-heic").await;
    let sandbox = spoolframe_media::sandbox::Sandbox::new(
        spoolframe_media::sandbox::Mode::Required,
        vec![dir.to_path_buf()],
        spoolframe_media::sandbox::GpuDevices::default(),
    )
    .expect("this kernel has Landlock");
    let covers = Covers::new(dir.as_ref(), Tool::new("ffmpeg", sandbox));
    let id = covers.ingest(&fixture("photo.heic")).await.unwrap();
    let kept = covers.read(&id).await.unwrap();
    assert_eq!(
        image::guess_format(&kept).unwrap(),
        image::ImageFormat::Jpeg
    );
}

#[tokio::test]
async fn the_same_picture_twice_is_one_file_and_a_sweep_keeps_what_is_used() {
    let (covers, _dir) = covers("same").await;
    let first = covers.ingest(&fixture("tall.png")).await.unwrap();
    let again = covers.ingest(&fixture("tall.png")).await.unwrap();
    let other = covers.ingest(&fixture("lossy.webp")).await.unwrap();
    assert_eq!(first, again);
    assert_ne!(first, other);
    let used: HashSet<String> = [first.to_string()].into();
    assert_eq!(covers.sweep(&used).await.unwrap(), 1);
    assert!(covers.has(&first).await);
    assert!(!covers.has(&other).await);
    covers.remove(&first).await;
    assert!(!covers.has(&first).await);
}

#[tokio::test]
async fn too_large_and_unreadable_are_told_apart_before_decoding() {
    // ffmpeg that is not there: nothing here may reach it.
    let dir = spoolframe_test_dir::TestDir::new("spoolframe-covers-refused").await;
    let covers = Covers::new(dir.as_ref(), Tool::unconfined("/nonexistent/ffmpeg"));
    let mut big = fixture("tall.png");
    big.resize(25 << 20, 0);
    assert!(matches!(
        covers.ingest(&big).await,
        Err(CoverError::TooLarge(_))
    ));
    assert!(matches!(
        covers.ingest(b"not a picture").await,
        Err(CoverError::Unreadable(_))
    ));
}

#[test]
fn ids_name_what_they_are_and_nothing_else() {
    let hash = "0123456789abcdef0123456789abcdef";
    assert_eq!(cover_id(hash).to_string(), format!("image:playlist/{hash}"));
    assert_eq!(local(&cover_id(hash)), Some(Local::Cover(hash.into())));
    let parts: Vec<ArtworkId> = (1..=4)
        .map(|n| {
            format!("plex:/library/metadata/{n}/thumb/7")
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(
        local(&mosaic_id(&parts)),
        Some(Local::Mosaic(parts.clone()))
    );
    assert_eq!(local(&tile_id(Tile::Liked)), Some(Local::Tile(Tile::Liked)));
    for bad in [
        "image:playlist/../../etc/passwd",
        "image:playlist/0123",
        "image:tile/liked/2",
        "plex:/library/metadata/1/thumb/1",
    ] {
        assert_eq!(local(&bad.parse().unwrap()), None, "{bad}");
    }
    // Three parts, or a mosaic of mosaics, are no mosaic.
    assert_eq!(local(&mosaic_id(&parts[..3])), None);
    let nested = [
        mosaic_id(&parts),
        parts[1].clone(),
        parts[2].clone(),
        parts[3].clone(),
    ];
    assert_eq!(local(&mosaic_id(&nested)), None);
}

fn png(color: [u8; 3], width: u32, height: u32) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb(color)))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// The four, left to right and top to bottom, each its centre square.
#[test]
fn a_mosaic_is_the_four_in_order() {
    let colors = [[250, 0, 0], [0, 250, 0], [0, 0, 250], [250, 250, 0]];
    let tiles: Vec<Vec<u8>> = colors.iter().map(|c| png(*c, 400, 600)).collect();
    let mosaic = image::load_from_memory(&compose(&tiles).unwrap())
        .unwrap()
        .to_rgb8();
    assert_eq!(mosaic.dimensions(), (1200, 1200));
    for (n, color) in colors.iter().enumerate() {
        let (x, y) = (300 + (n as u32 % 2) * 600, 300 + (n as u32 / 2) * 600);
        let got = mosaic.get_pixel(x, y).0;
        let near = got.iter().zip(color).all(|(a, b)| a.abs_diff(*b) < 12);
        assert!(near, "tile {n}: {got:?}, not {color:?}");
    }
}

#[test]
fn each_tile_is_drawn_and_they_differ() {
    let drawn: Vec<Vec<u8>> = [Tile::Liked, Tile::All, Tile::Empty]
        .into_iter()
        .map(|t| draw(t).unwrap())
        .collect();
    for tile in &drawn {
        let picture = image::load_from_memory(tile).unwrap();
        assert_eq!((picture.width(), picture.height()), (1024, 1024));
    }
    assert_ne!(drawn[0], drawn[1]);
    assert_ne!(drawn[1], drawn[2]);
}
