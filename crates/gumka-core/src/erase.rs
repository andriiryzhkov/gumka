// SPDX-License-Identifier: GPL-3.0-or-later

//! The erase pipeline around an inpainting model.
//!
//! 1. Crop around the mask with context: 2.5x the mask's size, at least the model's input size
//!    when the image is that big, so small objects are filled at full resolution.
//! 2. Grow the mask a little (covers anti-aliased object edges) and scale crop and mask to the
//!    model's input size. Pixel values go in as they are, in the image's own encoding, unless
//!    its profile is linear or very wide: then the scaled copy is converted to Display P3 (see
//!    [`ModelSpace`]); colors outside P3 are clipped in what the model sees.
//! 3. The model fills the masked area.
//! 4. Convert the result back to the picture's profile if it was converted, scale it back and
//!    blend it in through the grown mask with a feathered edge. A crop that is all gray (a
//!    grayscale picture) gets a gray fill, so it stays grayscale.
//!
//! Pixels the feathered mask does not reach are copied, never recomputed or converted, so they
//! stay bit-identical to the input.

use image::imageops::{self, FilterType};
use image::{GrayImage, Luma, Rgb, RgbImage};

use crate::color::ModelSpace;
use crate::picture::to_u8;
use crate::{Mask, Rect, Rgb16, mask_bounds};

pub type InpaintError = Box<dyn std::error::Error + Send + Sync>;

/// A model that fills masked areas of a fixed-size 8-bit image.
pub trait Inpainter {
    /// Input (and output) size, width by height.
    fn input_size(&self) -> (u32, u32);

    /// Fills the pixels where `mask` is 255. Both inputs are at [`Self::input_size`]; so is the
    /// result.
    fn inpaint(&mut self, image: &RgbImage, mask: &GrayImage) -> Result<RgbImage, InpaintError>;
}

/// How an erase joins the model's fill to the photo. Grow and feather are in model pixels, so
/// they scale with the crop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EraseSettings {
    /// Mask growth before inpainting: covers an object's soft outline and halo.
    pub grow: f32,
    /// Width of the soft edge outside the grown mask.
    pub feather: f32,
    /// Context around the mask, as a multiple of the mask's larger side.
    pub context: f32,
}

impl Default for EraseSettings {
    fn default() -> Self {
        Self {
            grow: 3.0,
            feather: 3.0,
            context: 2.5,
        }
    }
}

/// The prepared crop for one erase. Holds only crop-sized data, so it can move to a worker
/// thread while the full image stays where it is.
pub struct ErasePlan {
    /// The model's color space, for a picture whose own would mislead it.
    space: Option<ModelSpace>,
    crop: Rect,
    original: Rgb16,
    /// Grown mask in crop coordinates, 255 = fill.
    fill: GrayImage,
    /// Blend weight per crop pixel, 0..=65535.
    alpha: Vec<u16>,
    /// How far beyond the mask the blend reaches, in image pixels.
    reach: u32,
    model_size: (u32, u32),
    /// Every pixel of the crop is gray: the fill is made gray too.
    gray: bool,
}

/// Replacement pixels for a rectangle of the image. Once swapped in (see
/// [`crate::swap_region`]), `pixels` holds what they replaced.
pub struct Patch {
    pub rect: Rect,
    pub pixels: Rgb16,
}

impl ErasePlan {
    /// Returns `None` if the mask is empty. `profile` is the picture's ICC profile.
    pub fn new(
        image: &Rgb16,
        mask: &Mask,
        model_size: (u32, u32),
        settings: &EraseSettings,
        profile: Option<&[u8]>,
    ) -> Option<Self> {
        assert_eq!(
            image.dimensions(),
            mask.dimensions(),
            "image and mask sizes differ"
        );
        let bounds = mask_bounds(mask)?;
        let (w, h) = image.dimensions();
        let (mw, mh) = model_size;

        let side =
            (bounds.width.max(bounds.height) as f32 * settings.context.max(1.0)).ceil() as u32;
        let (cw, ch) = (side.max(mw).min(w), side.max(mh).min(h));
        let scale = (cw as f32 / mw as f32).max(ch as f32 / mh as f32);
        let grow = (settings.grow.max(0.0) * scale).ceil() as u32;
        let feather = (settings.feather.max(0.0) * scale).ceil() as u32;
        // The blend reaches this far beyond the mask; keep it inside the crop.
        let reach = grow + 2 * feather;
        let cw = cw.max(bounds.width + 2 * reach).min(w);
        let ch = ch.max(bounds.height + 2 * reach).min(h);
        let x = (bounds.x + bounds.width / 2)
            .saturating_sub(cw / 2)
            .min(w - cw);
        let y = (bounds.y + bounds.height / 2)
            .saturating_sub(ch / 2)
            .min(h - ch);
        let crop = Rect::new(x, y, cw, ch);

        let mask = imageops::crop_imm(mask, x, y, cw, ch).to_image();
        let fill = dilate(&mask, grow);
        let alpha = feathered(&dilate(&fill, feather), feather);
        let original = crate::crop(image, crop);
        let gray = original.pixels().all(|p| p[0] == p[1] && p[1] == p[2]);
        Some(ErasePlan {
            space: ModelSpace::for_profile(profile),
            crop,
            original,
            fill,
            alpha,
            reach,
            model_size,
            gray,
        })
    }

