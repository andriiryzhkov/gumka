// SPDX-License-Identifier: GPL-3.0-or-later

//! LaMa on Core ML as an [`Inpainter`].

use std::path::Path;

use gumka_core::erase::{InpaintError, Inpainter};
use gumka_core::image::{GrayImage, RgbImage};

use crate::coreml::{self, ComputeUnits, FeatureKind, Model, PixelBuffer};

/// LaMa with pixel-buffer I/O: `image` (BGRA), `mask` (one channel) in, `output` (BGRA) out.
pub struct CoreMlLama {
    model: Model,
    size: (u32, u32),
}

impl CoreMlLama {
    /// Compiles the package (once; the result is cached in `cache_dir`) and loads it.
    pub fn load(package: &Path, cache_dir: &Path, units: ComputeUnits) -> coreml::Result<Self> {
        let model = coreml::load_cached(package, cache_dir, units)?;
        let image_size = |name: &str| {
            model
                .inputs()
                .into_iter()
                .find(|f| f.name == name)
                .and_then(|f| match f.kind {
                    FeatureKind::Image { width, height, .. } => Some((width as u32, height as u32)),
                    _ => None,
                })
        };
        let size = image_size("image");
        if size.is_none() || size != image_size("mask") {
            return Err(coreml::Error::new(format!(
                "{} is not a LaMa model: expected image inputs \"image\" and \"mask\" of one size",
                package.display()
            )));
        }
        Ok(Self {
            model,
            size: size.unwrap_or_default(),
        })
    }
}

impl Inpainter for CoreMlLama {
    fn input_size(&self) -> (u32, u32) {
        self.size
    }

    fn inpaint(&mut self, image: &RgbImage, mask: &GrayImage) -> Result<RgbImage, InpaintError> {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let image = PixelBuffer::from_rgb8(w, h, image.as_raw())?;
        let mask = PixelBuffer::from_gray8(w, h, mask.as_raw())?;
        let output = self
            .model
            .predict(&[("image", &image), ("mask", &mask)])?
            .image("output")?;
        let (ow, oh) = (output.width() as u32, output.height() as u32);
        RgbImage::from_raw(ow, oh, output.to_rgb8()?)
            .ok_or_else(|| "model output size mismatch".into())
    }
}
