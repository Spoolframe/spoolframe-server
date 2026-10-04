//! Finding the tile for a time in an item's previews (P3-13).

use spoolframe_protocol::{PreviewTile, Previews};
use spoolframe_types::ArtworkId;

/// A two-hour film at 10 s: 720 tiles of 320 × 134 on 8 sheets.
fn film() -> Previews {
    Previews {
        interval_ms: 10_000,
        tile_width: 320,
        tile_height: 134,
        columns: 10,
        rows: 10,
        count: 720,
        sheets: (0..8)
            .map(|n| format!("preview:abc/{n}").parse::<ArtworkId>().unwrap())
            .collect(),
    }
}

#[test]
fn the_tile_for_1_23_40_is_on_its_sheet_by_row_and_column() {
    // 1:23:40 is 5020 s: tile 502, the 3rd of sheet 5, in its first row.
    assert_eq!(
        film().tile(5_020_000),
        Some(PreviewTile {
            index: 502,
            sheet: 5,
            x: 2 * 320,
            y: 0,
            width: 320,
            height: 134,
        })
    );
    // 37 s past it: still tile 505's span; tile 505 is the 6th, row 0.
    let tile = film().tile(5_057_000).unwrap();
    assert_eq!((tile.index, tile.x, tile.y), (505, 5 * 320, 0));
    // Tile 579: sheet 5, row 7, column 9.
    let tile = film().tile(5_799_999).unwrap();
    assert_eq!(
        (tile.index, tile.sheet, tile.x, tile.y),
        (579, 5, 9 * 320, 7 * 134)
    );
}

#[test]
fn a_time_between_tiles_shows_the_one_before() {
    assert_eq!(film().tile(0).unwrap().index, 0);
    assert_eq!(film().tile(9_999).unwrap().index, 0);
    assert_eq!(film().tile(10_000).unwrap().index, 1);
}

#[test]
fn past_the_end_is_the_last_tile() {
    let tile = film().tile(10 * 3_600_000).unwrap();
    assert_eq!((tile.index, tile.sheet), (719, 7));
    assert_eq!((tile.x, tile.y), (9 * 320, 134));
}

#[test]
fn previews_without_tiles_have_none() {
    let empty = Previews {
        count: 0,
        sheets: vec![],
        ..film()
    };
    assert_eq!(empty.tile(0), None);
}

#[test]
fn a_grid_that_is_not_square_is_read_by_its_columns() {
    // 4 across, 3 down: 12 a sheet.
    let previews = Previews {
        columns: 4,
        rows: 3,
        count: 20,
        sheets: film().sheets[..2].to_vec(),
        ..film()
    };
    let tile = previews.tile(70_000).unwrap();
    assert_eq!((tile.sheet, tile.x, tile.y), (0, 3 * 320, 134));
    let tile = previews.tile(130_000).unwrap();
    assert_eq!((tile.index, tile.sheet, tile.x, tile.y), (13, 1, 320, 0));
}
