//! Frame previews for scrubbing (P3-13): `Request::GetPreviews` and its
//! reply.
//!
//! An item's previews are pictures of it at even intervals, packed row by
//! row into sheets of `columns × rows` tiles. A sheet is an image like
//! any other: read it on the artwork stream (P1-05) by its [`ArtworkId`],
//! at [`crate::ArtworkSize::Original`], once for the item, and draw the
//! part of it that [`Previews::tile`] names. A smaller size scales the
//! whole sheet: scale the tile's rectangle by the image's width over
//! `columns × tile_width`.

use serde::{Deserialize, Serialize};
use spoolframe_types::ArtworkId;

/// An item's previews: what `GetPreviews` answers once they are made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
