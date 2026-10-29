// SPDX-License-Identifier: GPL-3.0-or-later

//! What every package of Gumka needs, whatever the platform: the models checked against
//! `models.toml`, the icon rendered from the logo, and the licenses of everything in it. The
//! packages themselves are `bundle.rs` (the macOS app) and `appimage.rs` (Linux).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;

use gumka_infer::models;

pub(crate) const NAME: &str = "Gumka";
pub(crate) const IDENTIFIER: &str = "io.github.andriiryzhkov.gumka";
pub(crate) const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub(crate) fn step(what: &str) {
    println!("\n{what}");
}

/// Gumka's version, from the checkout at `root` (see docs/packaging.md).
pub(crate) fn version(root: &Path) -> Result<gumka_version::Version, String> {
    // A source archive is 0.0.0, not an error: say so, as the package is named after it.
    if !root.join(".git").exists() {
        println!("  not a git checkout: the version is 0.0.0");
    }
    gumka_version::read(root)
}

/// The repository root (xtask lives one level below it).
pub(crate) fn repository() -> PathBuf {
    let xtask = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    xtask.parent().map_or(xtask.clone(), Path::to_path_buf)
}

pub(crate) fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from)
}

/// The models for this platform, each checked file by file against the registry.
pub(crate) fn check_models(dir: &Path) -> Result<Vec<&'static models::ModelInfo>, String> {
    let mut problems = Vec::new();
    let bundled: Vec<_> = models::registry()
        .iter()
        .filter(|m| m.for_this_platform())
        .collect();
    for model in &bundled {
        for file in &model.files {
            if let Err(problem) = crate::verify(&model.root_in(dir).join(&file.path), file) {
                problems.push(format!("  {problem:<9} {}", file.path));
            }
        }
        println!("  {} ({})", model.name, crate::size(model.size()));
    }
    if problems.is_empty() {
        Ok(bundled)
    } else {
        Err(format!(
            "models in {} aren't complete; run `cargo xtask models` first:\n{}",
            dir.display(),
            problems.join("\n")
        ))
    }
}

pub(crate) fn render_png(svg: &[u8], pixels: u32, path: &Path) -> Result<(), String> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default())
        .map_err(|e| format!("the logo doesn't parse: {e}"))?;
    let mut pixmap = tiny_skia::Pixmap::new(pixels, pixels).ok_or("bad icon size")?;
    let scale = pixels as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia keeps premultiplied alpha; PNG wants it unmultiplied.
    let rgba: Vec<u8> = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), pixels, pixels);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .write_image_data(&rgba)
        .and_then(|()| writer.finish())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The licenses of everything in the package: Gumka's own, the models', the fonts', and those
/// of every crate compiled into the app for `targets`. `more` lists, for the README, what else
/// the package put in `dir`.
pub(crate) fn write_licenses(
    root: &Path,
    app_version: &str,
    bundled: &[&models::ModelInfo],
    targets: &[&str],
    dir: &Path,
    more: &str,
) -> Result<(), String> {
    create_dir(&dir.join("models"))?;
    create_dir(&dir.join("fonts"))?;
    copy(&root.join("LICENSE"), &dir.join("Gumka.txt"))?;
    copy(
        &root.join("crates/gumka-app/assets/fonts/OFL.txt"),
        &dir.join("fonts/IBM-Plex.txt"),
    )?;
    for model in bundled {
        let text_path = root.join(format!("assets/licenses/{}.txt", model.license));
        let text = read_text(&text_path).map_err(|e| {
            format!(
                "{e}\n(add the {} license text there for {})",
                model.license, model.name
            )
        })?;
        write(
            &dir.join(format!("models/{}.txt", model.id)),
            &model_notice(model, &text),
        )?;
    }
    let crates = write_crate_licenses(root, app_version, targets, &dir.join("crates"))?;
    let readme = format!(
        "{NAME} {app_version}\n\n\
         {NAME} is free software: you can redistribute it and/or modify it under the terms of the\n\
         GNU General Public License as published by the Free Software Foundation, either version 3\n\
         of the License, or (at your option) any later version. See Gumka.txt.\n\n\
         Source code: {REPOSITORY}\n\n\
         This folder also holds:\n\
         - models/: the bundled models, their sources and licenses\n\
         - fonts/: IBM Plex, under the SIL Open Font License 1.1\n\
         - crates/: the {crates} Rust crates compiled into {NAME}, listed in crates/README.txt\n\
         \x20 with their license files\n{more}"
    );
    write(&dir.join("README.txt"), &readme)
}

