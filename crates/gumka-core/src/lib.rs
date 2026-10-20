// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka's image pipeline: opening and saving pictures with their metadata, and the
//! crop/scale/blend steps around model-based tools. No UI and no ML here.

pub mod color;
pub mod erase;
mod jpeg_io;
mod meta;
pub mod picture;
mod png_io;
#[cfg(test)]
mod round_trips;
pub mod select;
mod tiff_io;

pub use image;

/// RGB pixels at 16 bits per channel: the in-memory form of every opened picture.
pub type Rgb16 = image::ImageBuffer<image::Rgb<u16>, Vec<u16>>;

/// One byte per pixel: 0 keeps the pixel, anything else marks it for the tool.
pub type Mask = image::GrayImage;

/// A pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        let (r, b) = (
            self.right().max(other.right()),
            self.bottom().max(other.bottom()),
        );
        Rect::new(x, y, r - x, b - y)
    }
}

/// The bounding box of all non-zero mask pixels, or `None` for an empty mask.
pub fn mask_bounds(mask: &Mask) -> Option<Rect> {
    let (w, h) = mask.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (y, row) in mask.as_raw().chunks_exact(w as usize).enumerate() {
        let Some(first) = row.iter().position(|&v| v != 0) else {
            continue;
        };
        let last = row.iter().rposition(|&v| v != 0).unwrap_or(first);
        x0 = x0.min(first as u32);
        x1 = x1.max(last as u32 + 1);
        y0 = y0.min(y as u32);
        y1 = y as u32 + 1;
    }
    (x1 > x0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Copies a region out of an image.
pub fn crop(image: &Rgb16, rect: Rect) -> Rgb16 {
    image::imageops::crop_imm(image, rect.x, rect.y, rect.width, rect.height).to_image()
}

/// Swaps a region of `image` with `patch` (same size), so applying the result again undoes it.
pub fn swap_region(image: &mut Rgb16, rect: Rect, patch: &mut Rgb16) {
    assert_eq!(patch.dimensions(), (rect.width, rect.height), "patch size");
    let width = image.width() as usize;
    let row_len = rect.width as usize * 3;
    let image = image.as_mut();
    let patch = patch.as_mut();
    for row in 0..rect.height as usize {
        let start = ((rect.y as usize + row) * width + rect.x as usize) * 3;
        image[start..start + row_len]
            .swap_with_slice(&mut patch[row * row_len..(row + 1) * row_len]);
    }
}
