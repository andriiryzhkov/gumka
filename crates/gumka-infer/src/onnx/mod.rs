// SPDX-License-Identifier: GPL-3.0-or-later

//! ONNX Runtime (Linux): the system's library, loaded when the first model is.
//!
//! Gumka neither links nor ships ONNX Runtime: `ort` opens `libonnxruntime` at run time, so a
//! build needs none, and without it the models are only unavailable, with a message that says
//! what to install. `$ORT_DYLIB_PATH` names the library to use instead. Models run on the CPU.

mod lama;
mod sam;
mod upscale;

pub use lama::OnnxLama;
pub use sam::OnnxSam;
pub use upscale::OnnxUpscaler;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gumka_core::image::RgbImage;
use ort::environment::Environment;
use ort::session::Session;
use ort::value::{Outlet, TensorElementType, ValueType};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// The oldest ONNX Runtime `ort` takes, as built (API 17).
const MIN_MINOR: u32 = ort::MINOR_VERSION;
/// The newest minor version looked for under Debian's and Ubuntu's library names.
const MAX_MINOR: u32 = 40;

/// Where models run, for the app's status.
pub const RUNS_ON: &str = "ONNX Runtime · CPU";

/// ONNX Runtime's environment, or why it didn't load. A failure is kept: the app loads its
/// models once, so ONNX Runtime installed later takes a restart.
static LOADED: OnceLock<std::result::Result<Environment, String>> = OnceLock::new();

/// Loads ONNX Runtime, once: its environment, or the reason it isn't there.
pub fn runtime() -> std::result::Result<&'static Environment, String> {
    LOADED
        .get_or_init(|| load(&library_names(std::env::var_os("ORT_DYLIB_PATH"))))
        .as_ref()
        .map_err(Clone::clone)
}

/// Why ONNX Runtime didn't load, once a model has tried; never loads it, so the UI can ask.
pub fn problem() -> Option<&'static str> {
    LOADED.get()?.as_ref().err().map(String::as_str)
}

/// The libraries to try, in order: `overriding` alone if it is set, else ONNX Runtime's own
/// name, Debian's and Ubuntu's (one per minor version), and the development link.
fn library_names(overriding: Option<OsString>) -> Vec<PathBuf> {
    if let Some(path) = overriding.filter(|p| !p.is_empty()) {
        return vec![path.into()];
    }
    let mut names = vec![PathBuf::from("libonnxruntime.so.1")];
    names.extend(
        (MIN_MINOR..=MAX_MINOR)
            .rev()
            .map(|minor| PathBuf::from(format!("libonnxruntime.so.1.{minor}"))),
    );
    names.push("libonnxruntime.so".into());
    names
}

fn load(names: &[PathBuf]) -> std::result::Result<Environment, String> {
    let mut problems = Vec::new();
    for name in names {
        match ort::init_from(name) {
            Ok(builder) => {
                return builder
                    .with_name("gumka")
                    .build()
                    .map_err(|e| format!("ONNX Runtime didn't start: {e}"));
            }
            // Not there: try the next name. One that is there but doesn't open (a library it
            // needs is missing, too old a C library) says why.
            Err(ort::LoadDynamicError::Dlopen { error, path })
                if names.len() > 1 && absent(&dlerror(&error), &path) => {}
            Err(ort::LoadDynamicError::Dlopen { error, path }) => {
                // A missing library it needs is named in the message, the one opened isn't.
                let message = dlerror(&error);
                let name = path.display().to_string();
                problems.push(if message.contains(&name) {
                    message
                } else {
                    format!("{name}: {message}")
                });
            }
            Err(error) => problems.push(error.to_string()),
        }
    }
    if problems.is_empty() {
        Err(format!(
            "ONNX Runtime is not installed. Install it with your package manager \
             (libonnxruntime, 1.{MIN_MINOR} or later) or set ORT_DYLIB_PATH to its library, \
             then restart Gumka."
        ))
    } else {
        Err(format!("ONNX Runtime didn't load: {}", problems.join("; ")))
    }
}

/// dlopen's own message, which the loader keeps as the error's source.
fn dlerror(error: &dyn std::error::Error) -> String {
    error
        .source()
        .map_or_else(|| error.to_string(), ToString::to_string)
}

/// Whether dlopen's `message` says `path` itself isn't there, not a library it needs.
fn absent(message: &str, path: &Path) -> bool {
    message.starts_with(&format!(
        "{}: cannot open shared object file",
        path.display()
    ))
}

/// Opens a model on the CPU, loading ONNX Runtime first.
pub fn session(path: &Path) -> Result<Session> {
    Session::builder(runtime()?)
        .and_then(|mut builder| builder.commit_from_file(path))
        .map_err(|e| format!("{}: {e}", path.display()).into())
}

/// The shape of a float32 tensor among `outlets` (-1 for a size left open), if there is one
/// of that name.
fn float_shape(outlets: &[Outlet], name: &str) -> Option<Vec<i64>> {
    match outlets.iter().find(|o| o.name() == name)?.dtype() {
        ValueType::Tensor {
            ty: TensorElementType::Float32,
            shape,
            ..
        } => Some(shape.to_vec()),
        _ => None,
    }
}

