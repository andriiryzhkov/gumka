// SPDX-License-Identifier: GPL-3.0-or-later

//! SAM 2.1 on Core ML as a [`Segmenter`]: Apple's conversion in three models.
//!
//! - The image encoder takes the picture once (a 1024 x 1024 BGRA pixel buffer) and returns
//!   the image embedding and two higher-resolution feature maps.
//! - The prompt encoder turns clicks (points in 1024 x 1024 coordinates; label 1 includes, 0
//!   excludes) into sparse and dense embeddings.
//! - The mask decoder combines both into three candidate masks (256 x 256 scores) and a
//!   quality score for each; the best one is kept.
//!
//! The encoder outputs stay in Core ML's own arrays and go to the decoder unchanged.

use std::path::Path;

use gumka_core::image::RgbImage;
use gumka_core::select::{Click, Scores, SegmentError, Segmenter};

use crate::coreml::{self, ComputeUnits, FeatureKind, Input, Model, MultiArray, PixelBuffer};
use crate::models;

/// The most clicks one selection takes. The compiled mask decoder accepts 2 to 15 prompt
/// tokens (`sparse_embedding` [1, 2..15, 256]), and the prompt encoder adds a padding point
/// after the clicks.
const MAX_CLICKS: usize = 14;

pub struct CoreMlSam {
    encoder: Model,
    prompt: Model,
    decoder: Model,
    size: (u32, u32),
    /// Encoder outputs for the current image: embedding, feats_s0, feats_s1.
    features: Option<[MultiArray; 3]>,
}

impl CoreMlSam {
    /// Finds, compiles (once; cached in `cache_dir`) and loads the three models.
    pub fn load(cache_dir: &Path, units: ComputeUnits) -> Result<Self, SegmentError> {
        let load = |package: &Path| -> Result<Model, SegmentError> {
            Ok(coreml::load_cached(package, cache_dir, units)?)
        };
        // The registry lists them as image encoder, prompt encoder, mask decoder.
        let packages = models::model(models::SAM)?.find_packages()?;
        let [encoder, prompt, decoder] = &packages[..] else {
            return Err("models.toml must list SAM's three packages".into());
        };
        let encoder = load(encoder)?;
        let size = encoder
            .inputs()
            .into_iter()
            .find(|f| f.name == "image")
            .and_then(|f| match f.kind {
                FeatureKind::Image { width, height, .. } => Some((width as u32, height as u32)),
                _ => None,
            })
            .ok_or("the SAM image encoder has no image input")?;
        Ok(Self {
            encoder,
            prompt: load(prompt)?,
            decoder: load(decoder)?,
            size,
            features: None,
        })
    }
}

impl Segmenter for CoreMlSam {
    fn input_size(&self) -> (u32, u32) {
        self.size
    }

    fn max_clicks(&self) -> usize {
        MAX_CLICKS
    }

    fn set_image(&mut self, image: &RgbImage) -> Result<(), SegmentError> {
        let (w, h) = image.dimensions();
        let buffer = PixelBuffer::from_rgb8(w as usize, h as usize, image.as_raw())?;
        let out = self.encoder.predict(&[("image", &buffer)])?;
        self.features = Some([
            out.array("image_embedding")?,
            out.array("feats_s0")?,
            out.array("feats_s1")?,
        ]);
        Ok(())
    }

