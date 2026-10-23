// SPDX-License-Identifier: GPL-3.0-or-later

//! Making a whole picture larger with a super-resolution model, tile by tile.
//!
//! The model takes a fixed-size tile and gives it back `scale` times larger. The picture is
//! cut into tiles whose cores cover it exactly once; each tile also reads [`OVERLAP`] pixels of
//! context on every side (mirrored at the picture's edges), and only its core is written, so the
//! tiles join without seams.
//!
//! Values go to the model as floats from 0 to 1, so 16-bit pictures keep their precision. As for
//! Erase, they go in the picture's own encoding unless its profile is linear or very wide: then
//! each tile is converted to Display P3 for the model and back (`ModelSpace`), and colors
//! beyond P3 are clipped. A grayscale picture comes back grayscale; an alpha channel is scaled
//! with an ordinary filter. Under straight alpha, the colors of fully transparent pixels are
//! hidden and can be anything, so the model gets the nearest visible colors there instead;
//! under premultiplied alpha, no color comes back above its alpha.
//!
//! 2× and 4× are the models' own output. Any other factor runs the next model up and scales each
//! tile's output down to the factor (Lanczos 3), from the exact part of it that its core covers:
//! the filter reaches into the tile's context, so the tiles still join without seams.

use std::fmt;

use fast_image_resize::images::TypedImage;
use fast_image_resize::pixels::{U16, U16x3};
use fast_image_resize::{FilterType as Filter, ResizeAlg, ResizeOptions, Resizer};
use image::Rgb;

use crate::Rgb16;
use crate::color::ModelSpace;
use crate::picture::gray_value;

pub type UpscaleError = Box<dyn std::error::Error + Send + Sync>;

/// The largest result, in pixels: about 1.5 GB at 16 bits per channel.
pub const MAX_PIXELS: u64 = 256_000_000;

/// The longest side of a result, JPEG's limit.
pub const MAX_SIDE: u32 = 65_535;

/// Input pixels of context each tile reads on every side of its core.
pub const OVERLAP: u32 = 16;

/// How many times larger the result is, side by side. Kept in quarter steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Factor(u8);

impl Factor {
    pub const X2: Factor = Factor(8);
    pub const X4: Factor = Factor(16);
    pub const MIN: Factor = Factor(5);
    pub const MAX: Factor = Factor::X4;

    /// The factors the models make as they are.
    pub const EXACT: [Factor; 2] = [Factor::X2, Factor::X4];

    /// A factor of `quarters` / 4, if it is one offered: 1.25× to 4×.
    pub fn from_quarters(quarters: u32) -> Option<Factor> {
        (u32::from(Self::MIN.0)..=u32::from(Self::MAX.0))
            .contains(&quarters)
            .then_some(Factor(quarters as u8))
    }

    pub fn quarters(self) -> u32 {
        u32::from(self.0)
    }

    /// The scale of the model that makes this factor.
    pub fn model_scale(self) -> u32 {
        if self.0 <= 8 { 2 } else { 4 }
    }

    /// The model's output is the result as it is, with nothing resampled.
    pub fn is_exact(self) -> bool {
        u32::from(self.0) == 4 * self.model_scale()
    }

    /// A length of `n` pixels, scaled.
    pub fn of(self, n: u32) -> u64 {
        (u64::from(n) * u64::from(self.0) + 2) / 4
    }
}

impl fmt::Display for Factor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 4;
        match self.0 % 4 {
            0 => write!(f, "{whole}×"),
            2 => write!(f, "{whole}.5×"),
            quarter => write!(f, "{whole}.{}×", quarter * 25),
        }
    }
}

/// The size of the result of upscaling a `width` × `height` picture by `factor`, or why it
/// would be too large, said for the user.
pub fn result_size(width: u32, height: u32, factor: Factor) -> Result<(u32, u32), String> {
    let (w, h) = (factor.of(width), factor.of(height));
    let side = w.max(h);
    if side > u64::from(MAX_SIDE) {
        return Err(format!(
            "{factor} would make it {} px long, more than the {} px limit",
            grouped(side),
            grouped(u64::from(MAX_SIDE))
        ));
    }
    if w * h > MAX_PIXELS {
        return Err(format!(
            "{factor} would make it {} MP, more than the {} MP limit",
            (w * h).div_ceil(1_000_000),
            MAX_PIXELS / 1_000_000
        ));
    }
    Ok((w as u32, h as u32))
}

/// The largest factor whose result is within the limits for a `width` × `height` picture, or
/// `None` when not even the smallest is.
pub fn largest_factor(width: u32, height: u32) -> Option<Factor> {
    (Factor::MIN.quarters()..=Factor::MAX.quarters())
        .rev()
        .filter_map(Factor::from_quarters)
        .find(|&f| result_size(width, height, f).is_ok())
}

/// 65535 as "65,535".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A super-resolution model that works on tiles of one size.
pub trait Upscaler {
    /// How many times larger each tile comes back.
    fn scale(&self) -> u32;

    /// The input tile's side in pixels; the output's is `tile_size() * scale()`.
    fn tile_size(&self) -> u32;

    /// Upscales one tile: planar RGB (every red value, then green, then blue), `3 * tile²`
    /// values from 0 to 1. Returns the same layout at the output size.
    fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError>;
}