/// An 8-bit image as planar float RGB (every red value, then green, then blue), each channel
/// `c`'s values mapped by `map(c, value)`.
fn planar(image: &RgbImage, map: impl Fn(usize, u8) -> f32) -> Vec<f32> {
    let plane = image.width() as usize * image.height() as usize;
    let mut out = vec![0.0; 3 * plane];
    for (i, pixel) in image.pixels().enumerate() {
        for c in 0..3 {
            out[c * plane + i] = map(c, pixel.0[c]);
        }
    }
    out
}

/// Planar float RGB from 0 to `max` as an 8-bit image, if there are `3 * width * height`
/// values.
fn from_planar(values: &[f32], width: u32, height: u32, max: f32) -> Option<RgbImage> {
    let plane = width as usize * height as usize;
    if values.len() != 3 * plane {
        return None;
    }
    let to_u8 = |v: f32| (v / max * 255.0).round().clamp(0.0, 255.0) as u8;
    let mut raw = Vec::with_capacity(3 * plane);
    for i in 0..plane {
        raw.extend((0..3).map(|c| to_u8(values[c * plane + i])));
    }
    RgbImage::from_raw(width, height, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::image::Rgb;

    #[test]
    fn the_override_is_the_only_library_tried() {
        assert_eq!(
            library_names(Some("/opt/ort/libonnxruntime.so".into())),
            [PathBuf::from("/opt/ort/libonnxruntime.so")]
        );
        let names = library_names(Some(OsString::new()));
        assert_eq!(names[0], PathBuf::from("libonnxruntime.so.1"));
        assert!(
            names.contains(&PathBuf::from("libonnxruntime.so.1.23")),
            "Ubuntu's"
        );
        assert!(!names.contains(&PathBuf::from(format!(
            "libonnxruntime.so.1.{}",
            MIN_MINOR - 1
        ))));
        assert_eq!(names.last(), Some(&PathBuf::from("libonnxruntime.so")));
        assert_eq!(library_names(None), names);
    }

    /// Without the library, the reason says what to install and to restart; the one library
    /// asked for says why it didn't load.
    #[test]
    fn a_missing_runtime_says_what_to_install() {
        let missing = [PathBuf::from("libgumka-no-such-library.so")];
        let error = load(&missing).unwrap_err();
        assert!(
            error.starts_with("ONNX Runtime didn't load: libgumka-no-such-library.so: cannot open"),
            "{error}"
        );
        let missing = [missing[0].clone(), PathBuf::from("libgumka-nor-this.so")];
        let error = load(&missing).unwrap_err();
        assert!(
            error.starts_with("ONNX Runtime is not installed") && error.contains("ORT_DYLIB_PATH"),
            "{error}"
        );
        assert!(error.ends_with("then restart Gumka."), "{error}");
    }

    /// A library that is there but doesn't open isn't taken for a missing one: the reason
    /// names it and says what went wrong.
    #[test]
    fn a_library_that_does_not_open_says_why() {
        let broken =
            std::env::temp_dir().join(format!("libgumka-broken-{}.so", std::process::id()));
        std::fs::write(&broken, b"not a library").unwrap();
        let names = [broken.clone(), PathBuf::from("libgumka-no-such-library.so")];
        let error = load(&names).unwrap_err();
        std::fs::remove_file(&broken).unwrap();
        assert!(
            error.starts_with("ONNX Runtime didn't load")
                && error.contains(&*broken.to_string_lossy())
                && !error.contains("dlopen failed"),
            "{error}"
        );
        assert!(!error.contains("libgumka-no-such-library"), "{error}");
    }

    #[test]
    fn only_a_missing_file_counts_as_absent() {
        let path = Path::new("libonnxruntime.so.1");
        assert!(absent(
            "libonnxruntime.so.1: cannot open shared object file: No such file or directory",
            path
        ));
        // A library it needs is missing: the message names that one.
        assert!(!absent(
            "libonnx.so.1: cannot open shared object file: No such file or directory",
            path
        ));
        assert!(!absent("libonnxruntime.so.1: file too short", path));
    }

    #[test]
    fn planar_round_trips() {
        let image = RgbImage::from_fn(3, 2, |x, y| Rgb([x as u8 * 80, y as u8 * 200, 7]));
        let values = planar(&image, |_, v| f32::from(v));
        assert_eq!(
            &values[..6],
            &[0.0, 80.0, 160.0, 0.0, 80.0, 160.0],
            "reds first"
        );
        assert_eq!(&values[12..], &[7.0; 6], "then greens, then blues");
        assert_eq!(from_planar(&values, 3, 2, 255.0), Some(image));
        assert_eq!(
            from_planar(&values[1..], 3, 2, 255.0),
            None,
            "too few values"
        );
        // Values beyond the range are clamped.
        let out = from_planar(&[-5.0, 300.0, 0.5, 0.5, 1.0, 1.0], 2, 1, 1.0).unwrap();
        assert_eq!(out.as_raw(), &[0, 128, 255, 255, 128, 255]);
    }
}
