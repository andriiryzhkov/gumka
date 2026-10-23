// SPDX-License-Identifier: GPL-3.0-or-later

//! What `gumka-cli upscale` refuses before it loads a model, run as the binary itself: the
//! Neural Engine (on macOS; on Linux anything but the CPU), and a result past the size
//! limits. Neither needs the models.

use std::path::{Path, PathBuf};
use std::process::Output;

use gumka_core::image::GrayImage;

/// Runs `gumka-cli upscale INPUT OUTPUT MORE...` with its home in INPUT's folder, so that a
/// model it loads by mistake is compiled there, not into the user's cache.
fn cli(input: &Path, output: &Path, more: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gumka-cli"))
        .env("HOME", input.parent().unwrap())
        .arg("upscale")
        .args([input, output])
        .args(more)
        .output()
        .unwrap()
}

/// An empty folder of its own for each test.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gumka-cli-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn error(out: &Output) -> String {
    assert!(!out.status.success());
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}

#[test]
#[cfg(target_os = "macos")]
fn upscale_refuses_the_neural_engine_first() {
    let dir = folder("units");
    let (input, output) = (dir.join("missing.png"), dir.join("out.png"));
    for units in ["ane", "all"] {
        let out = cli(&input, &output, &["--scale", "2", "--units", units]);
        assert_eq!(
            error(&out),
            "error: RealPLKSR gives wrong results on the Neural Engine; use --units gpu or cpu"
        );
    }
    // The GPU and the CPU get as far as the input, which is missing.
    for units in ["gpu", "cpu"] {
        let out = cli(&input, &output, &["--scale", "2", "--units", units]);
        let error = error(&out);
        assert!(!error.contains("Neural Engine"), "{units}: {error}");
    }
    assert!(!output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn upscale_refuses_a_result_too_large_before_loading_a_model() {
    let dir = folder("too-large");
    let (input, output) = (dir.join("wide.png"), dir.join("out.png"));
    GrayImage::new(20000, 2).save(&input).unwrap();
    let out = cli(&input, &output, &["--scale", "4"]);
    assert_eq!(
        error(&out),
        "error: 4× would make it 80,000 px long, more than the 65,535 px limit; use a smaller \
         --scale"
    );
    assert!(out.stdout.is_empty(), "no model loaded");
    assert!(!output.exists());

    // Not even the smallest scale fits: no smaller one to suggest.
    let huge = dir.join("huge.png");
    GrayImage::new(60000, 2).save(&huge).unwrap();
    let out = cli(&huge, &output, &["--scale", "1.25"]);
    assert_eq!(
        error(&out),
        "error: 1.25× would make it 75,000 px long, more than the 65,535 px limit; the \
         picture is already as large as upscale can make it"
    );
    assert!(out.stdout.is_empty() && !output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg(target_os = "macos")]
fn unknown_units_get_a_specific_error() {
    let dir = folder("bogus-units");
    let (input, output) = (dir.join("missing.png"), dir.join("out.png"));
    let out = cli(&input, &output, &["--scale", "2", "--units", "bogus"]);
    assert_eq!(
        error(&out),
        "error: --units takes cpu or gpu, not \"bogus\""
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// On Linux ONNX Runtime runs on the CPU: other units are refused, the CPU gets as far as the
/// input, which is missing.
#[test]
#[cfg(target_os = "linux")]
fn upscale_takes_only_the_cpu_on_linux() {
    let dir = folder("cpu-only");
    let (input, output) = (dir.join("missing.png"), dir.join("out.png"));
    for units in ["gpu", "ane"] {
        let out = cli(&input, &output, &["--scale", "2", "--units", units]);
        assert_eq!(
            error(&out),
            format!(
                "error: --units takes only cpu here, where ONNX Runtime runs the models on the \
                 CPU, not \"{units}\""
            )
        );
    }
    let out = cli(&input, &output, &["--scale", "2", "--units", "cpu"]);
    let error = error(&out);
    assert!(!error.contains("--units"), "{error}");
    assert!(!output.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
