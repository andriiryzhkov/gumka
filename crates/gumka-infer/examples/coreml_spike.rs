// SPDX-License-Identifier: GPL-3.0-or-later

//! Core ML spike: compile and cache a model, load it, print its inputs and outputs, and
//! optionally run LaMa on an image and mask.
//!
//! ```text
//! cargo run --release -p gumka-infer --example coreml_spike -- MODEL.mlpackage \
//!     [--units cpu|gpu|ane|all] [IMAGE.png MASK.png OUT.png [REFERENCE.png]]
//! ```
//!
//! Without image arguments it only describes the model. IMAGE and MASK are resized to the
//! model's input size if needed, and the result is resized back. REFERENCE is an earlier
//! result to compare against, for example from another backend or compute unit.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    match spike::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("coreml_spike: Core ML is only available on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod spike {
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use gumka_infer::coreml::{self, ComputeUnits, Feature, FeatureKind, Model, PixelBuffer};
    use image::RgbImage;
    use image::imageops::{self, FilterType};

    const USAGE: &str = "usage: coreml_spike MODEL.mlpackage [--units cpu|gpu|ane|all] \
                         [IMAGE.png MASK.png OUT.png [REFERENCE.png]]";
    const RUNS: usize = 3;

    pub fn run() -> Result<(), Box<dyn Error>> {
        let mut units = ComputeUnits::CpuAndGpu;
        let mut paths = Vec::new();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--units" => {
                    units = match args.next().as_deref() {
                        Some("cpu") => ComputeUnits::CpuOnly,
                        Some("gpu") => ComputeUnits::CpuAndGpu,
                        Some("ane") => ComputeUnits::CpuAndNeuralEngine,
                        Some("all") => ComputeUnits::All,
                        _ => return Err(USAGE.into()),
                    }
                }
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return Ok(());
                }
                _ => paths.push(PathBuf::from(arg)),
            }
        }
        if !matches!(paths.len(), 1 | 4 | 5) {
            return Err(USAGE.into());
        }

        let cache = directories::ProjectDirs::from("", "", "Gumka")
            .ok_or("no home directory")?
            .cache_dir()
            .join("models");
        let t = Instant::now();
        let model = coreml::load_cached(&paths[0], &cache, units)?;
        println!(
            "loaded with {units:?}, compiled into {} if needed ({:.2}s)",
            cache.display(),
            secs(t)
        );

        let inputs = model.inputs();
        let outputs = model.outputs();
        println!("inputs:");
        inputs.iter().for_each(|f| println!("  {f}"));
        println!("outputs:");
        outputs.iter().for_each(|f| println!("  {f}"));

        if paths.len() > 1 {
            erase(
                &model,
                &inputs,
                &paths[1],
                &paths[2],
                &paths[3],
                paths.get(4),
            )?;
        }
        Ok(())
    }

    /// Runs LaMa: inputs `image` (BGRA) and `mask` (one channel), output `output` (BGRA).
    fn erase(
        model: &Model,
        inputs: &[Feature],
        image_path: &Path,
        mask_path: &Path,
        out_path: &Path,
        reference: Option<&PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        let (width, height) = image_size(inputs, "image")?;
        if image_size(inputs, "mask")? != (width, height) {
            return Err("image and mask inputs differ in size".into());
        }

        let image = image::open(image_path)?.to_rgb8();
        let mask = image::open(mask_path)?.to_luma8();
        if image.dimensions() != mask.dimensions() {
            return Err("image and mask files differ in size".into());
        }
        let original = image.dimensions();
        let resize = original != (width as u32, height as u32);
        let (image, mask) = if resize {
            println!(
                "resizing {}x{} to the model's {width}x{height}",
                original.0, original.1
            );
            (
                imageops::resize(&image, width as u32, height as u32, FilterType::Lanczos3),
                imageops::resize(&mask, width as u32, height as u32, FilterType::Nearest),
            )
        } else {
            (image, mask)
        };

        let image = PixelBuffer::from_rgb8(width, height, image.as_raw())?;
        let mask = PixelBuffer::from_gray8(width, height, mask.as_raw())?;

        let mut result = None;
        for run in 1..=RUNS {
            let t = Instant::now();
            let prediction = model.predict(&[("image", &image), ("mask", &mask)])?;
            let output = prediction.image("output")?;
            println!("predict run {run}: {:.3}s", secs(t));
            result = Some(output);
        }
        let output = result.expect("RUNS > 0");
        let mut output = RgbImage::from_raw(
            output.width() as u32,
            output.height() as u32,
            output.to_rgb8()?,
        )
        .ok_or("output size mismatch")?;
        if resize {
            output = imageops::resize(&output, original.0, original.1, FilterType::Lanczos3);
        }
        output.save(out_path)?;
        println!("saved {}", out_path.display());

        if let Some(reference) = reference {
            compare(&output, &image::open(reference)?.to_rgb8())?;
        }
        Ok(())
    }

    fn image_size(inputs: &[Feature], name: &str) -> Result<(usize, usize), Box<dyn Error>> {
        match inputs.iter().find(|f| f.name == name).map(|f| &f.kind) {
            Some(FeatureKind::Image { width, height, .. }) => Ok((*width, *height)),
            _ => Err(format!("model has no image input called {name:?}; is this LaMa?").into()),
        }
    }

    fn compare(output: &RgbImage, reference: &RgbImage) -> Result<(), Box<dyn Error>> {
        if output.dimensions() != reference.dimensions() {
            return Err("reference differs in size".into());
        }
        let (mut max, mut sum, mut differing) = (0u8, 0u64, 0usize);
        for (a, b) in output.as_raw().iter().zip(reference.as_raw()) {
            let d = a.abs_diff(*b);
            max = max.max(d);
            sum += u64::from(d);
            differing += usize::from(d > 0);
        }
        let n = output.as_raw().len();
        println!(
            "vs reference: max |diff| {max}, mean {:.4}, {differing} of {n} values differ",
            sum as f64 / n as f64
        );
        Ok(())
    }

    fn secs(t: Instant) -> f64 {
        t.elapsed().as_secs_f64()
    }
}
