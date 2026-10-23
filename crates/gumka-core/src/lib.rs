// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka's image pipeline: opening and saving pictures with their metadata, and the steps
//! around model-based tools (Erase's crop, scale and blend; Upscale's tiles). No UI and no ML
//! here.

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
pub mod upscale;

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

/// Work that reads fewer pixels than this is done on the calling thread: starting threads would
/// cost more than it saves.
pub const PARALLEL_FROM: u64 = 1 << 16;

/// Fills `out`, rows of `width` values, with `fill(first row, rows)`: split over the cores by
/// rows when the work reads at least [`PARALLEL_FROM`] pixels, else on the calling thread.
pub fn by_rows<T: Send>(
    out: &mut [T],
    width: usize,
    pixels_read: u64,
    fill: impl Fn(usize, &mut [T]) + Sync,
) {
    // Nothing to fill: callers split `out` into rows of `width`, which can't be 0.
    if out.is_empty() || width == 0 {
        return;
    }
    let threads = if pixels_read < PARALLEL_FROM {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    if threads == 1 {
        fill(0, out);
        return;
    }
    let rows_each = (out.len() / width).div_ceil(threads).max(1);
    let fill = &fill;
    std::thread::scope(|scope| {
        for (i, chunk) in out.chunks_mut(rows_each * width).enumerate() {
            scope.spawn(move || fill(i * rows_each, chunk));
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Large work is split over threads into the same result as one pass; small work stays on
    /// the calling thread.
    #[test]
    fn rows_split_over_threads_as_one_pass_would_fill_them() {
        let (width, rows) = (300, 250);
        let row = |first: usize, out: &mut [u32]| {
            for (i, v) in out.iter_mut().enumerate() {
                *v = ((first + i / width) * 1000 + i % width) as u32;
            }
        };
        let mut one = vec![0; width * rows];
        row(0, &mut one);
        let mut split = vec![0; width * rows];
        by_rows(&mut split, width, PARALLEL_FROM, row);
        assert_eq!(split, one);

        let caller = std::thread::current().id();
        let mut small = vec![0u32; width * 2];
        by_rows(&mut small, width, PARALLEL_FROM - 1, |_, _| {
            assert_eq!(std::thread::current().id(), caller);
        });
        let threads = std::sync::Mutex::new(std::collections::HashSet::new());
        by_rows(&mut split, width, PARALLEL_FROM, |_, _| {
            threads.lock().unwrap().insert(std::thread::current().id());
        });
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let chunks = rows.div_ceil(rows.div_ceil(cores));
        assert_eq!(
            threads.into_inner().unwrap().len(),
            chunks,
            "one thread a chunk"
        );
    }
}
