// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka on the command line:
//!
//! - `gumka-cli erase IN MASK OUT` removes what MASK marks (opaque pixels that aren't black)
//!   from IN and saves OUT. MASK covers IN's pixels as stored, before its EXIF orientation is
//!   applied. IN and OUT can be TIFF, PNG or JPEG; OUT keeps IN's bit depth (JPEG: 8-bit,
//!   re-encoded) and metadata.
//! - `gumka-cli select IN OUT_MASK --click X,Y` selects the object under the click with SAM
//!   and saves the mask, ready for `erase`.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use gumka_core::Mask;
use gumka_core::image::{DynamicImage, ImageFormat, ImageReader, Luma};
use gumka_core::picture::Picture;
use gumka_core::select::Click;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "usage: gumka-cli erase IN MASK OUT [--units cpu|gpu|ane|all]
       gumka-cli select IN OUT_MASK --click X,Y [--click X,Y] [--exclude X,Y] [--units ...]

  IN, OUT    TIFF, PNG or JPEG. OUT keeps IN's bit depth (JPEG: 8-bit, re-encoded), ICC
             profile and metadata.
  MASK       an image the size of IN's pixels as stored, before IN's EXIF orientation is
             applied (as `select` saves it). Opaque pixels that aren't black are erased;
             transparent and black ones are kept. Use PNG: in a JPEG, noise near black
             counts as erase.
  --click    a point on the object to select, in pixels of IN as stored.
  --exclude  a point to leave out of the selection.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = match args.next().as_deref() {
        Some(c @ ("erase" | "select")) => c.to_owned(),
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    };
    let mut paths = Vec::new();
    let mut clicks = Vec::new();
    let mut units = "gpu".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--units" => units = args.next().ok_or(USAGE)?,
            "--click" | "--exclude" => {
                let value = args.next().ok_or(USAGE)?;
                let (x, y) = value.split_once(',').ok_or(USAGE)?;
                clicks.push(Click {
                    x: x.trim().parse()?,
                    y: y.trim().parse()?,
                    include: arg == "--click",
                });
            }
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if command == "select" {
        let [input, output] = <[PathBuf; 2]>::try_from(paths).map_err(|_| USAGE)?;
        let picture = Picture::open(&input)?;
        warn_about_orientation(&picture, "the clicks");
        let mask = backend::select(&picture, &clicks, &units)?;
        mask.save(&output)?;
        println!("saved {}", output.display());
        return Ok(());
    }

    let [input, mask, output] = <[PathBuf; 3]>::try_from(paths).map_err(|_| USAGE)?;
    let mut picture = Picture::open(&input)?;
    let reader = ImageReader::open(&mask)?.with_guessed_format()?;
    if reader.format() == Some(ImageFormat::Jpeg) {
        eprintln!(
            "warning: MASK is a JPEG; its compression noise near black is erased too. A PNG \
             mask is exact."
        );
    }
    let mask = mask_from_image(&reader.decode()?);
    check_mask_size(
        mask.dimensions(),
        (picture.width(), picture.height()),
        picture.orientation(),
    )?;
    warn_about_orientation(&picture, "MASK");
    backend::erase(&mut picture, &mask, &units)?;

    let t = Instant::now();
    picture.save(&output)?;
    println!(
        "saved {} ({:.2}s)",
        output.display(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The erase marks of a mask image: opaque pixels (alpha above zero) with any channel above
/// zero, read at the image's own depth so that faint values count.
fn mask_from_image(image: &DynamicImage) -> Mask {
    let rgba = image.to_rgba16();
    Mask::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        Luma([if a > 0 && (r | g | b) > 0 { 255 } else { 0 }])
    })
}

/// The mask must cover IN's pixels as stored. One the size of the picture as viewed (turned by
/// its EXIF orientation) gets an error that says so.
fn check_mask_size(mask: (u32, u32), stored: (u32, u32), orientation: u8) -> Result<()> {
    if mask == stored {
        return Ok(());
    }
    let (w, h) = mask;
    if mask == gumka_core::picture::oriented_size(stored, orientation) {
        return Err(format!(
            "MASK is {w}x{h}, IN's size as viewed (EXIF orientation {orientation}: {}); it must \
             cover IN's pixels as stored, {}x{}",
            orientation_name(orientation),
            stored.0,
            stored.1
        )
        .into());
    }
    Err(format!("mask is {w}x{h}, image is {}x{}", stored.0, stored.1).into())
}

/// A mask (or clicks) the right size can still be turned or flipped against the picture as
/// viewed; size alone can't tell. Says so whenever IN has an orientation.
fn warn_about_orientation(picture: &Picture, what: &str) {
    let orientation = picture.orientation();
    if orientation != 1 {
        eprintln!(
            "warning: IN has EXIF orientation {orientation} ({}); {what} must be in its pixels \
             as stored, not as viewers show it",
            orientation_name(orientation)
        );
    }
}

