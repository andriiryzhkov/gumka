// SPDX-License-Identifier: GPL-3.0-or-later

//! RealPLKSR on ONNX Runtime as an [`Upscaler`]: one float32 input [1, 3, T, T] and one output
//! [1, 3, T·s, T·s], planar RGB from 0 to 1, as for Core ML.

use gumka_core::upscale::{UpscaleError, Upscaler};
use ort::session::Session;
use ort::value::TensorRef;

use super::float_shape;
use crate::models;

pub struct OnnxUpscaler {
    session: Session,
    input: String,
    output: String,
    tile: u32,
    scale: u32,
}

impl OnnxUpscaler {
    /// Finds the registry's `scale`× upscaler and loads it.
    pub fn load(scale: u32) -> super::Result<Self> {
        let info = models::model(models::upscaler_id(scale)?)?;
        let path = info
            .find_packages()?
            .into_iter()
            .next()
            .ok_or_else(|| format!("{} has no package", info.name))?;
        let session = super::session(&path)?;
        let not = |why: String| format!("{} is not a {scale}× upscaler: {why}", path.display());
        let (input, output) = match (session.inputs(), session.outputs()) {
            ([input], [output]) => (input.name().to_owned(), output.name().to_owned()),
            _ => return Err(not("expected one input and one output".into()).into()),
        };
        let in_shape = float_shape(session.inputs(), &input);
        let out_shape = float_shape(session.outputs(), &output);
        let tile =
            tile_size(in_shape.as_deref(), out_shape.as_deref(), scale).ok_or_else(|| {
                not(format!(
                    "input {in_shape:?} and output {out_shape:?}, expected float32 [1, 3, T, T] \
                 and [1, 3, {scale}T, {scale}T]"
                ))
            })?;
        Ok(Self {
            session,
            input,
            output,
            tile,
            scale,
        })
    }
}

/// The tile side, when the input is [1, 3, T, T] and the output [1, 3, sT, sT] or of a size
/// left open (the pipeline checks the length then).
fn tile_size(input: Option<&[i64]>, output: Option<&[i64]>, scale: u32) -> Option<u32> {
    let tile = match input? {
        [1, 3, h, w] if h == w && *h > 0 => *h,
        _ => return None,
    };
    let side = tile * i64::from(scale);
    match output? {
        [1, 3, h, w] if [*h, *w].iter().all(|&d| d == side || d < 0) => u32::try_from(tile).ok(),
        _ => None,
    }
}

impl Upscaler for OnnxUpscaler {
    fn scale(&self) -> u32 {
        self.scale
    }

    fn tile_size(&self) -> u32 {
        self.tile
    }

    fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
        let t = self.tile as usize;
        let input = TensorRef::from_array_view(([1, 3, t, t], rgb))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input.as_str() => input])?;
        let (_, values) = outputs[self.output.as_str()].try_extract_tensor::<f32>()?;
        Ok(values.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::Rgb16;
    use gumka_core::image::Rgb;
    use gumka_core::image::imageops::{self, FilterType};
    use gumka_core::upscale::{Factor, OVERLAP, upscale};

    #[test]
    fn the_tile_size_comes_from_the_shapes() {
        let size = |i: &[i64], o: &[i64], s| tile_size(Some(i), Some(o), s);
        assert_eq!(size(&[1, 3, 512, 512], &[1, 3, 1024, 1024], 2), Some(512));
        assert_eq!(size(&[1, 3, 256, 256], &[1, 3, 1024, 1024], 4), Some(256));
        assert_eq!(
            size(&[1, 3, 256, 256], &[1, 3, -1, -1], 4),
            Some(256),
            "left open"
        );
        assert_eq!(
            size(&[1, 3, 256, 256], &[1, 3, 512, 512], 4),
            None,
            "another scale"
        );
        assert_eq!(
            size(&[1, 3, 256, 128], &[1, 3, 1024, 512], 4),
            None,
            "not square"
        );
        assert_eq!(
            size(&[1, 3, -1, -1], &[1, 3, -1, -1], 2),
            None,
            "tile left open"
        );
        assert_eq!(tile_size(None, Some(&[1, 3, 8, 8]), 2), None, "not float32");
    }

    /// Runs RealPLKSR on ONNX Runtime, so it needs the library and the models (`cargo xtask
    /// models`): `cargo test -p gumka-infer -- --ignored`. A smooth picture, a few tiles wide,
    /// comes back the right size, close to an ordinary resize, and with no step at the tile
    /// borders.
    #[test]
    #[ignore = "needs ONNX Runtime and the downloaded upscale models"]
    fn both_upscalers_load_and_join_their_tiles_smoothly() {
        let (w, h) = (700u32, 300u32);
        let picture = Rgb16::from_fn(w, h, |x, y| {
            let (x, y) = (f64::from(x) / f64::from(w), f64::from(y) / f64::from(h));
            Rgb([
                (x * 50000.0 + 5000.0) as u16,
                (y * 40000.0 + 8000.0) as u16,
                ((0.5 + 0.1 * (x * 20.0).sin() * (y * 15.0).cos()) * 60000.0) as u16,
            ])
        });
        for factor in Factor::EXACT {
            let s = factor.model_scale();
            let mut model = OnnxUpscaler::load(s).unwrap();
            assert_eq!(model.scale(), s);
            let out = upscale(&picture, None, factor, &mut model, None, &mut |_, _| true)
                .unwrap()
                .unwrap()
                .pixels;
            assert_eq!(out.dimensions(), (w * s, h * s));

            let plain = imageops::resize(&picture, w * s, h * s, FilterType::Lanczos3);
            let mse: f64 = out
                .as_raw()
                .iter()
                .zip(plain.as_raw())
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / out.as_raw().len() as f64;
            let psnr = 10.0 * (65535.0f64.powi(2) / mse).log10();
            assert!(psnr > 35.0, "{factor}: {psnr:.1} dB against a plain resize");

            let core = (model.tile_size() - 2 * OVERLAP) * s;
            let step = |x: u32| {
                (s * 8..h * s - s * 8)
                    .map(|y| {
                        let (a, b) = (out.get_pixel(x, y), out.get_pixel(x + 1, y));
                        (0..3).map(|c| a.0[c].abs_diff(b.0[c])).max().unwrap_or(0)
                    })
                    .max()
                    .unwrap_or(0)
            };
            let borders: Vec<u32> = (1..)
                .map(|k| k * core - 1)
                .take_while(|&x| x < w * s - 1)
                .collect();
            let at_borders = borders.iter().map(|&x| step(x)).max().unwrap_or(0);
            let elsewhere = (s * 8..w * s - s * 8)
                .filter(|x| !borders.iter().any(|b| b.abs_diff(*x) <= 2))
                .step_by(7)
                .map(step)
                .max()
                .unwrap_or(0);
            assert!(
                at_borders <= elsewhere.max(64) * 2,
                "{factor}: steps of {at_borders} at tile borders, {elsewhere} elsewhere"
            );
        }
    }
}
