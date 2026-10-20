// SPDX-License-Identifier: GPL-3.0-or-later

//! `cargo xtask bundle`: builds `Gumka.app` and a disk image for macOS.
//!
//! 1. Checks the downloaded models against `models.toml`.
//! 2. Builds the app in release mode (for this Mac's architecture, or both with `--universal`).
//! 3. Assembles the bundle: the executable, `Info.plist`, the icon (rendered from the logo),
//!    the models compiled to `.mlmodelc`, and the licenses of everything in it.
//! 4. Makes a compressed disk image with a link to `/Applications`.
//!
//! The app isn't signed: the executable keeps the ad-hoc signature the linker gives it, which
//! Apple Silicon needs to run it. See docs/packaging.md.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use gumka_infer::{coreml, models};

const USAGE: &str = "usage: cargo xtask bundle [--universal] [--no-dmg]";

const NAME: &str = "Gumka";
const IDENTIFIER: &str = "io.github.andriiryzhkov.gumka";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// Must match `MACOSX_DEPLOYMENT_TARGET` in `.cargo/config.toml`.
const MINIMUM_MACOS: &str = "13.0";

const ARM: &str = "aarch64-apple-darwin";
const INTEL: &str = "x86_64-apple-darwin";

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut universal, mut dmg) = (false, true);
    for arg in args.by_ref() {
        match arg.as_str() {
            "--universal" => universal = true,
            "--no-dmg" => dmg = false,
            _ => return Err(format!("unknown option {arg:?}\n\n{USAGE}")),
        }
    }
    let root = repository();
    let out = target_dir(&root).join("bundle");
    let started = Instant::now();

    step("checking the models");
    let models_dir = models::repository_dir();
    let bundled = check_models(&models_dir)?;

    step("building the app");
    let (exe, arch) = build(&root, universal)?;

    let app = out.join(format!("{NAME}.app"));
    step(&format!("assembling {}", app.display()));
    if app.exists() {
        fs::remove_dir_all(&app).map_err(|e| format!("{}: {e}", app.display()))?;
    }
    let contents = app.join("Contents");
    let resources = contents.join("Resources");
    create_dir(&contents.join("MacOS"))?;
    create_dir(&resources)?;
    copy(&exe, &contents.join("MacOS").join(NAME))?;
    write(&contents.join("Info.plist"), &info_plist())?;
    write(&contents.join("PkgInfo"), "APPL????")?;
    lint_plist(&contents.join("Info.plist"))?;
    make_icon(&root, &out, &resources.join("AppIcon.icns"))?;
    compile_models(&models_dir, &bundled, &resources.join("models"))?;
    write_licenses(&root, &bundled, universal, &resources.join("licenses"))?;
    println!("  {} ({})", app.display(), size_of(&app));

    if dmg {
        let image = out.join(format!("{NAME}-{VERSION}-{arch}.dmg"));
        step(&format!("making {}", image.display()));
        make_dmg(&app, &out, &image)?;
        println!("  {} ({})", image.display(), size_of(&image));
    }
    println!(
        "\ndone in {:.0} s. The app isn't signed; see docs/packaging.md for opening it on another Mac.",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn step(what: &str) {
    println!("\n{what}");
}

/// The repository root (xtask lives one level below it).
fn repository() -> PathBuf {
    let xtask = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    xtask.parent().map_or(xtask.clone(), Path::to_path_buf)
}

fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from)
}

