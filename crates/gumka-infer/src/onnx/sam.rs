// SPDX-License-Identifier: GPL-3.0-or-later

//! SAM 2.1 on ONNX Runtime as a [`Segmenter`]: the onnx-community export in two models.
//!
//! - The vision encoder takes the picture once (`pixel_values` [1, 3, 1024, 1024], normalized
//!   with ImageNet's mean and deviation) and returns three feature maps, `image_embeddings.0`
//!   to `.2`, kept for the clicks that follow.
//! - The prompt encoder and mask decoder take them with the clicks (`input_points`
//!   [1, 1, N, 2] in 1024 × 1024 coordinates, `input_labels` [1, 1, N], 1 includes and 0
//!   excludes, and no boxes) and return three candidate masks (`pred_masks`, 256 × 256 scores)
//!   with a quality score each (`iou_scores`); the best one is kept.

use gumka_core::image::RgbImage;
use gumka_core::select::{Click, Scores, SegmentError, Segmenter};
use ort::session::Session;
use ort::value::{DynValue, Tensor};

use super::{float_shape, planar};
use crate::models;

/// ImageNet's mean and standard deviation, per channel, which the encoder was trained with.
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

const FEATURES: [&str; 3] = [
    "image_embeddings.0",
    "image_embeddings.1",
    "image_embeddings.2",
];

pub struct OnnxSam {
    encoder: Session,
    decoder: Session,
    size: (u32, u32),
    /// Encoder outputs for the current image, in [`FEATURES`] order.
    features: Option<[DynValue; 3]>,
}

impl OnnxSam {
    /// Finds and loads the two models.
    pub fn load() -> Result<Self, SegmentError> {
        // The registry lists them as encoder, then decoder.
        let packages = models::model(models::SAM)?.find_packages()?;
        let [encoder, decoder] = &packages[..] else {
            return Err("models.toml must list SAM's two ONNX packages".into());
        };
        let encoder = super::session(encoder)?;
        let size = match float_shape(encoder.inputs(), "pixel_values").as_deref() {
            Some([_, 3, h, w]) if *h > 0 && *w > 0 => (*w as u32, *h as u32),
            shape => {
                return Err(format!(
                    "the SAM encoder takes pixel_values {shape:?}, expected [1, 3, H, W]"
                )
                .into());
            }
        };
        Ok(Self {
            encoder,
            decoder: super::session(decoder)?,
            size,
            features: None,
        })
    }
}

impl Segmenter for OnnxSam {
    fn input_size(&self) -> (u32, u32) {
        self.size
    }

    /// The decoder takes any number of clicks.
    fn max_clicks(&self) -> usize {
        usize::MAX
    }

    fn set_image(&mut self, image: &RgbImage) -> Result<(), SegmentError> {
        self.features = None;
        let (w, h) = image.dimensions();
        let pixels = planar(image, |c, v| (f32::from(v) / 255.0 - MEAN[c]) / STD[c]);
        let pixels = Tensor::from_array(([1, 3, h as usize, w as usize], pixels))?;
        let mut out = self
            .encoder
            .run(ort::inputs! { "pixel_values" => pixels })?;
        let mut take = |name: &str| {
            out.remove(name)
                .ok_or_else(|| format!("the SAM encoder gave no {name}"))
        };
        self.features = Some([take(FEATURES[0])?, take(FEATURES[1])?, take(FEATURES[2])?]);
        Ok(())
    }

    fn segment(&mut self, clicks: &[Click]) -> Result<Scores, SegmentError> {
        let [f0, f1, f2] = self
            .features
            .as_ref()
            .ok_or("no image was set before selecting")?;
        if clicks.is_empty() {
            return Err("selecting needs at least one click".into());
        }
        let n = clicks.len();
        let points: Vec<f32> = clicks.iter().flat_map(|c| [c.x, c.y]).collect();
        let labels: Vec<i64> = clicks.iter().map(|c| i64::from(c.include)).collect();
        let points = Tensor::from_array(([1, 1, n, 2], points))?;
        let labels = Tensor::from_array(([1, 1, n], labels))?;
        let boxes = Tensor::from_array(([1, 0, 4], Vec::<f32>::new()))?;
        let out = self.decoder.run(ort::inputs! {
            "input_points" => points,
            "input_labels" => labels,
            "input_boxes" => boxes,
            FEATURES[0] => f0,
            FEATURES[1] => f1,
            FEATURES[2] => f2,
        })?;
        let (shape, masks) = out["pred_masks"].try_extract_tensor::<f32>()?;
        let (_, scores) = out["iou_scores"].try_extract_tensor::<f32>()?;
        best_mask(shape, masks, scores).map_err(Into::into)
    }
}