/// How viewers show the stored pixels under an EXIF orientation.
fn orientation_name(orientation: u8) -> &'static str {
    match orientation {
        2 => "shown flipped horizontally",
        3 => "shown turned 180°",
        4 => "shown flipped vertically",
        5 => "shown transposed",
        6 => "shown turned 90° clockwise",
        7 => "shown transversed",
        8 => "shown turned 90° counterclockwise",
        _ => "shown as stored",
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use std::time::Instant;

    use gumka_core::Mask;
    use gumka_core::picture::Picture;
    use gumka_core::select::Click;
    use gumka_infer::coreml::ComputeUnits;
    use gumka_infer::{CoreMlLama, CoreMlSam, models};

    use super::{Result, USAGE};

    fn units(name: &str) -> Result<ComputeUnits> {
        Ok(match name {
            "cpu" => ComputeUnits::CpuOnly,
            "gpu" => ComputeUnits::CpuAndGpu,
            "ane" => ComputeUnits::CpuAndNeuralEngine,
            "all" => ComputeUnits::All,
            _ => return Err(USAGE.into()),
        })
    }

    pub fn erase(picture: &mut Picture, mask: &Mask, unit_name: &str) -> Result<()> {
        let t = Instant::now();
        let package = models::model(models::LAMA)?.find_packages()?.remove(0);
        let mut lama = CoreMlLama::load(&package, &models::cache_dir(), units(unit_name)?)?;
        println!("model loaded ({:.2}s)", t.elapsed().as_secs_f64());

        let t = Instant::now();
        let profile = picture.icc_profile().map(<[u8]>::to_vec);
        match gumka_core::erase::erase(
            &mut picture.pixels,
            mask,
            &mut lama,
            &gumka_core::erase::EraseSettings::default(),
            profile.as_deref(),
        )? {
            Some(gumka_core::erase::Patch { rect, .. }) => println!(
                "erased: changed {}x{} at ({}, {}) ({:.2}s)",
                rect.width,
                rect.height,
                rect.x,
                rect.y,
                t.elapsed().as_secs_f64()
            ),
            None => println!("mask is empty; nothing to erase"),
        }
        Ok(())
    }

    pub fn select(picture: &Picture, clicks: &[Click], unit_name: &str) -> Result<Mask> {
        if !clicks.iter().any(|c| c.include) {
            return Err(format!("select needs at least one --click\n\n{USAGE}").into());
        }
        let t = Instant::now();
        let mut sam = CoreMlSam::load(&models::cache_dir(), units(unit_name)?)?;
        println!("SAM loaded ({:.2}s)", t.elapsed().as_secs_f64());

        use gumka_core::select::{Segmenter, mask_from_scores, model_input, to_input};
        let size = sam.input_size();
        let t = Instant::now();
        sam.set_image(&model_input(&picture.pixels, size))?;
        println!("image encoded ({:.2}s)", t.elapsed().as_secs_f64());
        let t = Instant::now();
        let scores = sam.segment(&to_input(clicks, picture.pixels.dimensions(), size))?;
        println!("selected ({:.3}s)", t.elapsed().as_secs_f64());
        let mask = mask_from_scores(&scores, picture.pixels.dimensions());
        let selected = mask.pixels().filter(|p| p.0[0] != 0).count();
        println!(
            "{selected} pixels selected ({:.1}% of the picture)",
            100.0 * selected as f64 / mask.len() as f64
        );
        Ok(mask)
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    use gumka_core::Mask;
    use gumka_core::picture::Picture;
    use gumka_core::select::Click;

    use super::Result;

    const NONE: &str = "no inference backend on this platform yet (Core ML only, macOS)";

    pub fn erase(_: &mut Picture, _: &Mask, _: &str) -> Result<()> {
        Err(NONE.into())
    }

    pub fn select(_: &Picture, _: &[Click], _: &str) -> Result<Mask> {
        Err(NONE.into())
    }
}

#[cfg(test)]
mod tests {
    use gumka_core::image::{GrayImage, ImageBuffer, LumaA, Rgba, RgbaImage};

    use super::*;

    fn marks(mask: &Mask) -> Vec<u8> {
        mask.as_raw().clone()
    }

    #[test]
    fn opaque_pixels_that_arent_black_are_erased() {
        let rgba = RgbaImage::from_fn(5, 1, |x, _| {
            Rgba(match x {
                0 => [255, 255, 255, 0], // transparent white: kept
                1 => [0, 0, 0, 255],     // opaque black: kept
                2 => [1, 0, 0, 255],     // faint red: erased
                3 => [0, 0, 0, 1],       // nearly transparent black: kept
                _ => [10, 10, 10, 1],    // nearly transparent gray: erased
            })
        });
        let mask = mask_from_image(&DynamicImage::ImageRgba8(rgba));
        assert_eq!(marks(&mask), [0, 0, 255, 0, 255]);

        // 16-bit values too faint for 8 bits still count.
        let faint: ImageBuffer<Luma<u16>, Vec<u16>> =
            ImageBuffer::from_raw(3, 1, vec![0, 1, 65535]).unwrap();
        let mask = mask_from_image(&DynamicImage::ImageLuma16(faint));
        assert_eq!(marks(&mask), [0, 255, 255]);

        let gray_alpha: ImageBuffer<LumaA<u8>, Vec<u8>> =
            ImageBuffer::from_raw(2, 1, vec![255, 0, 255, 255]).unwrap();
        let mask = mask_from_image(&DynamicImage::ImageLumaA8(gray_alpha));
        assert_eq!(marks(&mask), [0, 255]);
        let plain = mask_from_image(&DynamicImage::ImageLuma8(
            GrayImage::from_raw(2, 1, vec![0, 9]).unwrap(),
        ));
        assert_eq!(marks(&plain), [0, 255]);
    }

    #[test]
    fn a_mask_the_size_of_the_turned_picture_is_explained() {
        assert!(check_mask_size((40, 30), (40, 30), 6).is_ok());
        let turned = check_mask_size((30, 40), (40, 30), 6)
            .unwrap_err()
            .to_string();
        assert!(
            turned.contains("orientation 6") && turned.contains("40x30"),
            "{turned}"
        );
        let wrong = check_mask_size((10, 10), (40, 30), 6)
            .unwrap_err()
            .to_string();
        assert_eq!(wrong, "mask is 10x10, image is 40x30");
        let plain = check_mask_size((30, 40), (40, 30), 1)
            .unwrap_err()
            .to_string();
        assert_eq!(plain, "mask is 30x40, image is 40x30");
    }
}
