// SPDX-License-Identifier: GPL-3.0-or-later

//! Upscaling through the public API, with the color spaces a model sees: with a model that
//! only replicates pixels, an ordinary profile's values come back exactly, and a linear
//! profile's come back at their level after the round trip through Display P3.

use gumka_core::Rgb16;
use gumka_core::color::may_clip_beyond_p3;
use gumka_core::image::Rgb;
use gumka_core::upscale::{Factor, UpscaleError, Upscaler, upscale};
use lcms2::{CIExyY, CIExyYTRIPLE, Profile, ToneCurve};

/// Each pixel twice as wide and twice as tall. Keeps the largest value it was given.
struct Nearest(f32);

impl Upscaler for Nearest {
    fn scale(&self) -> u32 {
        2
    }

    fn tile_size(&self) -> u32 {
        64
    }

    fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
        let (t, o) = (64, 128);
        self.0 = rgb.iter().copied().fold(self.0, f32::max);
        let mut out = vec![0.0; 3 * o * o];
        for c in 0..3 {
            for y in 0..o {
                for x in 0..o {
                    out[c * o * o + y * o + x] = rgb[c * t * t + (y / 2) * t + x / 2];
                }
            }
        }
        Ok(out)
    }
}

fn linear_srgb() -> Vec<u8> {
    let xy = |x, y| CIExyY { x, y, Y: 1.0 };
    let primaries = CIExyYTRIPLE {
        Red: xy(0.64, 0.33),
        Green: xy(0.30, 0.60),
        Blue: xy(0.15, 0.06),
    };
    let linear = ToneCurve::new(1.0);
    Profile::new_rgb(
        &xy(0.3127, 0.3290),
        &primaries,
        &[&linear, &linear, &linear],
    )
    .unwrap()
    .icc()
    .unwrap()
}

/// A neutral ramp, from black to `top`, with a muted color across it.
fn picture(top: f64) -> Rgb16 {
    Rgb16::from_fn(90, 50, |x, y| {
        let v = f64::from(x) / 89.0 * top;
        Rgb([
            v.round() as u16,
            (v * 0.9).round() as u16,
            (v * (0.8 + f64::from(y) / 250.0)).round() as u16,
        ])
    })
}

/// The picture upscaled, and the largest value the model saw.
fn upscaled(pixels: &Rgb16, profile: Option<&[u8]>) -> (Rgb16, f32) {
    let mut model = Nearest(0.0);
    let out = upscale(
        pixels,
        None,
        Factor::X2,
        &mut model,
        profile,
        &mut |_, _| true,
    )
    .unwrap()
    .unwrap();
    (out.pixels, model.0)
}

#[test]
fn an_ordinary_profile_goes_to_the_model_unconverted() {
    let srgb = Profile::new_srgb().icc().unwrap();
    assert!(!may_clip_beyond_p3(Some(&srgb)));
    let pixels = picture(60000.0);
    let (out, seen) = upscaled(&pixels, Some(&srgb));
    assert_eq!(seen, 60000.0 / 65535.0);
    for (x, y, p) in out.enumerate_pixels() {
        assert_eq!(p, pixels.get_pixel(x / 2, y / 2), "at {x},{y}");
    }
}

#[test]
fn a_linear_picture_comes_back_at_its_level() {
    let linear = linear_srgb();
    assert!(!may_clip_beyond_p3(Some(&linear)), "P3 holds sRGB's colors");
    let pixels = picture(40000.0);
    let (out, seen) = upscaled(&pixels, Some(&linear));
    // Linear 0.61 is about 0.8 encoded: the model sees the picture in P3, not as stored.
    assert!(seen > 0.75, "{seen}");
    for (x, y, p) in out.enumerate_pixels() {
        let want = pixels.get_pixel(x / 2, y / 2);
        for c in 0..3 {
            let (got, want) = (f64::from(p.0[c]), f64::from(want.0[c]));
            assert!(
                (got - want).abs() <= (want * 0.02).max(40.0),
                "at {x},{y} channel {c}: {got} against {want}"
            );
        }
    }
}
