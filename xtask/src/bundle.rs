// SPDX-License-Identifier: GPL-3.0-or-later

//! `cargo xtask bundle` on macOS: builds `Gumka.app` and a disk image.
//!
//! 1. Checks the downloaded models against `models.toml`.
//! 2. Builds the app in release mode (for this Mac's architecture, or both with `--universal`).
//! 3. Assembles the bundle: the executable, `Info.plist`, the icon (rendered from the logo),
//!    the models compiled to `.mlmodelc`, and the licenses of everything in it.
//! 4. Makes a compressed disk image with a link to `/Applications`.
//!
//! The app isn't signed: the executable keeps the ad-hoc signature the linker gives it, which
//! Apple Silicon needs to run it. See docs/packaging.md.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use gumka_infer::{coreml, models};

use crate::package::{
    IDENTIFIER, NAME, VERSION, check_models, copy, create_dir, read, render_png, repository,
    run_command, size_of, step, target_dir, write, write_licenses,
};

const USAGE: &str = "usage: cargo xtask bundle [--universal] [--no-dmg]";

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
    let targets: &[&str] = if universal {
        &[ARM, INTEL]
    } else if cfg!(target_arch = "aarch64") {
        &[ARM]
    } else {
        &[INTEL]
    };
    write_licenses(&root, &bundled, targets, &resources.join("licenses"), "")?;
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

/// Compiles every package of the bundled models into `dir`, as `models.rs` looks for them.
fn compile_models(from: &Path, bundled: &[&models::ModelInfo], dir: &Path) -> Result<(), String> {
    for model in bundled {
        for package in &model.packages {
            let started = Instant::now();
            let dest = dir.join(models::compiled_name(package));
            coreml::compile_into(&model.root_in(from).join(package), &dest)
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
