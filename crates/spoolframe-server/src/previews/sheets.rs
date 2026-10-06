//! Tiles packed into sheets: a grid of [`COLUMNS`] × [`ROWS`] frames in one
//! JPEG, row by row, so a client fetches a few images per film rather than
//! hundreds. The last sheet is filled only as far as there are tiles; the
//! rest of it is black.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{GenericImage, RgbImage};

/// Tiles across a sheet.
pub const COLUMNS: u32 = 10;
/// Tiles down a sheet.
pub const ROWS: u32 = 10;
/// How wide each tile is; its height keeps the picture's shape.
pub const TILE_WIDTH: u32 = 320;
/// JPEG quality of a sheet. Tiles are seen small and briefly, under a
/// moving pointer: 70 keeps a two-hour film's sheets to a few MB.
const QUALITY: u8 = 70;

/// Tiles per sheet.
pub const PER_SHEET: u32 = COLUMNS * ROWS;

/// What [`pack`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub tile_width: u32,
    pub tile_height: u32,
    /// How many tiles there are, over every sheet.
    pub count: u32,
    /// JPEG sheets, in order.
    pub sheets: Vec<Vec<u8>>,
}

/// `tiles` (encoded images, in time order) packed into sheets. Every tile
/// takes the first one's size: they are frames of one film, and one that
/// differs (a part in another aspect ratio) is scaled to fit.
pub fn pack(tiles: &[Vec<u8>]) -> Result<Packed, String> {
    let first = tiles.first().ok_or("no tiles")?;
    let first = image::load_from_memory(first).map_err(|e| format!("tile 0: {e}"))?;
    let (tile_width, tile_height) = (first.width(), first.height());
    if tile_width == 0 || tile_height == 0 {
        return Err("tile 0 is empty".into());
    }
    // Every sheet is ten by ten of these (PX-09 finding 26): the height is
    // the film's own shape, which a crafted file sets.
    if tile_width > TILE_WIDTH || tile_height > 2 * tile_width {
        return Err(format!(
            "tile 0 is {tile_width}×{tile_height}: no film is that shape"
        ));
    }
    let mut sheets = Vec::new();
    for (n, chunk) in tiles.chunks(PER_SHEET as usize).enumerate() {
        let mut sheet = RgbImage::new(tile_width * COLUMNS, tile_height * ROWS);
        for (i, bytes) in chunk.iter().enumerate() {
            let at = n * PER_SHEET as usize + i;
            let mut tile = image::load_from_memory(bytes)
                .map_err(|e| format!("tile {at}: {e}"))?
                .to_rgb8();
            if tile.dimensions() != (tile_width, tile_height) {
                tile =
                    image::imageops::resize(&tile, tile_width, tile_height, FilterType::Triangle);
            }
            let i = i as u32;
            sheet
                .copy_from(
                    &tile,
                    (i % COLUMNS) * tile_width,
                    (i / COLUMNS) * tile_height,
                )
                .map_err(|e| format!("tile {at}: {e}"))?;
        }
        let mut out = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut out, QUALITY)
            .encode_image(&sheet)
            .map_err(|e| format!("sheet {n}: {e}"))?;
        sheets.push(out.into_inner());
    }
    Ok(Packed {
        tile_width,
        tile_height,
        count: tiles.len() as u32,
        sheets,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::Rgb;

    /// A JPEG tile of one colour.
    pub(crate) fn tile(width: u32, height: u32, grey: u8) -> Vec<u8> {
        let img = RgbImage::from_pixel(width, height, Rgb([grey, grey, grey]));
        let mut out = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&img)
            .unwrap();
        out.into_inner()
    }

    /// The mean grey of tile `i` of a sheet.
    fn grey_at(sheet: &[u8], i: u32, width: u32, height: u32) -> u8 {
        let sheet = image::load_from_memory(sheet).unwrap().to_rgb8();
        let (x, y) = ((i % COLUMNS) * width, (i / COLUMNS) * height);
        let sum: u64 = (0..height)
            .flat_map(|dy| (0..width).map(move |dx| (dx, dy)))
            .map(|(dx, dy)| u64::from(sheet.get_pixel(x + dx, y + dy).0[0]))
            .sum();
        (sum / u64::from(width * height)) as u8
    }

    #[test]
    fn tiles_fill_sheets_row_by_row_and_the_last_sheet_part_way() {
        // 150 tiles: one full sheet and half of another.
        let tiles: Vec<_> = (0..150)
            .map(|i| tile(32, 18, (i % 2) as u8 * 200))
            .collect();
        let packed = pack(&tiles).unwrap();
        assert_eq!((packed.tile_width, packed.tile_height), (32, 18));
        assert_eq!(packed.count, 150);
        assert_eq!(packed.sheets.len(), 2);
        let sheet = image::load_from_memory(&packed.sheets[1]).unwrap();
        assert_eq!((sheet.width(), sheet.height()), (320, 180));
        // Tile 113 is the 14th of sheet 1: row 1, column 3, odd, so light.
        assert!(grey_at(&packed.sheets[1], 13, 32, 18) > 180);
        assert!(grey_at(&packed.sheets[1], 12, 32, 18) < 20);
        // Past the last tile, black.
        assert!(grey_at(&packed.sheets[1], 51, 32, 18) < 20);
    }

    #[test]
    fn a_tile_of_another_size_is_scaled_to_the_first() {
        let packed = pack(&[tile(32, 18, 0), tile(64, 48, 255)]).unwrap();
        assert_eq!((packed.tile_width, packed.tile_height), (32, 18));
        assert!(grey_at(&packed.sheets[0], 1, 32, 18) > 230);
    }

    /// PX-09 finding 26 (B-7): a sheet is ten tiles by ten of the first
    /// tile's size, and the first tile's height is the film's own shape:
    /// a crafted aspect ratio made a sheet of gigabytes. A tile is at most
    /// [`TILE_WIDTH`] wide and twice as tall as it is wide.
    #[test]
    fn a_first_tile_of_no_film_s_shape_is_refused() {
        assert!(pack(&[tile(32, 4000, 0)]).is_err());
        assert!(pack(&[tile(4000, 18, 0)]).is_err());
        // A portrait film, half as wide as it is tall, is still one.
        assert!(pack(&[tile(32, 64, 0)]).is_ok());
    }

    #[test]
    fn no_tiles_is_an_error() {
        assert!(pack(&[]).is_err());
    }
}
