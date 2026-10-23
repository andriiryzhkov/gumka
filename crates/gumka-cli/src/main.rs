// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka on the command line:
//!
//! - `gumka-cli erase IN MASK OUT` removes what MASK marks (opaque pixels that aren't black)
//!   from IN and saves OUT. MASK covers IN's pixels as stored, before its EXIF orientation is
//!   applied. IN and OUT can be TIFF, PNG or JPEG; OUT keeps IN's bit depth (JPEG: 8-bit,
//!   re-encoded) and metadata.
//! - `gumka-cli select IN OUT_MASK --click X,Y` selects the object under the click with SAM
//!   and saves the mask, ready for `erase`.
//! - `gumka-cli upscale IN OUT --scale FACTOR` makes IN 1.25 to 4 times larger with RealPLKSR
//!   and saves OUT, with IN's bit depth, profile and metadata (EXIF's size updated).

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use gumka_core::Mask;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use gumka_core::erase::Inpainter;
use gumka_core::image::{DynamicImage, ImageFormat, ImageReader, Luma};
use gumka_core::picture::Picture;
use gumka_core::select::Click;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use gumka_core::select::{Segmenter, mask_from_scores, model_input, to_input};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use gumka_core::upscale::{Alpha, Upscaler};
use gumka_core::upscale::{Factor, largest_factor, result_size};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "usage: gumka-cli erase IN MASK OUT [--units cpu|gpu|ane|all]
       gumka-cli select IN OUT_MASK --click X,Y [--click X,Y] [--exclude X,Y] [--units ...]
       gumka-cli upscale IN OUT --scale FACTOR [--units cpu|gpu]

  IN, OUT    TIFF, PNG or JPEG. OUT keeps IN's bit depth (JPEG: 8-bit, re-encoded), ICC
             profile and metadata.
  MASK       an image the size of IN's pixels as stored, before IN's EXIF orientation is
             applied (as `select` saves it). Opaque pixels that aren't black are erased;
             transparent and black ones are kept. Use PNG: in a JPEG, noise near black
             counts as erase.
  --click    a point on the object to select, in pixels of IN as stored.
  --exclude  a point to leave out of the selection.
  --scale    how many times larger: 1.25 to 4 in steps of 0.25. 2 and 4 are the models' own
             output; others are made with the next one up and scaled down. The result may be
             at most 256 MP and 65,535 px a side.
  --units    where the model runs, on macOS (Core ML). RealPLKSR (upscale) gives wrong
             results on the Neural Engine, so it takes cpu or gpu only. On Linux models run
             on the CPU with ONNX Runtime, so only cpu is taken.";

/// Where models run unless `--units` says otherwise.
const DEFAULT_UNITS: &str = if cfg!(target_os = "macos") {
    "gpu"
} else {
    "cpu"
};

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
        Some(c @ ("erase" | "select" | "upscale")) => c.to_owned(),
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    };
    let mut paths = Vec::new();
    let mut clicks = Vec::new();
    let mut units = DEFAULT_UNITS.to_string();
    let mut factor = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--units" => units = args.next().ok_or(USAGE)?,
            "--scale" => factor = Some(scale(&args.next().unwrap_or_default())?),
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
    check_units(&command, &units)?;
    if command == "upscale" {
        let [input, output] = <[PathBuf; 2]>::try_from(paths).map_err(|_| USAGE)?;
        let factor = factor.ok_or_else(|| format!("upscale needs --scale\n\n{USAGE}"))?;
        let mut picture = Picture::open(&input)?;
        check_result_size((picture.width(), picture.height()), factor)?;
        backend::upscale(&mut picture, factor, &units)?;
        let t = Instant::now();
        picture.save(&output)?;
        println!(
            "saved {} ({:.2}s)",
            output.display(),
            t.elapsed().as_secs_f64()
        );
        return Ok(());
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

/// `--scale`'s value: 1.25 to 4, in quarter steps.
fn scale(value: &str) -> Result<Factor> {
    let quarters = value
        .trim_end_matches(['x', '×'])
        .parse::<f64>()
        .ok()
        .map(|v| v * 4.0);
    quarters
        .filter(|q| q.fract() == 0.0)
        .and_then(|q| Factor::from_quarters(q as u32))
        .ok_or_else(|| format!("--scale takes 1.25 to 4 in steps of 0.25, not {value:?}").into())
}

/// `--units` names compute units there are; `upscale` refuses the Neural Engine, where RealPLKSR
/// gives wrong results.
#[cfg(target_os = "macos")]
fn check_units(command: &str, units: &str) -> Result<()> {
    let upscale = command == "upscale";
    match units {
        "cpu" | "gpu" => Ok(()),
        "ane" | "all" if upscale => {
            Err("RealPLKSR gives wrong results on the Neural Engine; use --units gpu or cpu".into())
        }
        "ane" | "all" => Ok(()),
        other if upscale => Err(format!("--units takes cpu or gpu, not {other:?}").into()),
        other => Err(format!("--units takes cpu, gpu, ane or all, not {other:?}").into()),
    }
}

/// On Linux ONNX Runtime runs the models on the CPU, so `--units` can only say so.
#[cfg(target_os = "linux")]
fn check_units(_: &str, units: &str) -> Result<()> {
    match units {
        "cpu" => Ok(()),
        other => Err(format!(
            "--units takes only cpu here, where ONNX Runtime runs the models on the CPU, not \
             {other:?}"
        )
        .into()),
    }
}

/// No backend here yet: the command is refused whatever `--units` says.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn check_units(_: &str, _: &str) -> Result<()> {
    Ok(())
}