/// A bundled model's license file: what the model is and where it comes from, then its license,
/// with the copyright lines that licenses such as MIT go with.
fn model_notice(model: &models::ModelInfo, text: &str) -> String {
    let mut copyright = model.copyright.join("\n");
    if !copyright.is_empty() {
        copyright += "\n\n";
    }
    format!(
        "{name}\nUsed for: {tool}\nSource: {source} ({url})\nLicense: {license}\n\n{copyright}{text}",
        name = model.name,
        tool = model.tool,
        source = model.source,
        url = model.source_url,
        license = model.license,
    )
}

/// Copies the license files of every crate the app is built from (normal dependencies, not
/// build tools or procedural macros) and lists them. Returns how many there are.
fn write_crate_licenses(
    root: &Path,
    app_version: &str,
    targets: &[&str],
    dir: &Path,
) -> Result<usize, String> {
    // name version -> (license, repository)
    let mut crates = BTreeMap::new();
    for target in targets {
        let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(root)
            .args(["tree", "-p", "gumka-app", "-e", "normal,no-proc-macro"])
            .args([
                "--target",
                target,
                "--prefix",
                "none",
                "--format",
                "{p}|{l}|{r}",
            ])
            .output()
            .map_err(|e| format!("could not run cargo tree: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "cargo tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.trim_end_matches(" (*)");
            let mut fields = line.splitn(3, '|');
            let (Some(package), Some(license), Some(repository)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            // The workspace's own crates are Gumka.
            if package.contains(" (") {
                continue;
            }
            let Some((name, version)) = package.split_once(" v") else {
                continue;
            };
            crates.insert(
                (name.to_owned(), version.to_owned()),
                (license.to_owned(), repository.to_owned()),
            );
        }
    }
    let registry = cargo_home().join("registry/src");
    let sources: Vec<PathBuf> = fs::read_dir(&registry)
        .map_err(|e| format!("{}: {e}", registry.display()))?
        .flatten()
        .map(|e| e.path())
        .collect();
    let mut list = format!(
        "The Rust crates compiled into {NAME} {app_version}, with their licenses. Each one's license\n\
         files, as published with it, are in the folder of the same name.\n\n"
    );
    let mut without = Vec::new();
    for ((name, version), (license, repository)) in &crates {
        let folder = format!("{name}-{version}");
        let source = sources.iter().map(|s| s.join(&folder)).find(|s| s.is_dir());
        let mut copied = 0;
        if let Some(source) = &source {
            // Vendored C libraries carry their own license next to their sources, and bundled
            // fonts theirs next to the fonts.
            let fonts = fs::read_dir(source.join("fonts"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "txt"));
            let files = license_files(source)
                .into_iter()
                .chain(license_files(&source.join("vendor")))
                .chain(fonts);
            for file in files {
                let relative = file.strip_prefix(source).unwrap_or(&file);
                let dest = dir.join(&folder).join(relative);
                if let Some(parent) = dest.parent() {
                    create_dir(parent)?;
                }
                copy(&file, &dest)?;
                copied += 1;
            }
        }
        if license_files(&dir.join(&folder)).is_empty() {
            // Published without its own (the project keeps it at its repository's root): the
            // standard texts of the licenses it names.
            for (name, text) in standard_licenses(root, license, source.as_deref())? {
                create_dir(&dir.join(&folder))?;
                write(&dir.join(&folder).join(name), &text)?;
                copied += 1;
            }
        }
        if copied == 0 {
            without.push(folder.clone());
        }
        list += &format!("{name} {version}\n  License: {license}\n  Source: {repository}\n");
    }
    if !without.is_empty() {
        list += &format!(
            "\nPublished without a license file (their license is named above): {}\n",
            without.join(", ")
        );
        println!(
            "  {} of {} crates have no license file: {}",
            without.len(),
            crates.len(),
            without.join(", ")
        );
    }
    create_dir(dir)?;
    write(&dir.join("README.txt"), &list)?;
    Ok(crates.len())
}

/// For a crate published without license files: the standard text of each license its
/// `license` expression names that Gumka has (Apache-2.0, MIT), as file name and text. The MIT
/// text gets a copyright line naming the crate's authors (from its `Cargo.toml` in `source`).
fn standard_licenses(
    root: &Path,
    license: &str,
    source: Option<&Path>,
) -> Result<Vec<(&'static str, String)>, String> {
    let names = |id: &str| {
        license
            .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '.'))
            .any(|token| token == id)
    };
    let mut texts = Vec::new();
    if names("Apache-2.0") {
        let text = read_text(&root.join("assets/licenses/Apache-2.0.txt"))?;
        texts.push(("LICENSE-APACHE", text));
    }
    if names("MIT") {
        let authors = source
            .and_then(|s| fs::read_to_string(s.join("Cargo.toml")).ok())
            .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
            .and_then(|manifest| {
                let authors = manifest.get("package")?.get("authors")?.as_array()?.clone();
                let names: Vec<String> = authors
                    .iter()
                    .filter_map(|a| a.as_str())
                    // The names, without their e-mail addresses.
                    .map(|a| a.split('<').next().unwrap_or(a).trim().to_owned())
                    .filter(|a| !a.is_empty())
                    .collect();
                (!names.is_empty()).then(|| names.join(", "))
            });
        let holder = authors.unwrap_or_else(|| "the authors".into());
        let text = read_text(&root.join("assets/licenses/MIT.txt"))?;
        texts.push(("LICENSE-MIT", format!("Copyright (c) {holder}\n\n{text}")));
    }
    Ok(texts)
}

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map_or_else(PathBuf::new, PathBuf::from)
                .join(".cargo")
        },
        PathBuf::from,
    )
}