    /// The image region this plan reads.
    pub fn crop(&self) -> Rect {
        self.crop
    }

    /// How far beyond the mask pixels may change, in image pixels.
    pub fn reach(&self) -> u32 {
        self.reach
    }

    /// The model's inputs: the crop scaled to the model size in 8 bits (in the model's space),
    /// and the grown mask.
    pub fn model_inputs(&self) -> (RgbImage, GrayImage) {
        let (mw, mh) = self.model_size;
        let mut scaled = resize(&self.original, mw, mh, FilterType::Lanczos3);
        if let Some(space) = &self.space {
            space.to_model(&mut scaled);
        }
        let image = RgbImage::from_fn(mw, mh, |x, y| Rgb(scaled.get_pixel(x, y).0.map(to_u8)));
        let scaled = resize(&self.fill, mw, mh, FilterType::Triangle);
        let mask = GrayImage::from_fn(mw, mh, |x, y| {
            Luma([if scaled.get_pixel(x, y).0[0] > 0 {
                255
            } else {
                0
            }])
        });
        (image, mask)
    }

    /// Blends the model's output into the crop and returns the pixels that change.
    pub fn finish(&self, output: &RgbImage) -> Result<Patch, InpaintError> {
        if output.dimensions() != self.model_size {
            return Err(format!(
                "model returned {:?}, expected {:?}",
                output.dimensions(),
                self.model_size
            )
            .into());
        }
        let (cw, ch) = (self.crop.width, self.crop.height);
        let mut fill = Rgb16::from_fn(output.width(), output.height(), |x, y| {
            Rgb(output.get_pixel(x, y).0.map(|v| u16::from(v) * 257))
        });
        if let Some(space) = &self.space {
            space.to_picture(&mut fill);
        }
        let mut fill = resize(&fill, cw, ch, FilterType::CatmullRom);
        if self.gray {
            for p in fill.pixels_mut() {
                *p = Rgb([crate::picture::gray_value(p.0); 3]);
            }
        }

        let reach = alpha_bounds(&self.alpha, cw, ch).unwrap_or(Rect::new(0, 0, 0, 0));
        let pixels = Rgb16::from_fn(reach.width, reach.height, |px, py| {
            let (x, y) = (reach.x + px, reach.y + py);
            let a = u32::from(self.alpha[(y * cw + x) as usize]);
            let original = self.original.get_pixel(x, y).0;
            match a {
                0 => Rgb(original),
                65535 => *fill.get_pixel(x, y),
                _ => {
                    let new = fill.get_pixel(x, y).0;
                    Rgb(std::array::from_fn(|c| {
                        let mixed = u32::from(original[c]) * (65535 - a) + u32::from(new[c]) * a;
                        ((mixed + 32767) / 65535) as u16
                    }))
                }
            }
        });
        let rect = Rect::new(
            self.crop.x + reach.x,
            self.crop.y + reach.y,
            reach.width,
            reach.height,
        );
        Ok(Patch { rect, pixels })
    }
}