/// A picture's alpha channel: one value per pixel.
#[derive(Clone, Copy)]
pub struct Alpha<'a> {
    pub values: &'a [u16],
    /// The colors are premultiplied by it (a TIFF's associated alpha).
    pub premultiplied: bool,
}

/// An upscaled picture.
pub struct Upscaled {
    pub pixels: Rgb16,
    /// One value per pixel, when the picture has an alpha channel.
    pub alpha: Option<Vec<u16>>,
}

/// Upscales `pixels` (and `alpha`) by `factor`. `profile` is the picture's ICC profile, which
/// decides the space the model sees. `progress` hears (tiles done, tiles in all) before the
/// first tile and after each one; when it returns false, the upscale stops and gives
/// `Ok(None)`.
pub fn upscale(
    pixels: &Rgb16,
    alpha: Option<Alpha<'_>>,
    factor: Factor,
    upscaler: &mut dyn Upscaler,
    profile: Option<&[u8]>,
    progress: &mut dyn FnMut(usize, usize) -> bool,
) -> Result<Option<Upscaled>, UpscaleError> {
    let (w, h) = pixels.dimensions();
    let (scale, tile) = (upscaler.scale(), upscaler.tile_size());
    if w == 0 || h == 0 {
        return Err("the picture is empty".into());
    }
    if scale != factor.model_scale() {
        return Err(format!("a {scale}× model can't upscale by {factor}").into());
    }
    if tile <= 2 * OVERLAP {
        return Err(format!("a {tile} px tile leaves no core inside its context").into());
    }
    if alpha.is_some_and(|a| a.values.len() != w as usize * h as usize) {
        return Err("the alpha channel doesn't match the picture's size".into());
    }
    let (rw, rh) = result_size(w, h, factor)?;

    let len = rw as usize * rh as usize;
    let mut out = reserve(len * 3)?;
    let mut out_alpha = match alpha {
        Some(_) => Some(reserve(len)?),
        None => None,
    };

    let space = ModelSpace::for_profile(profile);
    let gray = pixels
        .as_raw()
        .as_chunks::<3>()
        .0
        .iter()
        .all(|[r, g, b]| r == g && g == b);
    let core = tile - 2 * OVERLAP;
    let total = w.div_ceil(core) as usize * h.div_ceil(core) as usize;
    if !progress(0, total) {
        return Ok(None);
    }

    let out_tile = tile * scale;
    let plane = (tile * tile) as usize;
    let out_plane = (out_tile * out_tile) as usize;
    let mut input = vec![0f32; 3 * plane];
    let mut window = Rgb16::new(tile, tile);
    let mut window_alpha = vec![0u16; plane];
    let mut resizer = Resizer::new();
    // Where a picture position lands in the result: the models' own scale, or one consistent
    // mapping for the whole picture, so that neighbouring tiles meet exactly.
    let exact = factor.is_exact();
    let to_result = |x: u32, len: u32, result: u32| -> u32 {
        if exact {
            x * scale
        } else {
            ((2 * u64::from(x) * u64::from(result) + u64::from(len)) / (2 * u64::from(len))) as u32
        }
    };
    let mut done = 0;
    for y0 in (0..h).step_by(core as usize) {
        let ys = indices(y0, tile, h);
        for x0 in (0..w).step_by(core as usize) {
            let xs = indices(x0, tile, w);
            for (ty, &sy) in ys.iter().enumerate() {
                for (tx, &sx) in xs.iter().enumerate() {
                    window.put_pixel(tx as u32, ty as u32, *pixels.get_pixel(sx, sy));
                    if let Some(alpha) = alpha {
                        window_alpha[ty * tile as usize + tx] =
                            alpha.values[sy as usize * w as usize + sx as usize];
                    }
                }
            }
            if alpha.is_some_and(|a| !a.premultiplied) {
                fill_transparent(&mut window, &window_alpha);
            }
            if let Some(space) = &space {
                space.to_model(&mut window);
            }
            for (i, p) in window.pixels().enumerate() {
                for c in 0..3 {
                    input[c * plane + i] = f32::from(p.0[c]) / 65535.0;
                }
            }

            let result = upscaler.upscale(&input)?;
            if result.len() != 3 * out_plane {
                return Err(format!(
                    "the model returned {} values, not {}",
                    result.len(),
                    3 * out_plane
                )
                .into());
            }

            // The core, and where it goes in the result.
            let (x1, y1) = (x0 + core.min(w - x0), y0 + core.min(h - y0));
            let (ox, oy) = (to_result(x0, w, rw), to_result(y0, h, rh));
            let (cw, ch) = (to_result(x1, w, rw) - ox, to_result(y1, h, rh) - oy);
            let skip = OVERLAP * scale;
            // The core's part of the tile, in the tile's input pixels: its left, top, width and
            // height (picture positions over the result's, from the window's corner).
            let source = [
                f64::from(ox) * f64::from(w) / f64::from(rw) - (f64::from(x0) - f64::from(OVERLAP)),
                f64::from(oy) * f64::from(h) / f64::from(rh) - (f64::from(y0) - f64::from(OVERLAP)),
                f64::from(cw) * f64::from(w) / f64::from(rw),
                f64::from(ch) * f64::from(h) / f64::from(rh),
            ];
            let mut part = if exact {
                Rgb16::from_fn(cw, ch, |x, y| {
                    let at = ((skip + y) * out_tile + skip + x) as usize;
                    Rgb([0, 1, 2].map(|c| to_u16(result[c * out_plane + at])))
                })
            } else {
                let output: Vec<U16x3> = (0..out_plane)
                    .map(|i| U16x3::new([0, 1, 2].map(|c| to_u16(result[c * out_plane + i]))))
                    .collect();
                let output = TypedImage::from_pixels(out_tile, out_tile, output)?;
                let in_output = source.map(|v| v * f64::from(scale));
                let part = resample(&mut resizer, &output, (cw, ch), in_output, Filter::Lanczos3)?;
                let values = part.pixels().iter().flat_map(|p| p.0).collect();
                Rgb16::from_raw(cw, ch, values).ok_or("the tile has the wrong size")?
            };
            if let Some(space) = &space {
                space.to_picture(&mut part);
            }
            if gray {
                for p in part.pixels_mut() {
                    p.0 = [gray_value(p.0); 3];
                }
            }
            for (row, values) in part.as_raw().chunks_exact(cw as usize * 3).enumerate() {
                let at = ((oy as usize + row) * rw as usize + ox as usize) * 3;
                out[at..at + values.len()].copy_from_slice(values);
            }

            if let (Some(alpha), Some(out_alpha)) = (alpha, out_alpha.as_mut()) {
                let window = window_alpha.iter().map(|&a| U16::new(a)).collect();
                let window = TypedImage::from_pixels(tile, tile, window)?;
                let part = resample(&mut resizer, &window, (cw, ch), source, Filter::CatmullRom)?;
                let part: Vec<u16> = part.pixels().iter().map(|p| p.0).collect();
                for (row, values) in part.chunks_exact(cw as usize).enumerate() {
                    let at = (oy as usize + row) * rw as usize + ox as usize;
                    out_alpha[at..at + values.len()].copy_from_slice(values);
                    // Premultiplied, a color above its alpha is light that isn't there (a
                    // sharpened edge, say); it would show as a fringe.
                    if alpha.premultiplied {
                        let colors = &mut out[at * 3..(at + values.len()) * 3];
                        for (color, &a) in colors.as_chunks_mut::<3>().0.iter_mut().zip(values) {
                            *color = color.map(|c| c.min(a));
                        }
                    }
                }
            }

            done += 1;
            if !progress(done, total) {
                return Ok(None);
            }
        }
    }
    let pixels = Rgb16::from_raw(rw, rh, out).ok_or("the result has the wrong size")?;
    Ok(Some(Upscaled {
        pixels,
        alpha: out_alpha,
    }))
}