/// The models for this platform, each checked file by file against the registry.
fn check_models(dir: &Path) -> Result<Vec<&'static models::ModelInfo>, String> {
    let mut problems = Vec::new();
    let bundled: Vec<_> = models::registry()
        .iter()
        .filter(|m| m.for_this_platform())
        .collect();
    for model in &bundled {
        for file in &model.files {
            if let Err(problem) = crate::verify(&dir.join(&file.path), file) {
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

/// Builds the app; returns its executable and the architecture name for the disk image.
fn build(root: &Path, universal: bool) -> Result<(PathBuf, &'static str), String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = target_dir(root);
    let cargo_build = |triple: Option<&str>| -> Result<PathBuf, String> {
        let mut command = Command::new(&cargo);
        command
            .current_dir(root)
            .args(["build", "--release", "-p", "gumka-app"]);
        let exe = match triple {
            Some(triple) => {
                command.args(["--target", triple]);
                target.join(triple).join("release/gumka")
            }
            None => target.join("release/gumka"),
        };
        run_command(&mut command)?;
        Ok(exe)
    };
    if !universal {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        return Ok((cargo_build(None)?, arch));
    }
    let slices = [cargo_build(Some(ARM))?, cargo_build(Some(INTEL))?];
    let fat = target.join("universal-apple-darwin/release/gumka");
    create_dir(target.join("universal-apple-darwin/release").as_path())?;
    run_command(
        Command::new("lipo")
            .arg("-create")
            .args(&slices)
            .arg("-output")
            .arg(&fat),
    )?;
    Ok((fat, "universal"))
}

fn info_plist() -> String {
    let entries = [
        (
            "CFBundleDevelopmentRegion",
            "<string>en</string>".to_owned(),
        ),
        ("CFBundleDisplayName", format!("<string>{NAME}</string>")),
        ("CFBundleExecutable", format!("<string>{NAME}</string>")),
        ("CFBundleIconFile", "<string>AppIcon</string>".to_owned()),
        (
            "CFBundleIdentifier",
            format!("<string>{IDENTIFIER}</string>"),
        ),
        (
            "CFBundleInfoDictionaryVersion",
            "<string>6.0</string>".to_owned(),
        ),
        ("CFBundleName", format!("<string>{NAME}</string>")),
        ("CFBundlePackageType", "<string>APPL</string>".to_owned()),
        (
            "CFBundleShortVersionString",
            format!("<string>{VERSION}</string>"),
        ),
        ("CFBundleVersion", format!("<string>{VERSION}</string>")),
        (
            "LSApplicationCategoryType",
            "<string>public.app-category.photography</string>".to_owned(),
        ),
        (
            "LSMinimumSystemVersion",
            format!("<string>{MINIMUM_MACOS}</string>"),
        ),
        ("NSHighResolutionCapable", "<true/>".to_owned()),
        (
            "NSHumanReadableCopyright",
            "<string>Free software under the GNU GPL, version 3 or later.</string>".to_owned(),
        ),
        ("NSSupportsAutomaticGraphicsSwitching", "<true/>".to_owned()),
        ("CFBundleDocumentTypes", document_types()),
    ];
    let mut plist = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (key, value) in entries {
        plist += &format!("\t<key>{key}</key>\n\t{value}\n");
    }
    plist + "</dict>\n</plist>\n"
}

/// The files Gumka opens, so that it is offered under Open With and as a RAW editor's
/// external editor: as an editor, but never the default app for them (rank Alternate).
fn document_types() -> String {
    let types = [
        ("TIFF image", "public.tiff"),
        ("PNG image", "public.png"),
        ("JPEG image", "public.jpeg"),
    ];
    let mut array = String::from("<array>\n");
    for (name, uti) in types {
        array += &format!(
            "\t\t<dict>\n             \t\t\t<key>CFBundleTypeName</key>\n\t\t\t<string>{name}</string>\n             \t\t\t<key>CFBundleTypeRole</key>\n\t\t\t<string>Editor</string>\n             \t\t\t<key>LSHandlerRank</key>\n\t\t\t<string>Alternate</string>\n             \t\t\t<key>LSItemContentTypes</key>\n             \t\t\t<array>\n\t\t\t\t<string>{uti}</string>\n\t\t\t</array>\n             \t\t</dict>\n"
        );
    }
    array + "\t</array>"
}

fn lint_plist(path: &Path) -> Result<(), String> {
    run_command(Command::new("plutil").arg("-lint").arg(path))
}

/// Renders the logo at every size an `.icns` holds and packs them with `iconutil`. The small
/// logo, drawn for 16 and 32 pixels, stands in at those sizes.
fn make_icon(root: &Path, out: &Path, icns: &Path) -> Result<(), String> {
    let logo = root.join("assets/logo");
    let large = read(&logo.join("gumka-icon.svg"))?;
    let small = read(&logo.join("gumka-icon-small.svg"))?;
    let iconset = out.join("AppIcon.iconset");
    if iconset.exists() {
        fs::remove_dir_all(&iconset).map_err(|e| format!("{}: {e}", iconset.display()))?;
    }
    create_dir(&iconset)?;
    for points in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let pixels = points * scale;
            let svg = if pixels <= 32 { &small } else { &large };
            let name = match scale {
                1 => format!("icon_{points}x{points}.png"),
                _ => format!("icon_{points}x{points}@2x.png"),
            };
            render_png(svg, pixels, &iconset.join(name))?;
        }
    }
    run_command(
        Command::new("iconutil")
            .args(["-c", "icns", "-o"])
            .arg(icns)
            .arg(&iconset),
    )?;
    fs::remove_dir_all(&iconset).map_err(|e| format!("{}: {e}", iconset.display()))
}

fn render_png(svg: &[u8], pixels: u32, path: &Path) -> Result<(), String> {
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

/// Compiles every package of the bundled models into `dir`, as `models.rs` looks for them.
fn compile_models(from: &Path, bundled: &[&models::ModelInfo], dir: &Path) -> Result<(), String> {
    for model in bundled {
        for package in &model.packages {
            let started = Instant::now();
            let dest = dir.join(models::compiled_name(package));
            coreml::compile_into(&from.join(package), &dest)
                .map_err(|e| format!("compiling {package}: {e}"))?;
            println!(
                "  {} ({:.1} s, {})",
                models::compiled_name(package),
                started.elapsed().as_secs_f64(),
                size_of(&dest)
            );
        }
    }
    Ok(())
}

/// The licenses of everything in the bundle: Gumka's own, the models', the fonts', and those
/// of every crate compiled into the app.
fn write_licenses(
    root: &Path,
    bundled: &[&models::ModelInfo],
    universal: bool,
    dir: &Path,
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
        let notice = format!(
            "{name}\nUsed for: {tool}\nSource: {source} ({url})\nLicense: {license}\n\n{text}",
            name = model.name,
            tool = model.tool,
            source = model.source,
            url = model.source_url,
            license = model.license,
        );
        write(&dir.join(format!("models/{}.txt", model.id)), &notice)?;
    }
    let crates = write_crate_licenses(root, universal, &dir.join("crates"))?;
    let readme = format!(
        "{NAME} {VERSION}\n\n\
         {NAME} is free software: you can redistribute it and/or modify it under the terms of the\n\
         GNU General Public License as published by the Free Software Foundation, either version 3\n\
         of the License, or (at your option) any later version. See Gumka.txt.\n\n\
         Source code: {REPOSITORY}\n\n\
         This folder also holds:\n\
         - models/: the bundled models, their sources and licenses\n\
         - fonts/: IBM Plex, under the SIL Open Font License 1.1\n\
         - crates/: the {crates} Rust crates compiled into {NAME}, listed in crates/README.txt\n\
         \x20 with their license files\n"
    );
    write(&dir.join("README.txt"), &readme)
}

/// Copies the license files of every crate the app is built from (normal dependencies, not
/// build tools or procedural macros) and lists them. Returns how many there are.
fn write_crate_licenses(root: &Path, universal: bool, dir: &Path) -> Result<usize, String> {
    let targets: &[&str] = if universal {
        &[ARM, INTEL]
    } else if cfg!(target_arch = "aarch64") {
        &[ARM]
    } else {
        &[INTEL]
    };
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
        "The Rust crates compiled into {NAME} {VERSION}, with their licenses. Each one's license\n\
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

/// A compressed, read-only disk image holding the app and a link to `/Applications`.
fn make_dmg(app: &Path, out: &Path, image: &Path) -> Result<(), String> {
    let staging = out.join("dmg");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    create_dir(&staging)?;
    // `cp -c` clones on APFS: the app isn't copied byte by byte.
    run_command(Command::new("cp").arg("-cR").arg(app).arg(&staging))?;
    std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
        .map_err(|e| format!("{}: {e}", staging.display()))?;
    // hdiutil now and then fails with "Resource busy" (often on CI runners); a retry works.
    let mut result = Ok(());
    for attempt in 1..=3 {
        result = run_command(
            Command::new("hdiutil")
                .args(["create", "-quiet", "-volname", NAME, "-fs", "HFS+"])
                .args(["-format", "UDZO", "-ov", "-srcfolder"])
                .arg(&staging)
                .arg(image),
        );
        if result.is_ok() || attempt == 3 {
            break;
        }
        println!("  hdiutil failed; trying again");
        std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
    }
    let _ = fs::remove_dir_all(&staging);
    result
}

fn run_command(command: &mut Command) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("could not run {:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", command.get_program()))
    }
}

fn create_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn size_of(path: &Path) -> String {
    crate::size(models::disk_size(path))
}
