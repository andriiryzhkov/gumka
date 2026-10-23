// SPDX-License-Identifier: GPL-3.0-or-later

//! LaMa on ONNX Runtime as an [`Inpainter`]: Carve's export, `image` [1, 3, H, W] RGB from 0
//! to 1 and `mask` [1, 1, H, W] (1 fills) in, `output` [1, 3, H, W] from 0 to 255 out.

use std::path::Path;

use gumka_core::erase::{InpaintError, Inpainter};
use gumka_core::image::{GrayImage, RgbImage};
use ort::session::Session;
use ort::value::Tensor;

use super::{float_shape, from_planar, planar};

pub struct OnnxLama {
    session: Session,
    size: (u32, u32),
}

impl OnnxLama {
    pub fn load(path: &Path) -> super::Result<Self> {
        let session = super::session(path)?;
        let size = input_size(
            float_shape(session.inputs(), "image"),
            float_shape(session.inputs(), "mask"),
        )
        .ok_or_else(|| {
            format!(
                "{} is not a LaMa model: expected inputs \"image\" [1, 3, H, W] and \"mask\" \
                 [1, 1, H, W]",
                path.display()
            )
        })?;
        Ok(Self { session, size })
    }
}

/// Width and height, when `image` is [_, 3, H, W] and `mask` [_, 1, H, W] of a fixed size.
fn input_size(image: Option<Vec<i64>>, mask: Option<Vec<i64>>) -> Option<(u32, u32)> {
    match (image?.as_slice(), mask?.as_slice()) {
        ([_, 3, h, w], [_, 1, mh, mw]) if h == mh && w == mw && *h > 0 && *w > 0 => {
            Some((u32::try_from(*w).ok()?, u32::try_from(*h).ok()?))
        }
        _ => None,
    }
}

impl Inpainter for OnnxLama {
    fn input_size(&self) -> (u32, u32) {
        self.size
    }

    fn inpaint(&mut self, image: &RgbImage, mask: &GrayImage) -> Result<RgbImage, InpaintError> {
        let (w, h) = self.size;
        let shape = |channels: usize| [1, channels, h as usize, w as usize];
        let image = Tensor::from_array((shape(3), planar(image, |_, v| f32::from(v) / 255.0)))?;
        let mask: Vec<f32> = mask.as_raw().iter().map(|&m| f32::from(m > 0)).collect();
        let mask = Tensor::from_array((shape(1), mask))?;
        let outputs = self
            .session
            .run(ort::inputs! { "image" => image, "mask" => mask })?;
        let (_, values) = outputs["output"].try_extract_tensor::<f32>()?;
        from_planar(values, w, h, 255.0).ok_or_else(|| "model output size mismatch".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::image::{Luma, Rgb};

    #[test]
    fn the_input_size_comes_from_matching_inputs() {
        let size = |image: &[i64], mask: &[i64]| input_size(Some(image.into()), Some(mask.into()));
        assert_eq!(
            size(&[-1, 3, 512, 512], &[-1, 1, 512, 512]),
            Some((512, 512))
        );
        assert_eq!(size(&[1, 3, 400, 600], &[1, 1, 400, 600]), Some((600, 400)));
        assert_eq!(
            size(&[1, 3, 512, 512], &[1, 1, 256, 256]),
            None,
            "sizes differ"
        );
        assert_eq!(
            size(&[1, 3, -1, -1], &[1, 1, -1, -1]),
            None,
            "size left open"
        );
        assert_eq!(size(&[1, 4, 512, 512], &[1, 1, 512, 512]), None);
        assert_eq!(input_size(None, Some(vec![1, 1, 8, 8])), None);
    }

    /// Runs LaMa on ONNX Runtime, so it needs the library and the models (`cargo xtask
    /// models`): `cargo test -p gumka-infer -- --ignored`. A hole in a flat picture is filled
    /// with its color.
    #[test]
    #[ignore = "needs ONNX Runtime and the downloaded LaMa model"]
    fn lama_fills_a_hole_in_a_flat_picture() {
        let info = crate::models::model(crate::models::LAMA).unwrap();
        let mut lama = OnnxLama::load(&info.find_packages().unwrap()[0]).unwrap();
        let (w, h) = lama.input_size();
        assert_eq!((w, h), (512, 512));
        let image = RgbImage::from_pixel(w, h, Rgb([180, 120, 60]));
        let mask = GrayImage::from_fn(w, h, |x, y| {
            let inside = (200..300).contains(&x) && (200..300).contains(&y);
            Luma([if inside { 255 } else { 0 }])
        });
        let out = lama.inpaint(&image, &mask).unwrap();
        assert_eq!(out.dimensions(), (w, h));
        for (x, y) in [(250, 250), (205, 295), (10, 10)] {
            let [r, g, b] = out.get_pixel(x, y).0;
            assert!(
                r.abs_diff(180) <= 6 && g.abs_diff(120) <= 6 && b.abs_diff(60) <= 6,
                "({x}, {y}) is {:?}",
                [r, g, b]
            );
        }
    }
}