/// Files in `dir` (not below it) that hold a license or notice.
fn license_files(dir: &Path) -> Vec<PathBuf> {
    const NAMES: [&str; 6] = [
        "license",
        "licence",
        "copying",
        "notice",
        "unlicense",
        "copyright",
    ];
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| {
            path.is_file()
                && path.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy().to_lowercase();
                    NAMES.iter().any(|name| n.starts_with(name))
                })
        })
        .collect();
    files.sort();
    files
}

pub(crate) fn run_command(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("could not run {:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", command.get_program()))
    }
}

pub(crate) fn create_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

pub(crate) fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))
}

pub(crate) fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn size_of(path: &Path) -> String {
    crate::size(models::disk_size(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_notice_carries_its_copyright_before_the_license() {
        let upscaler = models::model(models::UPSCALE_2X).unwrap();
        let notice = model_notice(upscaler, "Permission is hereby granted…");
        assert!(
            notice.starts_with("RealPLKSR 2×\nUsed for: Upscale\n"),
            "{notice}"
        );
        assert!(
            notice.ends_with(
                "License: MIT\n\nCopyright (c) 2024 Dongheon Lee\n\nPermission is hereby granted…"
            ),
            "{notice}"
        );

        let lama = models::model(models::LAMA).unwrap();
        let notice = model_notice(lama, "Apache License");
        assert!(
            notice.ends_with("License: Apache-2.0\n\nApache License"),
            "{notice}"
        );
    }
}
