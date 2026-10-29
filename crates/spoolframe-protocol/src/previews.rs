//! Frame previews for scrubbing: `Request::GetPreviews` and its
//! reply.
//!
//! An item's previews are pictures of it at even intervals, packed row by
//! row into sheets of `columns × rows` tiles. A sheet is an image like
//! any other: read it on the artwork stream by its [`ArtworkId`],
//! at [`crate::ArtworkSize::Original`], once for the item, and draw the
//! part of it that [`Previews::tile`] names. A smaller size scales the
//! whole sheet: scale the tile's rectangle by the image's width over
//! `columns × tile_width`.

use serde::{Deserialize, Serialize};
use spoolframe_types::ArtworkId;

/// An item's previews: what `GetPreviews` answers once they are made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Previews {
    /// Tile `i` is the picture at about `i × interval_ms` (the keyframe
    /// at or just before it).
    pub interval_ms: u64,
    pub tile_width: u32,
    pub tile_height: u32,
    /// Tiles across a sheet.
    pub columns: u32,
    /// Tiles down a sheet.
    pub rows: u32,
    /// Tiles in all. The last sheet is filled only this far.
    pub count: u32,
    /// The sheets, in order: tiles `0..columns × rows` are on the first.
    pub sheets: Vec<ArtworkId>,
}

/// Where the picture for a time is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewTile {
    /// Which tile, from 0.
    pub index: u32,
    /// Which sheet: an index into [`Previews::sheets`].
    pub sheet: usize,
    /// The tile's rectangle in the sheet, in pixels at its original size.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Previews {
    /// The tile to show for `position_ms`: the last one at or before it;
    /// past the end, the last one. `None` only for previews with no
    /// tiles, which the server does not send.
    pub fn tile(&self, position_ms: u64) -> Option<PreviewTile> {
        let per_sheet = self.columns.checked_mul(self.rows)?;
        if self.count == 0 || per_sheet == 0 || self.interval_ms == 0 {
            return None;
        }
        let index = (position_ms / self.interval_ms).min(u64::from(self.count - 1)) as u32;
        let sheet = (index / per_sheet) as usize;
        if sheet >= self.sheets.len() {
            return None;
        }
        let at = index % per_sheet;
        Some(PreviewTile {
            index,
            sheet,
            x: (at % self.columns) * self.tile_width,
            y: (at / self.columns) * self.tile_height,
            width: self.tile_width,
            height: self.tile_height,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Finding the tile for a time in an item's previews (P3-13).

    use crate::{PreviewTile, Previews};
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
}