/// `size` pixels resampled from the `[left, top, width, height]` part of `image`, in its pixels.
/// The filter reads beyond that part, into the rest of `image`.
fn resample<P: fast_image_resize::PixelTrait>(
    resizer: &mut Resizer,
    image: &TypedImage<P>,
    (width, height): (u32, u32),
    [left, top, w, h]: [f64; 4],
    filter: Filter,
) -> Result<TypedImage<'static, P>, UpscaleError> {
    let mut part = TypedImage::new(width, height);
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(filter))
        .crop(left, top, w, h);
    resizer.resize_typed(image, &mut part, &options)?;
    Ok(part)
}

/// Gives the fully transparent pixels of a tile (straight alpha) the colors of the nearest
/// visible ones, ring by ring outwards for up to [`OVERLAP`] pixels, each the mean of its
/// neighbours known by then. Their own colors are hidden and can be anything (often black):
/// a model would blur them into the visible edge. Pixels farther out keep their colors.
fn fill_transparent(window: &mut Rgb16, alpha: &[u16]) {
    if !alpha.contains(&0) || alpha.iter().all(|&a| a == 0) {
        return;
    }
    let (w, h) = (window.width() as i64, window.height() as i64);
    let mut known: Vec<bool> = alpha.iter().map(|&a| a != 0).collect();
    for _ in 0..OVERLAP {
        let mut ring = Vec::new();
        for y in 0..h {
            for x in 0..w {
                if known[(y * w + x) as usize] {
                    continue;
                }
                let (mut sum, mut n) = ([0u32; 3], 0);
                for (nx, ny) in (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (x + dx, y + dy))) {
                    if (0..w).contains(&nx) && (0..h).contains(&ny) && known[(ny * w + nx) as usize]
                    {
                        let p = window.get_pixel(nx as u32, ny as u32).0;
                        (0..3).for_each(|c| sum[c] += u32::from(p[c]));
                        n += 1;
                    }
                }
                if n > 0 {
                    ring.push((x, y, sum.map(|s| ((s + n / 2) / n) as u16)));
                }
            }
        }
        if ring.is_empty() {
            break;
        }
        for (x, y, color) in ring {
            window.put_pixel(x as u32, y as u32, Rgb(color));
            known[(y * w + x) as usize] = true;
        }
    }
}

/// A zeroed buffer of `len` values, or an error when there isn't the memory for it.
fn reserve(len: usize) -> Result<Vec<u16>, UpscaleError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| "there isn't enough memory for a picture that large")?;
    buffer.resize(len, 0);
    Ok(buffer)
}

/// The picture positions a tile starting at `start` reads: its core and [`OVERLAP`] pixels on
/// either side, mirrored beyond the picture's edges.
fn indices(start: u32, tile: u32, len: u32) -> Vec<u32> {
    let first = i64::from(start) - i64::from(OVERLAP);
    (0..i64::from(tile))
        .map(|i| mirror(first + i, len))
        .collect()
}

