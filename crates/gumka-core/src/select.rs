// SPDX-License-Identifier: GPL-3.0-or-later

//! Click-to-select around a segmentation model such as SAM: the picture goes in once, then
//! each set of clicks gives a mask.
//!
//! The picture is stretched to the model's square input, as the SAM 2.1 Core ML models
//! expect; clicks are scaled the same way, and the model's low-resolution scores are scaled
//! back up to the picture and cut at zero.

use image::imageops::{self, FilterType};
use image::{Luma, Rgb, RgbImage};

use crate::picture::to_u8;
use crate::{Mask, Rgb16};

pub type SegmentError = Box<dyn std::error::Error + Send + Sync>;

/// A click on the picture. `include: false` marks an area to leave out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Click {
    pub x: f32,
    pub y: f32,
    pub include: bool,
}

/// A model's low-resolution mask scores: above zero means selected.
pub struct Scores {
    pub width: u32,
    pub height: u32,
    pub values: Vec<f32>,
}

/// A model that turns clicks on an image into a mask.
pub trait Segmenter {
    /// Size of the image the model takes, width by height.
    fn input_size(&self) -> (u32, u32);

    /// The most clicks [`Self::segment`] takes at once.
    fn max_clicks(&self) -> usize;

    /// Takes in the image clicks will refer to; it is at [`Self::input_size`].
    fn set_image(&mut self, image: &RgbImage) -> Result<(), SegmentError>;

    /// Mask scores for clicks given in input-size coordinates.
    fn segment(&mut self, clicks: &[Click]) -> Result<Scores, SegmentError>;
}

/// The picture stretched to the model's input size, in 8 bits.
pub fn model_input(pixels: &Rgb16, (w, h): (u32, u32)) -> RgbImage {
    let scaled = imageops::resize(pixels, w, h, FilterType::Triangle);
    RgbImage::from_fn(w, h, |x, y| Rgb(scaled.get_pixel(x, y).0.map(to_u8)))
}

/// Clicks from picture pixels to the model's input coordinates.
pub fn to_input(clicks: &[Click], (pw, ph): (u32, u32), (iw, ih): (u32, u32)) -> Vec<Click> {
    let (sx, sy) = (iw as f32 / pw as f32, ih as f32 / ph as f32);
    clicks
        .iter()
        .map(|c| Click {
            x: c.x * sx,
            y: c.y * sy,
            include: c.include,
        })
        .collect()
}

/// Scores scaled up to a `width` by `height` picture (bilinear) and cut at zero.
pub fn mask_from_scores(scores: &Scores, (width, height): (u32, u32)) -> Mask {
    let (sw, sh) = (scores.width as usize, scores.height as usize);
    let at = |x: usize, y: usize| scores.values[y.min(sh - 1) * sw + x.min(sw - 1)];
    // Source position of each output column, as (index, weight of the next one).
    let sample = |i: u32, from: u32, to: usize| {
        let p = ((i as f32 + 0.5) * to as f32 / from as f32 - 0.5).max(0.0);
        (p.floor() as usize, p.fract())
    };
    let columns: Vec<(usize, f32)> = (0..width).map(|x| sample(x, width, sw)).collect();
    let mut mask = Mask::new(width, height);
    for y in 0..height {
        let (y0, fy) = sample(y, height, sh);
        for (x, &(x0, fx)) in columns.iter().enumerate() {
            let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
            let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
            if top * (1.0 - fy) + bottom * fy > 0.0 {
                mask.put_pixel(x as u32, y, Luma([255]));
            }
        }
    }
    mask
}

/// Runs a whole selection: the picture, clicks in picture pixels, a picture-sized mask.
pub fn select(
    pixels: &Rgb16,
    clicks: &[Click],
    segmenter: &mut dyn Segmenter,
) -> Result<Mask, SegmentError> {
    let size = segmenter.input_size();
    segmenter.set_image(&model_input(pixels, size))?;
    let scores = segmenter.segment(&to_input(clicks, pixels.dimensions(), size))?;
    Ok(mask_from_scores(&scores, pixels.dimensions()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Selects a disk around the first included click, in input coordinates.
    struct Disk {
        image_set: bool,
    }

    impl Segmenter for Disk {
        fn input_size(&self) -> (u32, u32) {
            (64, 64)
        }

        fn max_clicks(&self) -> usize {
            4
        }

        fn set_image(&mut self, image: &RgbImage) -> Result<(), SegmentError> {
            assert_eq!(image.dimensions(), (64, 64));
            self.image_set = true;
            Ok(())
        }

        fn segment(&mut self, clicks: &[Click]) -> Result<Scores, SegmentError> {
            assert!(self.image_set);
            let c = clicks.iter().find(|c| c.include).ok_or("no click")?;
            // Scores at 16 x 16 for a 64 x 64 input: a disk of radius 8 input pixels.
            let values = (0..16 * 16)
                .map(|i| {
                    let (x, y) = ((i % 16) as f32 * 4.0 + 2.0, (i / 16) as f32 * 4.0 + 2.0);
                    8.0 - ((x - c.x).powi(2) + (y - c.y).powi(2)).sqrt()
                })
                .collect();
            Ok(Scores {
                width: 16,
                height: 16,
                values,
            })
        }
    }

    #[test]
    fn clicks_select_around_the_point_in_picture_pixels() {
        let pixels = Rgb16::new(400, 200);
        let click = Click {
            x: 300.0,
            y: 50.0,
            include: true,
        };
        let mask = select(&pixels, &[click], &mut Disk { image_set: false }).unwrap();
        assert_eq!(mask.dimensions(), (400, 200));
        assert_eq!(mask.get_pixel(300, 50).0[0], 255, "the click is selected");
        assert_eq!(mask.get_pixel(100, 150).0[0], 0, "far away is not");
        // A radius of 8 input pixels is 50 picture pixels across and 25 down.
        assert_eq!(mask.get_pixel(340, 50).0[0], 255);
        assert_eq!(mask.get_pixel(360, 50).0[0], 0);
        assert_eq!(mask.get_pixel(300, 70).0[0], 255);
        assert_eq!(mask.get_pixel(300, 85).0[0], 0);
    }

    #[test]
    fn negative_scores_select_nothing() {
        let scores = Scores {
            width: 2,
            height: 2,
            values: vec![-1.0; 4],
        };
        assert!(
            mask_from_scores(&scores, (10, 10))
                .pixels()
                .all(|p| p.0[0] == 0)
        );
    }
}