/// The mask with the best score among `masks` [1, 1, count, H, W].
fn best_mask(shape: &[i64], masks: &[f32], scores: &[f32]) -> Result<Scores, String> {
    let dims: Option<Vec<usize>> = shape.iter().map(|&d| usize::try_from(d).ok()).collect();
    let (count, height, width) = match dims.as_deref() {
        Some(&[1, 1, count, height, width]) if count > 0 && height > 0 && width > 0 => {
            (count, height, width)
        }
        _ => return Err(format!("unexpected mask shape {shape:?}")),
    };
    let unexpected = || format!("unexpected mask shape {shape:?}");
    let (w, h) = (
        u32::try_from(width).map_err(|_| unexpected())?,
        u32::try_from(height).map_err(|_| unexpected())?,
    );
    // Checked: sizes a model merely claims can't wrap around into a short slice.
    let plane = width.checked_mul(height).ok_or_else(unexpected)?;
    if count.checked_mul(plane) != Some(masks.len()) || scores.len() < count {
        return Err("the SAM decoder's outputs don't match their shapes".into());
    }
    let best = scores[..count]
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i);
    Ok(Scores {
        width: w,
        height: h,
        values: masks[best * plane..(best + 1) * plane].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::image::Rgb;
    use gumka_core::select::{mask_from_scores, to_input};

    #[test]
    fn the_best_scored_mask_is_kept() {
        let masks: Vec<f32> = (0..3).flat_map(|m| [m as f32; 4]).collect();
        let best = best_mask(&[1, 1, 3, 2, 2], &masks, &[0.2, 0.9, 0.4]).unwrap();
        assert_eq!((best.width, best.height), (2, 2));
        assert_eq!(best.values, [1.0; 4]);
        assert!(best_mask(&[1, 1, 3, 2, 2], &masks[1..], &[0.2, 0.9, 0.4]).is_err());
        assert!(best_mask(&[1, 3, 2, 2], &masks, &[0.2, 0.9, 0.4]).is_err());
        assert!(best_mask(&[1, 1, 3, 2, 2], &masks, &[0.2]).is_err());
        // An empty mask, and sizes whose product wraps around, are refused.
        assert!(best_mask(&[1, 1, 3, 0, 0], &[], &[0.2, 0.9, 0.4]).is_err());
        assert!(best_mask(&[1, 1, 1, 1 << 32, 1 << 32], &[], &[1.0]).is_err());
        assert!(best_mask(&[1, 1, 1 << 34, 1 << 15, 1 << 15], &[], &[1.0]).is_err());
    }

    /// Runs SAM on ONNX Runtime, so it needs the library and the models (`cargo xtask
    /// models`): `cargo test -p gumka-infer -- --ignored`. A click on a light disk selects the
    /// disk; one more on a square adds it, or leaves it out as an exclusion.
    #[test]
    #[ignore = "needs ONNX Runtime and the downloaded SAM models"]
    fn clicks_select_and_exclude_objects() {
        let mut sam = OnnxSam::load().unwrap();
        let (w, h) = sam.input_size();
        assert_eq!((w, h), (1024, 1024));
        let disk = |x: u32, y: u32| (x as f32 - 400.0).hypot(y as f32 - 500.0) < 150.0;
        let square = |x: u32, y: u32| x.abs_diff(780) < 90 && y.abs_diff(300) < 90;
        let image = RgbImage::from_fn(w, h, |x, y| match (disk(x, y), square(x, y)) {
            (true, _) => Rgb([220, 200, 180]),
            (_, true) => Rgb([30, 90, 200]),
            _ => Rgb([(x * 60 / w + 20) as u8, (y * 60 / h + 20) as u8, 40]),
        });
        sam.set_image(&image).unwrap();
        let click = |x, y, include| Click { x, y, include };
        let mut overlap = |clicks: &[Click], object: &dyn Fn(u32, u32) -> bool| {
            let scores = sam.segment(&to_input(clicks, (w, h), (w, h))).unwrap();
            let mask = mask_from_scores(&scores, (w, h));
            let (mut both, mut either) = (0, 0);
            for (x, y, m) in mask.enumerate_pixels() {
                let (a, b) = (m.0[0] != 0, object(x, y));
                both += u32::from(a && b);
                either += u32::from(a || b);
            }
            f64::from(both) / f64::from(either.max(1))
        };
        let on_disk = click(400.0, 500.0, true);
        assert!(overlap(&[on_disk], &disk) > 0.95);
        let both = |x, y| disk(x, y) || square(x, y);
        assert!(overlap(&[on_disk, click(780.0, 300.0, true)], &both) > 0.95);
        assert!(overlap(&[on_disk, click(780.0, 300.0, false)], &disk) > 0.95);
    }
}