/// Position `i` reflected into `0..n` without repeating the edge pixel (… 2 1 0 1 2 …), for any
/// `i`, however far outside.
fn mirror(i: i64, n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    let period = 2 * (i64::from(n) - 1);
    let m = i.rem_euclid(period);
    (if m >= i64::from(n) { period - m } else { m }) as u32
}

/// A model's value as 16 bits; anything outside 0 to 1 (or not a number) is clipped.
fn to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replicates every pixel `scale` × `scale` times: a model whose output depends on one
    /// input pixel only, so tiling it must change nothing.
    struct Nearest {
        scale: u32,
        tile: u32,
        calls: usize,
    }

    impl Nearest {
        fn new(scale: u32, tile: u32) -> Self {
            Nearest {
                scale,
                tile,
                calls: 0,
            }
        }
    }

    /// `rgb` upscaled `s` times, each output pixel made by `f` from the tile and its position.
    fn replicate(
        rgb: &[f32],
        t: usize,
        s: usize,
        f: impl Fn(&[f32], usize, usize) -> f32,
    ) -> Vec<f32> {
        let o = t * s;
        let mut out = vec![0.0; 3 * o * o];
        for c in 0..3 {
            let plane = &rgb[c * t * t..(c + 1) * t * t];
            for y in 0..o {
                for x in 0..o {
                    out[c * o * o + y * o + x] = f(plane, x / s, y / s);
                }
            }
        }
        out
    }

    impl Upscaler for Nearest {
        fn scale(&self) -> u32 {
            self.scale
        }

        fn tile_size(&self) -> u32 {
            self.tile
        }

        fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
            self.calls += 1;
            let t = self.tile as usize;
            Ok(replicate(rgb, t, self.scale as usize, |p, x, y| {
                p[y * t + x]
            }))
        }
    }

    /// The mean of each pixel's 3 × 3 neighbourhood, then replicated: a model that sees one
    /// pixel of context, well within the overlap.
    struct Blur {
        scale: u32,
        tile: u32,
    }

    /// The 3 × 3 mean at (x, y), with neighbours found by `at`, in a fixed order.
    fn mean(at: impl Fn(i64, i64) -> f32) -> f32 {
        let mut sum = 0.0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                sum += at(dx, dy);
            }
        }
        sum / 9.0
    }

    impl Upscaler for Blur {
        fn scale(&self) -> u32 {
            self.scale
        }

        fn tile_size(&self) -> u32 {
            self.tile
        }

        fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
            let t = self.tile as usize;
            Ok(replicate(rgb, t, self.scale as usize, |p, x, y| {
                mean(|dx, dy| {
                    let xx = (x as i64 + dx).clamp(0, t as i64 - 1) as usize;
                    let yy = (y as i64 + dy).clamp(0, t as i64 - 1) as usize;
                    p[yy * t + xx]
                })
            }))
        }
    }

    /// Nearest, with red raised: a model that would tint a gray picture.
    struct Tint(Nearest);

    impl Upscaler for Tint {
        fn scale(&self) -> u32 {
            self.0.scale
        }

        fn tile_size(&self) -> u32 {
            self.0.tile
        }

        fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
            let mut out = self.0.upscale(rgb)?;
            let plane = out.len() / 3;
            for v in &mut out[..plane] {
                *v = (*v + 0.05).min(1.0);
            }
            Ok(out)
        }
    }

    fn picture(w: u32, h: u32) -> Rgb16 {
        Rgb16::from_fn(w, h, |x, y| {
            Rgb([
                ((x * 977 + y * 131) % 65536) as u16,
                ((x * 37 + y * 2011) % 65536) as u16,
                ((x ^ y) * 257 % 65536) as u16,
            ])
        })
    }

    /// Upscales with straight alpha, if any.
    fn run(
        pixels: &Rgb16,
        alpha: Option<&[u16]>,
        factor: Factor,
        upscaler: &mut dyn Upscaler,
    ) -> Result<Option<Upscaled>, UpscaleError> {
        let alpha = alpha.map(|values| Alpha {
            values,
            premultiplied: false,
        });
        upscale(pixels, alpha, factor, upscaler, None, &mut |_, _| true)
    }

    #[test]
    fn mirror_reflects_without_repeating_the_edge() {
        let row: Vec<u32> = (-4..9).map(|i| mirror(i, 5)).collect();
        assert_eq!(row, [4, 3, 2, 1, 0, 1, 2, 3, 4, 3, 2, 1, 0]);
        assert_eq!(mirror(-1000, 1), 0);
        assert_eq!(mirror(7, 2), 1);
        assert!((-500..500).all(|i| mirror(i, 3) < 3));
    }

    #[test]
    fn the_cores_cover_each_pixel_once() {
        let core = 16;
        for len in [1, 5, 15, 16, 17, 1000] {
            let mut seen = vec![0; len as usize];
            for start in (0..len).step_by(core as usize) {
                for x in start..(start + core).min(len) {
                    seen[x as usize] += 1;
                }
                let read = indices(start, core + 2 * OVERLAP, len);
                assert_eq!(
                    read[OVERLAP as usize], start,
                    "the core starts after the context"
                );
            }
            assert!(seen.iter().all(|&n| n == 1), "{len}: {seen:?}");
        }
    }

    /// A model that ignores context gives exactly the replicated picture, whatever the size
    /// and however many tiles it takes.
    #[test]
    fn tiles_join_without_a_trace() {
        for factor in Factor::EXACT {
            let s = factor.model_scale();
            for (w, h) in [(1, 1), (5, 3), (47, 33), (100, 37), (16, 81)] {
                let pixels = picture(w, h);
                let mut model = Nearest::new(s, 48);
                let out = run(&pixels, None, factor, &mut model).unwrap().unwrap();
                assert_eq!(out.pixels.dimensions(), (w * s, h * s));
                for (x, y, p) in out.pixels.enumerate_pixels() {
                    assert_eq!(
                        p,
                        pixels.get_pixel(x / s, y / s),
                        "{w}×{h} {factor} at {x},{y}"
                    );
                }
                assert_eq!(model.calls as u32, w.div_ceil(16) * h.div_ceil(16));
            }
        }
    }

    /// A model that sees context gives what it gives on the whole picture at once, mirrored at
    /// the edges the same way: the overlap holds all the context it needs.
    #[test]
    fn tiles_match_the_whole_picture_for_a_model_that_sees_context() {
        let (w, h) = (70u32, 41u32);
        let pixels = picture(w, h);
        let out = run(&pixels, None, Factor::X2, &mut Blur { scale: 2, tile: 48 })
            .unwrap()
            .unwrap();
        for (x, y, p) in out.pixels.enumerate_pixels() {
            let (px, py) = (i64::from(x / 2), i64::from(y / 2));
            let want = [0, 1, 2].map(|c| {
                to_u16(mean(|dx, dy| {
                    let at = pixels.get_pixel(mirror(px + dx, w), mirror(py + dy, h));
                    f32::from(at.0[c]) / 65535.0
                }))
            });
            assert_eq!(p.0, want, "at {x},{y}");
        }
    }

    #[test]
    fn a_gray_picture_stays_gray() {
        let gray = Rgb16::from_fn(40, 30, |x, y| Rgb([(x * 1000 + y * 300) as u16; 3]));
        let out = run(&gray, None, Factor::X2, &mut Tint(Nearest::new(2, 48)))
            .unwrap()
            .unwrap();
        assert!(out.pixels.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));

        let mut color = gray.clone();
        color.put_pixel(3, 3, Rgb([100, 200, 300]));
        let out = run(&color, None, Factor::X2, &mut Tint(Nearest::new(2, 48)))
            .unwrap()
            .unwrap();
        assert!(
            out.pixels.pixels().any(|p| p[0] != p[1]),
            "a color picture keeps its tint"
        );
    }

    #[test]
    fn alpha_is_scaled_with_the_picture() {
        let (w, h) = (60u32, 35u32);
        let pixels = picture(w, h);
        let constant = vec![40000u16; (w * h) as usize];
        let out = run(
            &pixels,
            Some(&constant),
            Factor::X4,
            &mut Nearest::new(4, 48),
        )
        .unwrap()
        .unwrap();
        let alpha = out.alpha.unwrap();
        assert_eq!(alpha.len(), (w * 4 * h * 4) as usize);
        assert!(alpha.iter().all(|&a| a.abs_diff(40000) <= 1));

        // Inside the picture, the tiles' alpha is the whole picture's resized at once.
        let ramp: Vec<u16> = (0..w * h)
            .map(|i| ((i % w) * 1000 + (i / w) * 300) as u16)
            .collect();
        let out = run(&pixels, Some(&ramp), Factor::X2, &mut Nearest::new(2, 48))
            .unwrap()
            .unwrap();
        let ramp = TypedImage::from_pixels(w, h, ramp.into_iter().map(U16::new).collect()).unwrap();
        let whole = [0.0, 0.0, f64::from(w), f64::from(h)];
        let reference = resample(
            &mut Resizer::new(),
            &ramp,
            (w * 2, h * 2),
            whole,
            Filter::CatmullRom,
        )
        .unwrap();
        let alpha = out.alpha.unwrap();
        for y in 4..h * 2 - 4 {
            for x in 4..w * 2 - 4 {
                let i = (y * w * 2 + x) as usize;
                let (got, want) = (alpha[i], reference.pixels()[i].0);
                assert!(got.abs_diff(want) <= 1, "at {x},{y}: {got} against {want}");
            }
        }
    }

    /// Straight alpha: the black hidden under the transparent right half doesn't reach the
    /// visible white left half through a model that blurs.
    #[test]
    fn hidden_colors_dont_bleed_into_visible_edges() {
        let (w, h) = (40u32, 30u32);
        let pixels = Rgb16::from_fn(w, h, |x, _| Rgb([if x < 20 { 65535 } else { 0 }; 3]));
        let alpha: Vec<u16> = (0..w * h)
            .map(|i| if i % w < 20 { 65535 } else { 0 })
            .collect();
        let mut model = Blur { scale: 2, tile: 48 };
        let straight = Alpha {
            values: &alpha,
            premultiplied: false,
        };
        let out = upscale(
            &pixels,
            Some(straight),
            Factor::X2,
            &mut model,
            None,
            &mut |_, _| true,
        )
        .unwrap()
        .unwrap();
        for (x, y, p) in out.pixels.enumerate_pixels() {
            if x < 40 {
                assert_eq!(p.0, [65535; 3], "at {x},{y}");
            }
        }
        // Premultiplied colors under alpha 0 are black already, and stay as they are.
        let premultiplied = Alpha {
            values: &alpha,
            premultiplied: true,
        };
        let mut model = Blur { scale: 2, tile: 48 };
        let out = upscale(
            &pixels,
            Some(premultiplied),
            Factor::X2,
            &mut model,
            None,
            &mut |_, _| true,
        )
        .unwrap()
        .unwrap();
        assert!(
            out.pixels.get_pixel(39, 10).0[0] < 65535,
            "blurred into the edge"
        );
        // Where the alpha is whole, the clamp doesn't hide it: a third of the 3 x 3 is black.
        assert_eq!(out.pixels.get_pixel(38, 10).0, [43690; 3]);
    }

    /// The mean of each pixel's (2 × `radius` + 1)² neighbourhood, then replicated: a model
    /// that sees further than one pixel, as real ones do.
    struct Wide {
        scale: u32,
        tile: u32,
        radius: i64,
    }

    impl Upscaler for Wide {
        fn scale(&self) -> u32 {
            self.scale
        }

        fn tile_size(&self) -> u32 {
            self.tile
        }

        fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
            let (t, r) = (i64::from(self.tile), self.radius);
            let blurred: Vec<f32> = (0..3 * t * t)
                .map(|i| {
                    let (c, y, x) = (i / (t * t), i / t % t, i % t);
                    let mut sum = 0.0;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let (xx, yy) = ((x + dx).clamp(0, t - 1), (y + dy).clamp(0, t - 1));
                            sum += rgb[(c * t * t + yy * t + xx) as usize];
                        }
                    }
                    sum / ((2 * r + 1) * (2 * r + 1)) as f32
                })
                .collect();
            let t = t as usize;
            Ok(replicate(&blurred, t, self.scale as usize, |p, x, y| {
                p[y * t + x]
            }))
        }
    }

    /// Straight alpha, and a model that sees 15 pixels around: the hidden colors are filled far
    /// enough out that none of them reaches a visible pixel, here 12 rings into 24 x 24 holes.
    #[test]
    fn hidden_colors_stay_hidden_from_a_model_that_sees_far() {
        let (w, h) = (96u32, 72u32);
        let visible = |x: u32, y: u32| (x / 24 + y / 24).is_multiple_of(2);
        let pixels = Rgb16::from_fn(w, h, |x, y| {
            Rgb(if visible(x, y) {
                [50000; 3]
            } else {
                [0, 65535, 0]
            })
        });
        let alpha: Vec<u16> = (0..w * h)
            .map(|i| if visible(i % w, i / w) { 65535 } else { 0 })
            .collect();
        let straight = Alpha {
            values: &alpha,
            premultiplied: false,
        };
        let mut model = Wide {
            scale: 2,
            tile: 64,
            radius: 15,
        };
        let out = upscale(
            &pixels,
            Some(straight),
            Factor::X2,
            &mut model,
            None,
            &mut |_, _| true,
        )
        .unwrap()
        .unwrap();
        for (x, y, p) in out.pixels.enumerate_pixels() {
            if visible(x / 2, y / 2) {
                assert_eq!(p.0, [50000; 3], "at {x},{y}");
            }
        }
    }

    /// Straight alpha across tile borders: a model that sees context gives the visible pixels
    /// in tiles what it gives them in one tile holding the whole picture. Each tile fills its
    /// hidden colors from its own window, and the fill doesn't depend on where the window is.
    #[test]
    fn straight_alpha_fills_join_across_tiles() {
        let (w, h) = (70u32, 41u32);
        let visible =
            |x: u32, y: u32| (x / 5 + y / 7).is_multiple_of(3) || (x + y).is_multiple_of(11);
        let pixels = Rgb16::from_fn(w, h, |x, y| {
            Rgb(if visible(x, y) {
                [(x * 900) as u16, (y * 1500) as u16, 30000]
            } else {
                [65535, (x * y * 13 % 65536) as u16, 65535]
            })
        });
        let alpha: Vec<u16> = (0..w * h)
            .map(|i| if visible(i % w, i / w) { 40000 } else { 0 })
            .collect();
        let run = |tile| {
            let straight = Alpha {
                values: &alpha,
                premultiplied: false,
            };
            let mut model = Blur { scale: 2, tile };
            upscale(
                &pixels,
                Some(straight),
                Factor::X2,
                &mut model,
                None,
                &mut |_, _| true,
            )
            .unwrap()
            .unwrap()
            .pixels
        };
        let (tiled, whole) = (run(48), run(w.max(h) + 2 * OVERLAP));
        for (x, y, p) in tiled.enumerate_pixels() {
            if visible(x / 2, y / 2) {
                assert_eq!(p, whole.get_pixel(x, y), "at {x},{y}");
            }
        }
    }

    /// Premultiplied gray stays gray, and within its alpha, however the model tints it.
    #[test]
    fn premultiplied_gray_stays_gray_within_its_alpha() {
        let (w, h) = (45u32, 30u32);
        let alpha: Vec<u16> = (0..w * h)
            .map(|i| ((i % w) * 1400 + (i / w) * 50) as u16)
            .collect();
        let pixels = Rgb16::from_fn(w, h, |x, y| Rgb([alpha[(y * w + x) as usize] / 2; 3]));
        for quarters in [8, 10, 16] {
            let factor = Factor::from_quarters(quarters).unwrap();
            let premultiplied = Alpha {
                values: &alpha,
                premultiplied: true,
            };
            let mut model = Tint(Nearest::new(factor.model_scale(), 48));
            let out = upscale(
                &pixels,
                Some(premultiplied),
                factor,
                &mut model,
                None,
                &mut |_, _| true,
            )
            .unwrap()
            .unwrap();
            let alpha = out.alpha.unwrap();
            let mut clamped = 0;
            for (p, &a) in out.pixels.pixels().zip(&alpha) {
                assert!(p[0] == p[1] && p[1] == p[2], "{factor}: {:?}", p.0);
                assert!(p[0] <= a, "{factor}: {} above {a}", p[0]);
                clamped += usize::from(p[0] == a);
            }
            assert!(
                clamped > 0,
                "{factor}: the tint went above the alpha somewhere"
            );
        }
    }

    /// Premultiplied alpha: a model that overshoots (here, raising red) never leaves a color
    /// above its alpha, at the models' own factors and between them.
    #[test]
    fn premultiplied_colors_stay_within_their_alpha() {
        let (w, h) = (50u32, 37u32);
        let alpha: Vec<u16> = (0..w * h)
            .map(|i| ((i % w) * 1300 + (i / w) * 17) as u16)
            .collect();
        let pixels = Rgb16::from_fn(w, h, |x, y| {
            let a = alpha[(y * w + x) as usize];
            Rgb([a, a / 2, a / 3])
        });
        for quarters in [8, 10, 16] {
            let factor = Factor::from_quarters(quarters).unwrap();
            let over = |premultiplied| {
                let alpha = Alpha {
                    values: &alpha,
                    premultiplied,
                };
                let mut model = Tint(Nearest::new(factor.model_scale(), 48));
                let out = upscale(
                    &pixels,
                    Some(alpha),
                    factor,
                    &mut model,
                    None,
                    &mut |_, _| true,
                )
                .unwrap()
                .unwrap();
                let alpha = out.alpha.unwrap();
                out.pixels
                    .pixels()
                    .zip(&alpha)
                    .filter(|(p, a)| p.0.iter().any(|c| c > a))
                    .count()
            };
            assert!(over(false) > 0, "{factor}: the model overshoots");
            assert_eq!(over(true), 0, "{factor}");
        }
    }

    #[test]
    fn progress_counts_tiles_and_can_stop() {
        let pixels = picture(64, 10);
        let mut heard = Vec::new();
        let mut model = Nearest::new(2, 48);
        upscale(
            &pixels,
            None,
            Factor::X2,
            &mut model,
            None,
            &mut |done, total| {
                heard.push((done, total));
                true
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(heard, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);

        let mut model = Nearest::new(2, 48);
        let stopped = upscale(
            &pixels,
            None,
            Factor::X2,
            &mut model,
            None,
            &mut |done, _| done < 2,
        )
        .unwrap();
        assert!(stopped.is_none());
        assert_eq!(
            model.calls, 2,
            "stops right after the tile that was asked to be the last"
        );

        let mut model = Nearest::new(2, 48);
        let none = upscale(&pixels, None, Factor::X2, &mut model, None, &mut |_, _| {
            false
        });
        assert!(none.unwrap().is_none());
        assert_eq!(model.calls, 0);
    }

    #[test]
    fn what_cant_be_upscaled_says_why() {
        let pixels = picture(10, 10);
        let wrong_model = run(&pixels, None, Factor::X4, &mut Nearest::new(2, 48));
        assert!(wrong_model.is_err());
        let no_core = run(&pixels, None, Factor::X2, &mut Nearest::new(2, 32));
        assert!(no_core.is_err());
        let wrong_alpha = run(&pixels, Some(&[0; 3]), Factor::X2, &mut Nearest::new(2, 48));
        assert!(wrong_alpha.is_err());
        let empty = run(
            &Rgb16::new(0, 0),
            None,
            Factor::X2,
            &mut Nearest::new(2, 48),
        );
        assert!(empty.is_err());
        let too_long = run(
            &picture(40_000, 1),
            None,
            Factor::X2,
            &mut Nearest::new(2, 48),
        );
        assert!(too_long.is_err_and(|e| e.to_string().contains("65,535 px")));

        struct Short;
        impl Upscaler for Short {
            fn scale(&self) -> u32 {
                2
            }
            fn tile_size(&self) -> u32 {
                48
            }
            fn upscale(&mut self, _: &[f32]) -> Result<Vec<f32>, UpscaleError> {
                Ok(vec![0.0; 10])
            }
        }
        assert!(run(&pixels, None, Factor::X2, &mut Short).is_err());
    }

    #[test]
    fn results_are_limited_in_size() {
        assert_eq!(result_size(6000, 4000, Factor::X2), Ok((12000, 8000)));
        assert_eq!(result_size(4000, 4000, Factor::X4), Ok((16000, 16000)));
        let too_many = result_size(6000, 4000, Factor::X4).unwrap_err();
        assert_eq!(
            too_many,
            "4× would make it 384 MP, more than the 256 MP limit"
        );
        let too_long = result_size(20000, 100, Factor::X4).unwrap_err();
        assert_eq!(
            too_long,
            "4× would make it 80,000 px long, more than the 65,535 px limit"
        );
        assert_eq!(result_size(32767, 10, Factor::X2), Ok((65534, 20)));

        assert_eq!(largest_factor(4000, 3000), Some(Factor::X4));
        // 3.25× of 20,000 px is 65,000; 3.5× would be 70,000.
        assert_eq!(largest_factor(20000, 2), Factor::from_quarters(13));
        // 1.5× of 10,000 x 10,000 is 225 MP; 1.75× would be 306 MP.
        assert_eq!(largest_factor(10000, 10000), Factor::from_quarters(6));
        assert_eq!(largest_factor(60000, 2), None, "1.25× would be 75,000 px");
    }

    #[test]
    fn factors_read_as_people_write_them() {
        assert_eq!(Factor::X2.to_string(), "2×");
        assert_eq!(Factor(5).to_string(), "1.25×");
        assert_eq!(Factor(10).to_string(), "2.5×");
        assert_eq!(Factor(15).to_string(), "3.75×");
        assert!(Factor::X2.is_exact() && Factor::X4.is_exact() && !Factor(12).is_exact());
        assert_eq!(Factor(12).model_scale(), 4);
        assert_eq!(Factor(5).model_scale(), 2);
        assert_eq!(Factor::from_quarters(5), Some(Factor(5)));
        assert_eq!(Factor::from_quarters(4), None, "1× isn't an upscale");
        assert_eq!(Factor::from_quarters(17), None, "past 4×");
        // Sizes round to the nearest pixel, halves up.
        assert_eq!(
            (Factor(5).of(3), Factor(10).of(3), Factor(6).of(10)),
            (4, 8, 15)
        );
    }

    /// Every factor between the models' own: tiles of a model that ignores context, scaled down
    /// from the next model up, give what scaling the whole picture down at once gives. Only the
    /// picture's edges differ, where the tiles mirror it and the whole picture can't.
    #[test]
    fn fractional_factors_join_like_one_resample_of_the_whole_picture() {
        let (w, h) = (70u32, 41u32);
        let pixels = Rgb16::from_fn(w, h, |x, y| {
            let v = |a: u32, b: u32| (1000 + (x * a + y * b) % 60000) as u16;
            Rgb([v(911, 37), v(53, 1201), v(311, 307)])
        });
        for quarters in 5..=15 {
            let factor = Factor::from_quarters(quarters).unwrap();
            if factor.is_exact() {
                continue;
            }
            let s = factor.model_scale();
            let mut model = Nearest::new(s, 48);
            let out = run(&pixels, None, factor, &mut model).unwrap().unwrap();
            let (rw, rh) = result_size(w, h, factor).unwrap();
            assert_eq!(out.pixels.dimensions(), (rw, rh), "{factor}");

            let big: Vec<U16x3> = (0..w * s * h * s)
                .map(|i| U16x3::new(pixels.get_pixel(i % (w * s) / s, i / (w * s) / s).0))
                .collect();
            let big = TypedImage::from_pixels(w * s, h * s, big).unwrap();
            let whole = [0.0, 0.0, f64::from(w * s), f64::from(h * s)];
            let mut resizer = Resizer::new();
            let reference =
                resample(&mut resizer, &big, (rw, rh), whole, Filter::Lanczos3).unwrap();
            let edge = (4 * quarters).div_ceil(4) + 1;
            for (x, y, p) in out.pixels.enumerate_pixels() {
                if x < edge || y < edge || x >= rw - edge || y >= rh - edge {
                    continue;
                }
                let want = reference.pixels()[(y * rw + x) as usize].0;
                for c in 0..3 {
                    assert!(
                        p.0[c].abs_diff(want[c]) <= 1,
                        "{factor} at {x},{y}: {:?} against {want:?}",
                        p.0
                    );
                }
            }
        }
    }

    /// A flat picture stays flat at every factor: every pixel of the result is written, edges
    /// included.
    #[test]
    fn every_factor_writes_every_pixel() {
        let flat = Rgb16::from_pixel(53, 37, Rgb([40000, 20000, 9000]));
        for quarters in 5..=16 {
            let factor = Factor::from_quarters(quarters).unwrap();
            let mut model = Nearest::new(factor.model_scale(), 48);
            let out = run(&flat, None, factor, &mut model).unwrap().unwrap();
            for (x, y, p) in out.pixels.enumerate_pixels() {
                let ok =
                    p.0.iter()
                        .zip([40000u16, 20000, 9000])
                        .all(|(&v, w)| v.abs_diff(w) <= 1);
                assert!(ok, "{factor} at {x},{y}: {:?}", p.0);
            }
        }
    }

    #[test]
    fn fractional_factors_keep_gray_and_alpha() {
        let gray = Rgb16::from_fn(40, 30, |x, y| Rgb([(x * 1000 + y * 300) as u16; 3]));
        let alpha = vec![30000u16; 40 * 30];
        let factor = Factor::from_quarters(10).unwrap();
        let out = run(&gray, Some(&alpha), factor, &mut Tint(Nearest::new(4, 48)))
            .unwrap()
            .unwrap();
        assert_eq!(out.pixels.dimensions(), (100, 75));
        assert!(out.pixels.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));
        let alpha = out.alpha.unwrap();
        assert_eq!(alpha.len(), 100 * 75);
        assert!(alpha.iter().all(|&a| a.abs_diff(30000) <= 1));
    }
}