/// A picture of `size` must stay within the result's limits at `factor`. Too large, it says to
/// use a smaller scale, or that none is small enough.
fn check_result_size((width, height): (u32, u32), factor: Factor) -> Result<()> {
    let Err(why) = result_size(width, height, factor) else {
        return Ok(());
    };
    let advice = match largest_factor(width, height) {
        Some(_) => "use a smaller --scale",
        None => "the picture is already as large as upscale can make it",
    };
    Err(format!("{why}; {advice}").into())
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

/// Erases what `mask` marks with `model`, and says what changed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn erase(picture: &mut Picture, mask: &Mask, model: &mut dyn Inpainter) -> Result<()> {
    let t = Instant::now();
    let profile = picture.icc_profile().map(<[u8]>::to_vec);
    match gumka_core::erase::erase(
        &mut picture.pixels,
        mask,
        model,
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

/// Upscales the picture by `factor` with `model`, showing progress.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn upscale(picture: &mut Picture, factor: Factor, model: &mut dyn Upscaler) -> Result<()> {
    let (w, h) = (picture.width(), picture.height());
    let t = Instant::now();
    let profile = picture.icc_profile().map(<[u8]>::to_vec);
    let mut shown = 0;
    let alpha = picture.alpha().map(|values| Alpha {
        values,
        premultiplied: picture.associated_alpha(),
    });
    let result = gumka_core::upscale::upscale(
        &picture.pixels,
        alpha,
        factor,
        model,
        profile.as_deref(),
        &mut |done, total| {
            let tenths = done * 10 / total.max(1);
            if tenths > shown {
                shown = tenths;
                eprintln!("{}% ({done} of {total} tiles)", tenths * 10);
            }
            true
        },
    )?
    .ok_or("stopped")?;
    let (rw, rh) = result.pixels.dimensions();
    let (mut pixels, mut alpha) = (result.pixels, result.alpha);
    picture.swap_pixels(&mut pixels, &mut alpha)?;
    println!(
        "upscaled {w}x{h} to {rw}x{rh} ({:.1}s)",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// A selection needs a click on the object, before any model loads.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn check_clicks(clicks: &[Click]) -> Result<()> {
    if clicks.iter().any(|c| c.include) {
        Ok(())
    } else {
        Err(format!("select needs at least one --click\n\n{USAGE}").into())
    }
}

/// Selects the object under the clicks with `model`, and says how much of the picture it is.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn select(picture: &Picture, clicks: &[Click], model: &mut dyn Segmenter) -> Result<Mask> {
    let size = model.input_size();
    let t = Instant::now();
    model.set_image(&model_input(&picture.pixels, size))?;
    println!("image encoded ({:.2}s)", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let scores = model.segment(&to_input(clicks, picture.pixels.dimensions(), size))?;
    println!("selected ({:.3}s)", t.elapsed().as_secs_f64());
    let mask = mask_from_scores(&scores, picture.pixels.dimensions());
    let selected = mask.pixels().filter(|p| p.0[0] != 0).count();
    println!(
        "{selected} pixels selected ({:.1}% of the picture)",
        100.0 * selected as f64 / mask.len() as f64
    );
    Ok(mask)
}

#[cfg(target_os = "macos")]
mod backend {
    use std::time::Instant;

    use gumka_core::Mask;
    use gumka_core::picture::Picture;
    use gumka_core::select::Click;
    use gumka_core::upscale::Factor;
    use gumka_infer::coreml::ComputeUnits;
    use gumka_infer::{CoreMlLama, CoreMlSam, CoreMlUpscaler, models};

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
        super::erase(picture, mask, &mut lama)
    }

    pub fn upscale(picture: &mut Picture, factor: Factor, unit_name: &str) -> Result<()> {
        let t = Instant::now();
        let scale = factor.model_scale();
        let mut model = CoreMlUpscaler::load(scale, &models::cache_dir(), units(unit_name)?)?;
        let name = &models::model(models::upscaler_id(scale)?)?.name;
        println!("{name} loaded ({:.2}s)", t.elapsed().as_secs_f64());
        super::upscale(picture, factor, &mut model)
    }

    pub fn select(picture: &Picture, clicks: &[Click], unit_name: &str) -> Result<Mask> {
        super::check_clicks(clicks)?;
        let t = Instant::now();
        let mut sam = CoreMlSam::load(&models::cache_dir(), units(unit_name)?)?;
        println!("SAM loaded ({:.2}s)", t.elapsed().as_secs_f64());
        super::select(picture, clicks, &mut sam)
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::time::Instant;

    use gumka_core::Mask;
    use gumka_core::picture::Picture;
    use gumka_core::select::Click;
    use gumka_core::upscale::Factor;
    use gumka_infer::models;
    use gumka_infer::onnx::{OnnxLama, OnnxSam, OnnxUpscaler};

    use super::Result;

    pub fn erase(picture: &mut Picture, mask: &Mask, _: &str) -> Result<()> {
        let t = Instant::now();
        let package = models::model(models::LAMA)?.find_packages()?.remove(0);
        let mut lama = OnnxLama::load(&package)?;
        println!("model loaded ({:.2}s)", t.elapsed().as_secs_f64());
        super::erase(picture, mask, &mut lama)
    }

    pub fn upscale(picture: &mut Picture, factor: Factor, _: &str) -> Result<()> {
        let t = Instant::now();
        let scale = factor.model_scale();
        let mut model = OnnxUpscaler::load(scale)?;
        let name = &models::model(models::upscaler_id(scale)?)?.name;
        println!("{name} loaded ({:.2}s)", t.elapsed().as_secs_f64());
        super::upscale(picture, factor, &mut model)
    }

    pub fn select(picture: &Picture, clicks: &[Click], _: &str) -> Result<Mask> {
        super::check_clicks(clicks)?;
        let t = Instant::now();
        let mut sam = OnnxSam::load()?;
        println!("SAM loaded ({:.2}s)", t.elapsed().as_secs_f64());
        super::select(picture, clicks, &mut sam)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod backend {
    use gumka_core::Mask;
    use gumka_core::picture::Picture;
    use gumka_core::select::Click;
    use gumka_core::upscale::Factor;

    use super::Result;

    const NONE: &str = "no inference backend on this platform yet (Core ML on macOS, ONNX \
                        Runtime on Linux)";

    pub fn erase(_: &mut Picture, _: &Mask, _: &str) -> Result<()> {
        Err(NONE.into())
    }

    pub fn select(_: &Picture, _: &[Click], _: &str) -> Result<Mask> {
        Err(NONE.into())
    }

    pub fn upscale(_: &mut Picture, _: Factor, _: &str) -> Result<()> {
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
    fn scales_are_quarter_steps_from_one_and_a_quarter_to_four() {
        assert_eq!(scale("2").unwrap(), Factor::X2);
        assert_eq!(scale("4x").unwrap(), Factor::X4);
        assert_eq!(scale("2.5").unwrap().to_string(), "2.5×");
        assert_eq!(scale("1.25×").unwrap().to_string(), "1.25×");
        for wrong in ["1", "2.3", "5", "", "two"] {
            assert!(scale(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn upscale_refuses_the_neural_engine() {
        for units in ["ane", "all"] {
            let error = check_units("upscale", units).unwrap_err().to_string();
            assert_eq!(
                error,
                "RealPLKSR gives wrong results on the Neural Engine; use --units gpu or cpu"
            );
            assert!(check_units("erase", units).is_ok() && check_units("select", units).is_ok());
        }
        for command in ["erase", "select", "upscale"] {
            assert!(check_units(command, "gpu").is_ok() && check_units(command, "cpu").is_ok());
        }
    }

    /// An unknown unit gets an error that names it, not the whole usage.
    #[test]
    #[cfg(target_os = "macos")]
    fn unknown_units_are_named() {
        let error = |command| check_units(command, "bogus").unwrap_err().to_string();
        assert_eq!(
            error("erase"),
            "--units takes cpu, gpu, ane or all, not \"bogus\""
        );
        assert_eq!(error("upscale"), "--units takes cpu or gpu, not \"bogus\"");
    }

    /// ONNX Runtime runs on the CPU: that is the default and the only unit taken.
    #[test]
    #[cfg(target_os = "linux")]
    fn only_the_cpu_is_taken_on_linux() {
        assert_eq!(DEFAULT_UNITS, "cpu");
        for command in ["erase", "select", "upscale"] {
            assert!(check_units(command, "cpu").is_ok());
            let error = check_units(command, "gpu").unwrap_err().to_string();
            assert_eq!(
                error,
                "--units takes only cpu here, where ONNX Runtime runs the models on the CPU, \
                 not \"gpu\""
            );
        }
    }

    #[test]
    fn a_result_too_large_says_to_use_a_smaller_scale() {
        assert!(check_result_size((20000, 2), Factor::X2).is_ok());
        let error = check_result_size((20000, 2), Factor::X4)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "4× would make it 80,000 px long, more than the 65,535 px limit; use a smaller --scale"
        );
        // Not even 1.25× fits: no smaller scale to use.
        let error = check_result_size((60000, 2), Factor::X2)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "2× would make it 120,000 px long, more than the 65,535 px limit; the picture is \
             already as large as upscale can make it"
        );
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
