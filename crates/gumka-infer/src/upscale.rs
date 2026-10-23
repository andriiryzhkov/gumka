// SPDX-License-Identifier: GPL-3.0-or-later

//! RealPLKSR on Core ML as an [`Upscaler`].

use std::path::Path;

use gumka_core::upscale::{UpscaleError, Upscaler};

use crate::coreml::{self, ComputeUnits, Feature, FeatureKind, Input, Model, MultiArray};
use crate::models;

/// A super-resolution model with float32 tensor I/O, as our conversion makes it: one input
/// [1, 3, T, T] and one output [1, 3, T·s, T·s], planar RGB from 0 to 1.
pub struct CoreMlUpscaler {
    model: Model,
    input: String,
    output: String,
    tile: u32,
    scale: u32,
}

impl CoreMlUpscaler {
    /// Finds the registry's `scale`× upscaler, compiles it (once; the result is cached in
    /// `cache_dir`) and loads it.
    pub fn load(scale: u32, cache_dir: &Path, units: ComputeUnits) -> coreml::Result<Self> {
        let info = models::upscaler_id(scale)
            .and_then(models::model)
            .map_err(coreml::Error::new)?;
        let package = info
            .find_packages()
            .map_err(|e| coreml::Error::new(e.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| coreml::Error::new(format!("{} has no package", info.name)))?;
        let model = coreml::load_cached(&package, cache_dir, units)?;
        let not = |why: String| {
            coreml::Error::new(format!(
                "{} is not a {scale}× upscaler: {why}",
                package.display()
            ))
        };
        let (input, in_shape) =
            single_array(model.inputs()).ok_or_else(|| not("expected one float32 input".into()))?;
        let (output, out_shape) = single_array(model.outputs())
            .ok_or_else(|| not("expected one float32 output".into()))?;
        let tile = match in_shape.as_slice() {
            [1, 3, h, w] if h == w => *h,
            _ => return Err(not(format!("input shape {in_shape:?}"))),
        };
        // Core ML may leave an output's shape unsaid; the pipeline checks the length then.
        if !out_shape.is_empty()
            && out_shape != [1, 3, tile * scale as usize, tile * scale as usize]
        {
            return Err(not(format!("output shape {out_shape:?}")));
        }
        Ok(Self {
            model,
            input,
            output,
            tile: tile as u32,
            scale,
        })
    }
}

/// The name and shape of the only feature, if it is a float32 multi-array.
fn single_array(features: Vec<Feature>) -> Option<(String, Vec<usize>)> {
    match features.as_slice() {
        [
            Feature {
                name,
                kind:
                    FeatureKind::MultiArray {
                        shape,
                        data_type: "float32",
                    },
            },
        ] => Some((name.clone(), shape.clone())),
        _ => None,
    }
}

impl Upscaler for CoreMlUpscaler {
    fn scale(&self) -> u32 {
        self.scale
    }

    fn tile_size(&self) -> u32 {
        self.tile
    }

    fn upscale(&mut self, rgb: &[f32]) -> Result<Vec<f32>, UpscaleError> {
        let t = self.tile as usize;
        let input = MultiArray::from_f32(&[1, 3, t, t], rgb)?;
        let output = self
            .model
            .predict_inputs(&[(&self.input, Input::Array(&input))])?
            .array(&self.output)?;
        Ok(output.to_f32()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::Rgb16;
    use gumka_core::image::Rgb;
    use gumka_core::image::imageops::{self, FilterType};
    use gumka_core::upscale::{Factor, OVERLAP, upscale};

    /// Runs RealPLKSR on this Mac, so it needs the models (`cargo xtask models`):
    /// `cargo test -p gumka-infer -- --ignored`. A smooth picture, a few tiles wide, comes back
    /// the right size, close to an ordinary resize, and with no step at the tile borders.
    /// Compiled models go to a folder of the test's own, always the same one, so the app's
    /// cache is left alone and Core ML's plan cache doesn't grow with every run.
    #[test]
    #[ignore = "needs the downloaded upscale models"]
    fn both_upscalers_load_and_join_their_tiles_smoothly() {
        let (w, h) = (1000u32, 300u32);
        let picture = Rgb16::from_fn(w, h, |x, y| {
            let (x, y) = (f64::from(x) / f64::from(w), f64::from(y) / f64::from(h));
            Rgb([
                (x * 50000.0 + 5000.0) as u16,
                (y * 40000.0 + 8000.0) as u16,
                ((0.5 + 0.1 * (x * 20.0).sin() * (y * 15.0).cos()) * 60000.0) as u16,
            ])
        });
        let cache = std::env::temp_dir().join("gumka-upscale-test-models");
        for factor in Factor::EXACT {
            let s = factor.model_scale();
            let mut model = CoreMlUpscaler::load(s, &cache, ComputeUnits::CpuAndGpu).unwrap();
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

            // The largest step between neighbouring columns at a tile border is no larger than
            // elsewhere inside the picture.
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
