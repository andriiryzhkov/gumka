// SPDX-License-Identifier: GPL-3.0-or-later

//! Development tasks, run as `cargo xtask <task>`.
//!
//! `models` downloads the models listed in `models.toml` into the repository's `models/` and
//! checks each file's size and SHA-256. Files that are already there and correct are kept, so
//! running it again is cheap. Downloads go through `curl`, which macOS, Windows 10 and later,
//! and Linux distributions ship.
//!
//! `bundle` builds the macOS app and its disk image (see `bundle.rs`).

#[cfg(target_os = "macos")]
mod bundle;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use gumka_infer::models::{self, ModelFile};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: cargo xtask models [--check] [--all] [--dir DIR]
       cargo xtask bundle [--universal] [--no-dmg]

  models  Downloads the models in models.toml into models/ and checks each file's SHA-256.
          Files that are already there and correct are kept.
          --check  only verify; fail if anything is missing or wrong
          --all    include models for other platforms
          --dir    use DIR instead of the repository's models/

  bundle  Builds Gumka.app and its disk image in target/bundle (macOS; not signed).
          --universal  for Apple Silicon and Intel Macs in one app
          --no-dmg     the app only";

/// Files at least this big get a progress bar.
const PROGRESS_FROM: u64 = 10_000_000;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether the task succeeded.
fn run() -> Result<bool, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("models") => fetch_models(args),
        Some("bundle") => bundle(args),
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            Ok(true)
        }
        Some(other) => Err(format!("unknown task {other:?}\n\n{USAGE}")),
    }
}

#[cfg(target_os = "macos")]
fn bundle(args: impl Iterator<Item = String>) -> Result<bool, String> {
    bundle::run(args).map(|()| true)
}

#[cfg(not(target_os = "macos"))]
fn bundle(_args: impl Iterator<Item = String>) -> Result<bool, String> {
    Err("`cargo xtask bundle` builds the macOS app; run it on a Mac".into())
}

/// `cargo xtask models`. Returns whether every file is in place.
fn fetch_models(mut args: impl Iterator<Item = String>) -> Result<bool, String> {
    let (mut check, mut all, mut dir) = (false, false, models::repository_dir());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--all" => all = true,
            "--dir" => dir = PathBuf::from(args.next().ok_or(USAGE)?),
            _ => return Err(format!("unknown option {arg:?}\n\n{USAGE}")),
        }
    }
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dir = dir.canonicalize().unwrap_or(dir);
    println!("models in {}", dir.display());

    let mut complete = true;
    for model in models::registry() {
        if !all && !model.for_this_platform() {
            println!("\n{}: skipped, runs on {}", model.name, model.platform);
            continue;
        }
        println!(
            "\n{} · {} · {} · {}",
            model.name,
            model.license,
            model.source,
            size(model.size())
        );
        let root = model.root_in(&dir);
        for file in &model.files {
            let target = root.join(&file.path);
            let shown = target.strip_prefix(&dir).unwrap_or(&target).display();
            match verify(&target, file) {
                Ok(()) => {
                    println!("  ok        {shown}");
                    continue;
                }
                Err(problem) if check => {
                    println!("  {problem:<9} {shown}");
                    complete = false;
                    continue;
                }
                Err(_) => {}
            }
            println!("  download  {shown} ({})", size(file.size));
            download(&format!("{}{}", model.base_url, file.path), &target, file)?;
        }
    }
    if !complete {
        println!("\nSome files are missing or wrong; run `cargo xtask models` to fetch them.");
    }
    Ok(complete)
}

/// Checks a file against the registry: present, the right size, the right SHA-256.
pub(crate) fn verify(path: &Path, file: &ModelFile) -> Result<(), &'static str> {
    let meta = fs::metadata(path).map_err(|_| "missing")?;
    if meta.len() != file.size {
        return Err("wrong size");
    }
    let hash = sha256(path).map_err(|_| "unreadable")?;
    if !hash.eq_ignore_ascii_case(&file.sha256) {
        return Err("corrupt");
    }
    Ok(())
}

fn sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Downloads to a `.part` file next to the target, checks it, then moves it into place.
fn download(url: &str, target: &Path, file: &ModelFile) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let partial = PathBuf::from(format!("{}.part", target.display()));
    let progress = if file.size >= PROGRESS_FROM {
        "--progress-bar"
    } else {
        "--silent"
    };
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--retry",
            "3",
            "--show-error",
            progress,
        ])
        .arg("--output")
        .arg(&partial)
        .arg(url)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        return Err(format!("download failed: {url}"));
    }
    if let Err(problem) = verify(&partial, file) {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "{}: the download is {problem} (expected {} bytes, SHA-256 {})",
            file.path, file.size, file.sha256
        ));
    }
    fs::rename(&partial, target).map_err(|e| format!("{}: {e}", target.display()))
}

pub(crate) fn size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{:.0} KB", bytes as f64 / 1e3)
    }
}