/// Runs a whole erase in place (`profile` is the picture's ICC profile). Returns the changed
/// rectangle with the pixels it had before, for undo; `None` for an empty mask.
pub fn erase(
    image: &mut Rgb16,
    mask: &Mask,
    inpainter: &mut dyn Inpainter,
    settings: &EraseSettings,
    profile: Option<&[u8]>,
) -> Result<Option<Patch>, InpaintError> {
    let size = inpainter.input_size();
    let Some(plan) = ErasePlan::new(image, mask, size, settings, profile) else {
        return Ok(None);
    };
    let (input, input_mask) = plan.model_inputs();
    let output = inpainter.inpaint(&input, &input_mask)?;
    let mut patch = plan.finish(&output)?;
    crate::swap_region(image, patch.rect, &mut patch.pixels);
    Ok(Some(patch))
}

fn resize<P: image::Pixel + 'static>(
    image: &image::ImageBuffer<P, Vec<P::Subpixel>>,
    width: u32,
    height: u32,
    filter: FilterType,
) -> image::ImageBuffer<P, Vec<P::Subpixel>> {
    if image.dimensions() == (width, height) {
        image.clone()
    } else {
        imageops::resize(image, width, height, filter)
    }
}

/// Binary dilation with a square of radius `r`: 255 wherever a non-zero pixel is within `r`
/// pixels horizontally and vertically.
pub(crate) fn dilate(mask: &GrayImage, r: u32) -> GrayImage {
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    let mut set: Vec<bool> = mask.as_raw().iter().map(|&v| v != 0).collect();
    if r > 0 {
        let r = r as usize;
        let mut line = Vec::new();
        for y in 0..h {
            line.clear();
            line.extend((0..w).map(|x| set[y * w + x]));
            for (x, v) in dilate_line(&line, r).into_iter().enumerate() {
                set[y * w + x] = v;
            }
        }
        for x in 0..w {
            line.clear();
            line.extend((0..h).map(|y| set[y * w + x]));
            for (y, v) in dilate_line(&line, r).into_iter().enumerate() {
                set[y * w + x] = v;
            }
        }
    }
    GrayImage::from_raw(
        w as u32,
        h as u32,
        set.into_iter().map(|v| if v { 255 } else { 0 }).collect(),
    )
    .expect("same size")
}

/// Sets every element within `r` of a set element.
fn dilate_line(line: &[bool], r: usize) -> Vec<bool> {
    let n = line.len();
    let mut out = vec![false; n];
    let mut last = None; // most recent set index, scanning left to right
    for i in 0..n {
        if line[i] {
            last = Some(i);
        }
        out[i] = last.is_some_and(|j| i - j <= r);
    }
    let mut next = None;
    for i in (0..n).rev() {
        if line[i] {
            next = Some(i);
        }
        out[i] |= next.is_some_and(|j| j - i <= r);
    }
    out
}

/// Box blur of radius `r` over a binary mask, as 0..=65535 weights. Only in-bounds pixels
/// count, so a mask touching the crop edge stays fully opaque there.
fn feathered(mask: &GrayImage, r: u32) -> Vec<u16> {
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    let r = r as usize;
    // Horizontal window sums, then vertical sums of those.
    let mut rows = vec![0u32; w * h];
    for y in 0..h {
        let row = &mask.as_raw()[y * w..(y + 1) * w];
        let prefix: Vec<u32> = std::iter::once(0)
            .chain(row.iter().scan(0, |s, &v| {
                *s += u32::from(v != 0);
                Some(*s)
            }))
            .collect();
        for x in 0..w {
            let (a, b) = (x.saturating_sub(r), (x + r + 1).min(w));
            rows[y * w + x] = prefix[b] - prefix[a];
        }
    }
    let mut alpha = vec![0u16; w * h];
    let mut prefix = vec![0u32; h + 1];
    for x in 0..w {
        let nx = ((x + r + 1).min(w) - x.saturating_sub(r)) as u64;
        for y in 0..h {
            prefix[y + 1] = prefix[y] + rows[y * w + x];
        }
        for y in 0..h {
            let (a, b) = (y.saturating_sub(r), (y + r + 1).min(h));
            let area = nx * (b - a) as u64;
            let sum = u64::from(prefix[b] - prefix[a]);
            alpha[y * w + x] = ((sum * 65535 + area / 2) / area) as u16;
        }
    }
    alpha
}