    fn segment(&mut self, clicks: &[Click]) -> Result<Scores, SegmentError> {
        let [embedding, feats_s0, feats_s1] = self
            .features
            .as_ref()
            .ok_or("no image was set before selecting")?;
        if clicks.is_empty() {
            return Err("selecting needs at least one click".into());
        }
        if clicks.len() > MAX_CLICKS {
            return Err(format!("SAM takes at most {MAX_CLICKS} clicks per selection").into());
        }
        let n = clicks.len();
        let points: Vec<f32> = clicks.iter().flat_map(|c| [c.x, c.y]).collect();
        let labels: Vec<f32> = clicks
            .iter()
            .map(|c| if c.include { 1.0 } else { 0.0 })
            .collect();
        let points = MultiArray::from_f32(&[1, n, 2], &points)?;
        let labels = MultiArray::from_f32(&[1, n], &labels)?;
        let prompt = self.prompt.predict_inputs(&[
            ("points", Input::Array(&points)),
            ("labels", Input::Array(&labels)),
        ])?;
        let sparse = prompt.array("sparse_embeddings")?;
        let dense = prompt.array("dense_embeddings")?;
        let out = self.decoder.predict_inputs(&[
            ("image_embedding", Input::Array(embedding)),
            ("sparse_embedding", Input::Array(&sparse)),
            ("dense_embedding", Input::Array(&dense)),
            ("feats_s0", Input::Array(feats_s0)),
            ("feats_s1", Input::Array(feats_s1)),
        ])?;

        let masks = out.array("low_res_masks")?;
        let shape = masks.shape();
        let [_, count, height, width] = shape[..] else {
            return Err(format!("unexpected mask shape {shape:?}").into());
        };
        let scores = out.array("scores")?.to_f32()?;
        let best = scores
            .iter()
            .take(count)
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map_or(0, |(i, _)| i);
        let all = masks.to_f32()?;
        let plane = width * height;
        Ok(Scores {
            width: width as u32,
            height: height as u32,
            values: all[best * plane..(best + 1) * plane].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::image::Rgb;

    /// Runs SAM on this Mac, so it needs the models (`cargo xtask models`):
    /// `cargo test -p gumka-infer -- --ignored`.
    #[test]
    #[ignore = "needs the downloaded SAM models"]
    fn fourteen_clicks_work_and_fifteen_are_refused() {
        let mut sam = CoreMlSam::load(&models::cache_dir(), ComputeUnits::CpuAndGpu).unwrap();
        assert_eq!(sam.max_clicks(), 14);
        let (w, h) = sam.input_size();
        // A light disk on a dark background.
        let image = RgbImage::from_fn(w, h, |x, y| {
            let (dx, dy) = (x as f32 - w as f32 / 2.0, y as f32 - h as f32 / 2.0);
            let v = if dx.hypot(dy) < w as f32 / 4.0 {
                220
            } else {
                30
            };
            Rgb([v, v, v])
        });
        sam.set_image(&image).unwrap();
        let clicks: Vec<Click> = (0..15)
            .map(|i| Click {
                x: w as f32 / 2.0 + (i % 5) as f32 * 20.0 - 40.0,
                y: h as f32 / 2.0 + (i / 5) as f32 * 20.0 - 20.0,
                include: i % 4 != 3,
            })
            .collect();
        for n in 1..=14 {
            let scores = sam
                .segment(&clicks[..n])
                .unwrap_or_else(|e| panic!("{n} clicks: {e}"));
            assert_eq!(scores.values.len(), (scores.width * scores.height) as usize);
        }
        let error = sam.segment(&clicks).err().unwrap().to_string();
        assert_eq!(error, "SAM takes at most 14 clicks per selection");

        // The limit is the decoder's: 15 clicks and the padding point are one token too many.
        let points: Vec<f32> = clicks.iter().flat_map(|c| [c.x, c.y]).collect();
        let points = MultiArray::from_f32(&[1, 15, 2], &points).unwrap();
        let labels = MultiArray::from_f32(&[1, 15], &[1.0; 15]).unwrap();
        let prompt = sam
            .prompt
            .predict_inputs(&[
                ("points", Input::Array(&points)),
                ("labels", Input::Array(&labels)),
            ])
            .unwrap();
        let sparse = prompt.array("sparse_embeddings").unwrap();
        assert_eq!(sparse.shape(), vec![1, 16, 256]);
        let [embedding, feats_s0, feats_s1] = sam.features.as_ref().unwrap();
        let decoded = sam.decoder.predict_inputs(&[
            ("image_embedding", Input::Array(embedding)),
            ("sparse_embedding", Input::Array(&sparse)),
            (
                "dense_embedding",
                Input::Array(&prompt.array("dense_embeddings").unwrap()),
            ),
            ("feats_s0", Input::Array(feats_s0)),
            ("feats_s1", Input::Array(feats_s1)),
        ]);
        assert!(
            decoded.is_err(),
            "the decoder takes 16 prompt tokens after all"
        );
    }
}