fn alpha_bounds(alpha: &[u16], w: u32, h: u32) -> Option<Rect> {
    let mask = GrayImage::from_raw(w, h, alpha.iter().map(|&a| u8::from(a > 0)).collect())?;
    mask_bounds(&mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::tests as color_tests;

    /// Fills everything with one color and checks the inputs it gets.
    struct Flat {
        size: (u32, u32),
        calls: usize,
    }

    const FLAT: u8 = 200;

    impl Inpainter for Flat {
        fn input_size(&self) -> (u32, u32) {
            self.size
        }

        fn inpaint(
            &mut self,
            image: &RgbImage,
            mask: &GrayImage,
        ) -> Result<RgbImage, InpaintError> {
            assert_eq!(image.dimensions(), self.size);
            assert_eq!(mask.dimensions(), self.size);
            assert!(mask.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
            assert!(mask.pixels().any(|p| p.0[0] == 255));
            self.calls += 1;
            Ok(RgbImage::from_pixel(
                self.size.0,
                self.size.1,
                Rgb([FLAT; 3]),
            ))
        }
    }

    fn noise(w: u32, h: u32) -> Rgb16 {
        let mut state = 0x9e37_79b9_u32;
        Rgb16::from_fn(w, h, |_, _| {
            Rgb(std::array::from_fn(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u16
            }))
        })
    }

    fn disk(w: u32, h: u32, cx: i64, cy: i64, r: i64) -> Mask {
        Mask::from_fn(w, h, |x, y| {
            let (dx, dy) = (x as i64 - cx, y as i64 - cy);
            Luma([if dx * dx + dy * dy <= r * r { 255 } else { 0 }])
        })
    }

    /// Erases with [`Flat`] from a picture in `profile` and checks that only the blend
    /// changed, the mask got the fill (in the picture's space), and the patch undoes it all.
    fn check_erase(w: u32, h: u32, mask: Mask, model: (u32, u32), profile: Option<&[u8]>) {
        let before = noise(w, h);
        let mut image = before.clone();
        let settings = EraseSettings::default();
        let plan = ErasePlan::new(&image, &mask, model, &settings, profile).unwrap();
        let crop = plan.crop();
        assert!(crop.right() <= w && crop.bottom() <= h, "crop inside image");

        let mut flat = Flat {
            size: model,
            calls: 0,
        };
        let mut patch = erase(&mut image, &mask, &mut flat, &settings, profile)
            .unwrap()
            .unwrap();
        assert_eq!(flat.calls, 1);
        let rect = patch.rect;

        let near = dilate(&mask, plan.reach);
        let mut flat16 = Rgb16::from_pixel(1, 1, Rgb([u16::from(FLAT) * 257; 3]));
        if let Some(space) = ModelSpace::for_profile(profile) {
            space.to_picture(&mut flat16);
        }
        let flat16 = flat16.get_pixel(0, 0).0;
        for (x, y, p) in image.enumerate_pixels() {
            let changed = p != before.get_pixel(x, y);
            let inside = rect.x <= x && x < rect.right() && rect.y <= y && y < rect.bottom();
            if near.get_pixel(x, y).0[0] == 0 || !inside {
                assert!(!changed, "pixel ({x}, {y}) outside the blend changed");
            }
            if mask.get_pixel(x, y).0[0] != 0 {
                assert!(
                    p.0.iter().zip(flat16).all(|(&v, f)| v.abs_diff(f) <= 257),
                    "({x}, {y}) not filled: {p:?}, expected {flat16:?}"
                );
            }
        }
        // The patch holds what was replaced: swapped back, the picture is as it was.
        crate::swap_region(&mut image, rect, &mut patch.pixels);
        assert!(image == before, "the patch undoes the erase exactly");
    }

    #[test]
    fn small_mask_is_filled_and_nothing_else_changes() {
        check_erase(1200, 900, disk(1200, 900, 600, 400, 40), (800, 800), None);
    }

    #[test]
    fn large_mask_scales_down_to_the_model() {
        check_erase(
            1500,
            1000,
            disk(1500, 1000, 700, 500, 450),
            (200, 200),
            None,
        );
    }

    #[test]
    fn mask_at_the_edge_of_a_small_image() {
        check_erase(300, 200, disk(300, 200, 0, 199, 30), (800, 800), None);
    }

    /// Linear and very wide pictures go to the model converted, and still nothing outside the
    /// blend changes.
    #[test]
    fn converted_pictures_keep_every_other_pixel() {
        for profile in [color_tests::linear_srgb(), color_tests::prophoto()] {
            check_erase(
                600,
                500,
                disk(600, 500, 300, 250, 30),
                (256, 256),
                Some(&profile),
            );
            check_erase(
                300,
                200,
                disk(300, 200, 0, 199, 30),
                (256, 256),
                Some(&profile),
            );
        }
    }

    /// Gives back what it gets, and keeps a copy.
    struct Mirror {
        seen: Option<RgbImage>,
    }

    impl Inpainter for Mirror {
        fn input_size(&self) -> (u32, u32) {
            (64, 64)
        }

        fn inpaint(&mut self, image: &RgbImage, _: &GrayImage) -> Result<RgbImage, InpaintError> {
            self.seen = Some(image.clone());
            Ok(image.clone())
        }
    }

    /// A linear picture reaches the model as P3 (its mid-gray as 128, not 55) and the fill
    /// comes back linear; a picture in an ordinary profile goes as it is.
    #[test]
    fn a_linear_picture_reaches_the_model_in_p3() {
        let linear_mid = 14025; // sRGB's 0.5, linear
        let picture = Rgb16::from_pixel(64, 64, Rgb([linear_mid; 3]));
        let mask = disk(64, 64, 32, 32, 8);
        let srgb = color_tests::rgb_profile(
            (0.3127, 0.3290),
            [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)],
            2.2,
        );
        for (profile, seen) in [(color_tests::linear_srgb(), 128), (srgb, 55)] {
            let mut image = picture.clone();
            let mut mirror = Mirror { seen: None };
            erase(
                &mut image,
                &mask,
                &mut mirror,
                &EraseSettings::default(),
                Some(&profile),
            )
            .unwrap()
            .unwrap();
            let input = mirror.seen.unwrap();
            assert!(
                input
                    .pixels()
                    .all(|p| p.0.iter().all(|v| v.abs_diff(seen) <= 1)),
                "{:?}",
                input.get_pixel(32, 32)
            );
            let center = image.get_pixel(32, 32).0;
            assert!(
                center.iter().all(|v| v.abs_diff(linear_mid) < 150),
                "{center:?}"
            );
        }
    }

    #[test]
    fn empty_mask_does_nothing() {
        let mut image = noise(50, 40);
        let before = image.clone();
        let mut flat = Flat {
            size: (32, 32),
            calls: 0,
        };
        assert!(
            erase(
                &mut image,
                &Mask::new(50, 40),
                &mut flat,
                &EraseSettings::default(),
                None
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(flat.calls, 0);
        assert_eq!(image, before);
    }

    /// Fills with a color, whatever it gets.
    struct Colored;

    impl Inpainter for Colored {
        fn input_size(&self) -> (u32, u32) {
            (64, 64)
        }

        fn inpaint(&mut self, _: &RgbImage, _: &GrayImage) -> Result<RgbImage, InpaintError> {
            Ok(RgbImage::from_pixel(64, 64, Rgb([200, 120, 40])))
        }
    }

    #[test]
    fn a_gray_crop_gets_a_gray_fill() {
        let gray = Rgb16::from_fn(100, 80, |x, y| Rgb([(x * 300 + y * 7) as u16; 3]));
        let mask = disk(100, 80, 50, 40, 10);
        let mut image = gray.clone();
        erase(
            &mut image,
            &mask,
            &mut Colored,
            &EraseSettings::default(),
            None,
        )
        .unwrap();
        assert!(image.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));
        assert_ne!(image, gray, "filled");

        // A crop with any color in it keeps the model's colors.
        let mut color = gray.clone();
        color.put_pixel(30, 40, Rgb([1, 2, 3]));
        erase(
            &mut color,
            &mask,
            &mut Colored,
            &EraseSettings::default(),
            None,
        )
        .unwrap();
        let center = color.get_pixel(50, 40).0;
        assert!(center[0] > center[2], "{center:?}");
    }

    #[test]
    fn feather_is_opaque_inside_and_reaches_two_radii() {
        let mut mask = GrayImage::new(41, 1);
        mask.put_pixel(20, 0, Luma([255]));
        // As in the pipeline: grow by 3, then feather by 3 around the grown mask.
        let grown = dilate(&mask, 3);
        let alpha = feathered(&dilate(&grown, 3), 3);
        assert!((17..=23).all(|x| alpha[x] == 65535));
        assert!(alpha[11] > 0 && alpha[16] < 65535 && alpha[24] < 65535 && alpha[29] > 0);
        assert!(alpha[..11].iter().chain(&alpha[30..]).all(|&a| a == 0));
    }
}
