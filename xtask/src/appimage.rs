// SPDX-License-Identifier: GPL-3.0-or-later

//! `cargo xtask bundle` on Linux: builds `Gumka-<version>-x86_64.AppImage`.
//!
//! 1. Checks the downloaded models against `models.toml`.
//! 2. Downloads ONNX Runtime and the AppImage runtime (pinned below, checked by size and
//!    SHA-256) into `target/bundle/downloads`, once.
//! 3. Builds the app in release mode.
//! 4. Assembles `Gumka.AppDir`: `usr/bin/gumka`, ONNX Runtime in `usr/lib`, the models in
//!    `usr/share/gumka/models`, the desktop file and icons, the licenses of everything in it,
//!    and `AppRun`, which turns ONNX Runtime's telemetry off and starts the app.
//! 5. Packs it with `mksquashfs` behind the AppImage runtime, with the update information in
//!    the runtime, and makes the `.zsync` file an updater needs, with `zsyncmake`.
//!
//! An AppImage runs on the glibc it was built with or newer, so releases are built on the
//! oldest Ubuntu the CI has. See docs/packaging.md.

use std::fs;
use std::ops::Range;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use gumka_infer::models::{self, ModelFile};

use crate::package::{
    IDENTIFIER, NAME, REPOSITORY, check_models, copy, create_dir, read, render_png, repository,
    run_command, size_of, step, target_dir, version, write, write_licenses,
};

const USAGE: &str = "usage: cargo xtask bundle [--nightly] [--no-appimage]";

/// The nightly AppImage's name in place of a version: the same from one build to the next.
const NIGHTLY: &str = "nightly";

const TARGET: &str = "x86_64-unknown-linux-gnu";

/// A file the package is made with, pinned like the models.
struct Download {
    url: &'static str,
    size: u64,
    sha256: &'static str,
}

const ONNX_RUNTIME_VERSION: &str = "1.31.0";

/// ONNX Runtime's own CPU build (MIT). It needs glibc 2.28 and libstdc++ 6 or later.
const ONNX_RUNTIME: Download = Download {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-x64-1.31.0.tgz",
    size: 11_408_003,
    sha256: "cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57",
};

/// The type 2 AppImage runtime (MIT), linked statically with the libraries whose licenses are
/// in `assets/licenses/appimage-runtime`. It needs no libfuse on the computer it runs on.
const RUNTIME: Download = Download {
    url: "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64",
    size: 944_632,
    sha256: "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d",
};

/// The AppImage's entry point. Microsoft's ONNX Runtime keeps a device id and records usage
/// data to send unless `ORT_DISABLE_TELEMETRY` is set before it starts, and only the
/// environment can say so that early: nothing leaves the computer.
const APP_RUN: &str = "#!/bin/sh\n\
    export ORT_DISABLE_TELEMETRY=1\n\
    exec \"$(dirname \"$(readlink -f \"$0\")\")/usr/bin/gumka\" \"$@\"\n";

/// Icon sizes for the desktop's icon theme; the small logo, drawn for 16 and 32 pixels, stands
/// in at those.
const ICON_SIZES: [u32; 7] = [16, 32, 48, 64, 128, 256, 512];

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut appimage, mut nightly) = (true, false);
    for arg in args.by_ref() {
        match arg.as_str() {
            "--nightly" => nightly = true,
            "--no-appimage" => appimage = false,
            _ => return Err(format!("unknown option {arg:?}\n\n{USAGE}")),
        }
    }
    if !cfg!(target_arch = "x86_64") {
        return Err("the AppImage is made for x86_64 only, for now".into());
    }
    let root = repository();
    let out = target_dir(&root).join("bundle");
    let downloads = out.join("downloads");
    let started = Instant::now();
    let version = version(&root)?;
    println!("{NAME} {} (build {})", version.full, version.build);

    step("checking the models");
    let models_dir = models::repository_dir();
    let bundled = check_models(&models_dir)?;

    step("getting ONNX Runtime and the AppImage runtime");
    let onnx_runtime = unpack_onnx_runtime(&fetch(&ONNX_RUNTIME, &downloads)?, &downloads)?;
    let runtime = fetch(&RUNTIME, &downloads)?;

    step("building the app");
    let exe = build(&root)?;
    if let Some(glibc) = glibc_needed(&exe) {
        println!("  it needs glibc {glibc} or later");
    }

    let app_dir = out.join(format!("{NAME}.AppDir"));
    step(&format!("assembling {}", app_dir.display()));
    if app_dir.exists() {
        fs::remove_dir_all(&app_dir).map_err(|e| format!("{}: {e}", app_dir.display()))?;
    }
    let usr = app_dir.join("usr");
    let share = usr.join("share");
    create_dir(&usr.join("bin"))?;
    create_dir(&usr.join("lib"))?;
    copy(&exe, &usr.join("bin/gumka"))?;
    // Under ONNX Runtime's own name, which is where the app looks for it.
    copy(
        &onnx_runtime.join(format!("lib/libonnxruntime.so.{ONNX_RUNTIME_VERSION}")),
        &usr.join("lib/libonnxruntime.so.1"),
    )?;
    let desktop = share.join(format!("applications/{IDENTIFIER}.desktop"));
    create_dir(desktop.parent().unwrap_or(&share))?;
    write(&desktop, &desktop_entry(&version.full))?;
    run_command(Command::new("desktop-file-validate").arg(&desktop))?;
    make_icons(&root, &share.join("icons/hicolor"))?;
    copy_models(&models_dir, &bundled, &share.join("gumka/models"))?;
    let licenses = share.join("gumka/licenses");
    let more = format!(
        "- onnxruntime/: ONNX Runtime {ONNX_RUNTIME_VERSION}, under the MIT license, with its \
         third-party notices\n\
         - appimage-runtime/: the AppImage runtime and the libraries built into it, with their \
         licenses\n"
    );
    write_licenses(&root, &version.full, &bundled, &[TARGET], &licenses, &more)?;
    create_dir(&licenses.join("onnxruntime"))?;
    for file in ["LICENSE", "ThirdPartyNotices.txt"] {
        copy(
            &onnx_runtime.join(file),
            &licenses.join("onnxruntime").join(file),
        )?;
    }
    copy_folder(
        &root.join("assets/licenses/appimage-runtime"),
        &licenses.join("appimage-runtime"),
    )?;
    // What the AppImage spec looks for at the top: the entry point, the desktop file and icon.
    let app_run = app_dir.join("AppRun");
    write(&app_run, APP_RUN)?;
    fs::set_permissions(&app_run, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", app_run.display()))?;
    link(
        &format!("usr/share/applications/{IDENTIFIER}.desktop"),
        &app_dir.join(format!("{IDENTIFIER}.desktop")),
    )?;
    link(
        &format!("usr/share/icons/hicolor/256x256/apps/{IDENTIFIER}.png"),
        &app_dir.join(format!("{IDENTIFIER}.png")),
    )?;
    link(&format!("{IDENTIFIER}.png"), &app_dir.join(".DirIcon"))?;
    println!("  {} ({})", app_dir.display(), size_of(&app_dir));

    if appimage {
        let image = out.join(image_name(if nightly {
            NIGHTLY
        } else {
            version.full.as_str()
        }));
        step(&format!("making {}", image.display()));
        let update_info = update_info(nightly)?;
        make_appimage(&app_dir, &runtime, &update_info, &image)?;
        println!("  {} ({})", image.display(), size_of(&image));
        println!("  updates: {update_info}");
        let zsync = make_zsync(&image)?;
        println!("  {} ({})", zsync.display(), size_of(&zsync));
    }
    println!("\ndone in {:.0} s.", started.elapsed().as_secs_f64());
    Ok(())
}

/// `file` in `dir`, downloaded unless it is there and checks out.
fn fetch(file: &Download, dir: &Path) -> Result<PathBuf, String> {
    let name = file.url.rsplit('/').next().unwrap_or(file.url);
    let path = dir.join(name);
    let listed = ModelFile {
        path: name.to_owned(),
        size: file.size,
        sha256: file.sha256.to_owned(),
    };
    if crate::verify(&path, &listed).is_err() {
        println!("  download  {name} ({})", crate::size(file.size));
        crate::download(file.url, &path, &listed)?;
    }
    println!("  ok        {name}");
    Ok(path)
}

/// Unpacks ONNX Runtime's archive afresh; returns its folder.
fn unpack_onnx_runtime(archive: &Path, dir: &Path) -> Result<PathBuf, String> {
    let unpacked = dir.join(format!("onnxruntime-linux-x64-{ONNX_RUNTIME_VERSION}"));
    if unpacked.exists() {
        fs::remove_dir_all(&unpacked).map_err(|e| format!("{}: {e}", unpacked.display()))?;
    }
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dir),
    )?;
    Ok(unpacked)
}

fn build(root: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    run_command(
        Command::new(cargo)
            .current_dir(root)
            .args(["build", "--release", "-p", "gumka-app"])
            // The app reads its version from git, as xtask did, so the two agree.
            .env_remove("GUMKA_VERSION"),
    )?;
    Ok(target_dir(root).join("release/gumka"))
}

/// The newest glibc `exe` asks for, as `objdump` lists its symbols (if it is installed).
fn glibc_needed(exe: &Path) -> Option<String> {
    let output = Command::new("objdump").arg("-T").arg(exe).output().ok()?;
    let (major, minor) = newest_glibc(&String::from_utf8_lossy(&output.stdout))?;
    Some(format!("{major}.{minor}"))
}

/// The newest `GLIBC_<major>.<minor>` version in `objdump -T`'s output.
fn newest_glibc(symbols: &str) -> Option<(u32, u32)> {
    symbols
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|word| word.strip_prefix("GLIBC_"))
        .filter_map(|version| {
            let mut parts = version.split('.').map(str::parse::<u32>);
            Some((parts.next()?.ok()?, parts.next()?.ok()?))
        })
        .max()
}

/// The desktop entry: Gumka in the applications menu, and offered under Open With for the
/// files it opens, but never their default app (that is the desktop's choice).
fn desktop_entry(version: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={NAME}\n\
         GenericName=Photo retouching\n\
         Comment=AI photo retouching on your computer\n\
         Exec=gumka %F\n\
         Icon={IDENTIFIER}\n\
         Terminal=false\n\
         Categories=Graphics;2DGraphics;RasterGraphics;Photography;\n\
         MimeType=image/tiff;image/png;image/jpeg;\n\
         StartupWMClass={IDENTIFIER}\n\
         X-AppImage-Version={version}\n"
    )
}

/// The logo at every size of `ICON_SIZES`, in the icon theme's layout under `hicolor`.
fn make_icons(root: &Path, hicolor: &Path) -> Result<(), String> {
    let logo = root.join("assets/logo");
    let large = read(&logo.join("gumka-icon.svg"))?;
    let small = read(&logo.join("gumka-icon-small.svg"))?;
    for pixels in ICON_SIZES {
        let dir = hicolor.join(format!("{pixels}x{pixels}/apps"));
        create_dir(&dir)?;
        let svg = if pixels <= 32 { &small } else { &large };
        render_png(svg, pixels, &dir.join(format!("{IDENTIFIER}.png")))?;
    }
    Ok(())
}

/// Every file of the bundled models into `dir`, laid out as in a models folder. Hard links where
/// they can be: the files are only read until they are packed.
fn copy_models(from: &Path, bundled: &[&models::ModelInfo], dir: &Path) -> Result<(), String> {
    for model in bundled {
        for file in &model.files {
            let (source, dest) = (
                model.root_in(from).join(&file.path),
                model.root_in(dir).join(&file.path),
            );
            create_dir(dest.parent().unwrap_or(dir))?;
            if fs::hard_link(&source, &dest).is_err() {
                copy(&source, &dest)?;
            }
        }
        println!("  {} ({})", model.name, crate::size(model.size()));
    }
    Ok(())
}

/// The files of `from` (not below it) into `to`.
fn copy_folder(from: &Path, to: &Path) -> Result<(), String> {
    create_dir(to)?;
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries.flatten() {
        if entry.path().is_file() {
            copy(&entry.path(), &to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn link(target: &str, path: &Path) -> Result<(), String> {
    symlink(target, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The AppImage's file name: with the version, or [`NIGHTLY`] for the rolling nightly build.
fn image_name(label: &str) -> String {
    format!("{NAME}-{label}-x86_64.AppImage")
}

/// The AppImage's update information: the GitHub release an updater looks in, and the `.zsync`
/// file there. A release follows the latest release. A nightly follows the newest pre-release:
/// some updaters (Gear Lever) take only `latest`, `latest-pre` or `latest-all`, not a tag, and
/// the nightly's file name matches no other release's.
fn update_info(nightly: bool) -> Result<String, String> {
    let (owner, repo) = REPOSITORY
        .strip_prefix("https://github.com/")
        .and_then(|path| path.split_once('/'))
        .ok_or_else(|| {
            format!("{REPOSITORY} isn't a GitHub repository, where updates are found")
        })?;
    let (tag, file) = if nightly {
        ("latest-pre", image_name(NIGHTLY))
    } else {
        ("latest", format!("{NAME}-*-x86_64.AppImage"))
    };
    Ok(format!(
        "gh-releases-zsync|{owner}|{repo}|{tag}|{file}.zsync"
    ))
}

/// Writes `info` into the runtime's `.upd_info` section, where updaters read it, NUL-terminated.
fn write_update_info(runtime: &mut [u8], info: &str) -> Result<(), String> {
    let section = elf_section(runtime, ".upd_info")
        .ok_or("the AppImage runtime has no .upd_info section for the update information")?;
    if info.len() >= section.len() {
        return Err(format!(
            "the update information is {} bytes, more than the runtime's {}",
            info.len(),
            section.len() - 1
        ));
    }
    let section = &mut runtime[section];
    section.fill(0);
    section[..info.len()].copy_from_slice(info.as_bytes());
    Ok(())
}

/// Where the section `name` is in `elf`, a 64-bit little-endian ELF file, if it has one. Every
/// offset is checked against the file's length.
fn elf_section(elf: &[u8], name: &str) -> Option<Range<usize>> {
    let bytes = |at: usize, n: usize| elf.get(at..at.checked_add(n)?);
    let u16_at = |at| Some(u16::from_le_bytes(bytes(at, 2)?.try_into().ok()?));
    let u32_at = |at| Some(u32::from_le_bytes(bytes(at, 4)?.try_into().ok()?));
    let u64_at = |at| usize::try_from(u64::from_le_bytes(bytes(at, 8)?.try_into().ok()?)).ok();
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let (table, entry) = (u64_at(0x28)?, usize::from(u16_at(0x3a)?));
    let (count, names_index) = (usize::from(u16_at(0x3c)?), usize::from(u16_at(0x3e)?));
    // A section header: where its name is among the names, and where its bytes are.
    let header = |index: usize| {
        let at = table.checked_add(index.checked_mul(entry)?)?;
        let (offset, size) = (
            u64_at(at.checked_add(0x18)?)?,
            u64_at(at.checked_add(0x20)?)?,
        );
        let range = offset..offset.checked_add(size)?;
        elf.get(range.clone())?;
        Some((usize::try_from(u32_at(at)?).ok()?, range))
    };
    let names = &elf[header(names_index)?.1];
    (0..count).find_map(|index| {
        let (at, range) = header(index)?;
        let rest = names.get(at..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        (&rest[..end] == name.as_bytes()).then_some(range)
    })
}

/// The `.zsync` file beside the AppImage: block checksums an updater compares with the copy it
/// has, to download only the blocks that changed. It names the AppImage by its file name, so
/// the two are published side by side and not renamed.
fn make_zsync(image: &Path) -> Result<PathBuf, String> {
    let name = image
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", image.display()))?;
    let zsync = image.with_extension("AppImage.zsync");
    run_command(
        Command::new("zsyncmake")
            .current_dir(image.parent().unwrap_or(Path::new(".")))
            .args(["-u", name, "-o"])
            .arg(&zsync)
            .arg(name),
    )?;
    Ok(zsync)
}

/// The runtime, with `update_info` in it, then the AppDir as a SquashFS image: an executable
/// AppImage. Made under a temporary name and moved into place, so a failed run leaves no
/// half-made AppImage.
fn make_appimage(
    app_dir: &Path,
    runtime: &Path,
    update_info: &str,
    image: &Path,
) -> Result<(), String> {
    let squashfs = image.with_extension("squashfs");
    let _ = fs::remove_file(&squashfs);
    run_command(
        Command::new("mksquashfs")
            .arg(app_dir)
            .arg(&squashfs)
            .args(["-root-owned", "-noappend", "-no-progress"])
            .args(["-comp", "zstd", "-b", "1M"]),
    )?;
    let mut bytes = read(runtime)?;
    write_update_info(&mut bytes, update_info)?;
    bytes.extend(read(&squashfs)?);
    let _ = fs::remove_file(&squashfs);
    let partial = image.with_extension("AppImage.part");
    fs::write(&partial, &bytes).map_err(|e| format!("{}: {e}", partial.display()))?;
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", partial.display()))?;
    fs::rename(&partial, image).map_err(|e| format!("{}: {e}", image.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release follows the latest release; a nightly, named the same every time, the newest
    /// pre-release with that name.
    #[test]
    fn updates_are_looked_for_in_the_github_releases() {
        assert_eq!(
            update_info(false).unwrap(),
            "gh-releases-zsync|andriiryzhkov|gumka|latest|Gumka-*-x86_64.AppImage.zsync"
        );
        assert_eq!(
            update_info(true).unwrap(),
            "gh-releases-zsync|andriiryzhkov|gumka|latest-pre|Gumka-nightly-x86_64.AppImage.zsync"
        );
        assert_eq!(image_name(NIGHTLY), "Gumka-nightly-x86_64.AppImage");
        assert_eq!(image_name("0.2.0"), "Gumka-0.2.0-x86_64.AppImage");
    }

    /// A minimal 64-bit ELF file: the null section, `.upd_info` of 16 bytes (filled with 0xee,
    /// to show what is overwritten), and the section names.
    fn elf_with_update_section() -> Vec<u8> {
        let names = b"\0.upd_info\0.shstrtab\0";
        let (data_at, names_at, table_at) = (64usize, 80usize, 80 + names.len());
        let mut elf = vec![0u8; table_at + 3 * 64];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[0x28..0x30].copy_from_slice(&(table_at as u64).to_le_bytes());
        elf[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        elf[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes());
        elf[0x3e..0x40].copy_from_slice(&2u16.to_le_bytes());
        elf[data_at..data_at + 16].fill(0xee);
        elf[names_at..names_at + names.len()].copy_from_slice(names);
        let mut section = |index: usize, name: u32, offset: usize, size: usize| {
            let at = table_at + index * 64;
            elf[at..at + 4].copy_from_slice(&name.to_le_bytes());
            elf[at + 0x18..at + 0x20].copy_from_slice(&(offset as u64).to_le_bytes());
            elf[at + 0x20..at + 0x28].copy_from_slice(&(size as u64).to_le_bytes());
        };
        section(1, 1, data_at, 16);
        section(2, 11, names_at, names.len());
        elf
    }

    #[test]
    fn the_update_information_goes_in_its_section() {
        let mut elf = elf_with_update_section();
        assert_eq!(elf_section(&elf, ".upd_info"), Some(64..80));
        assert_eq!(elf_section(&elf, ".sig_key"), None);
        write_update_info(&mut elf, "zsync|x").unwrap();
        assert_eq!(&elf[64..80], b"zsync|x\0\0\0\0\0\0\0\0\0", "NUL after it");
        let error = write_update_info(&mut elf, &"x".repeat(16)).unwrap_err();
        assert!(error.contains("more than the runtime's 15"), "{error}");
    }

    /// A file that isn't one, or whose offsets point past its end, has no sections.
    #[test]
    fn a_malformed_elf_has_no_sections() {
        let elf = elf_with_update_section();
        assert_eq!(elf_section(&elf[..100], ".upd_info"), None, "cut short");
        assert_eq!(elf_section(b"not an elf file", ".upd_info"), None);
        let mut far = elf.clone();
        far[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(elf_section(&far, ".upd_info"), None, "a table past the end");
        let mut long = elf;
        let size_at = 80 + 22 + 64 + 0x20;
        long[size_at..size_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            elf_section(&long, ".upd_info"),
            None,
            "a section past the end"
        );
    }

    #[test]
    fn the_newest_glibc_is_found_among_the_symbols() {
        let symbols = "\
            0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.2.5) free\n\
            0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.35) hypotf\n\
            0000000000000000  w   DF *UND*  0000000000000000 (GLIBC_2.9) pipe2\n\
            0000000000000000      DF *UND*  0000000000000000 (GLIBC_PRIVATE) _dl_x\n";
        assert_eq!(
            newest_glibc(symbols),
            Some((2, 35)),
            "2.35 is newer than 2.9"
        );
        assert_eq!(newest_glibc("no versions here"), None);
    }

    /// AppRun turns ONNX Runtime's telemetry off before starting the app with its arguments.
    #[test]
    fn app_run_starts_gumka_without_telemetry() {
        assert_eq!(
            APP_RUN,
            "#!/bin/sh\nexport ORT_DISABLE_TELEMETRY=1\n\
             exec \"$(dirname \"$(readlink -f \"$0\")\")/usr/bin/gumka\" \"$@\"\n"
        );
    }

    /// The desktop file is named after the app id the window gives, so the desktop shows the
    /// window with its icon, and offers Gumka for the files it opens.
    #[test]
    fn the_desktop_entry_matches_the_window() {
        let entry = desktop_entry("0.2.0+12~gabc1234");
        assert!(entry.starts_with("[Desktop Entry]\n"), "{entry}");
        for line in [
            "X-AppImage-Version=0.2.0+12~gabc1234",
            "Exec=gumka %F",
            "Icon=io.github.andriiryzhkov.gumka",
            "StartupWMClass=io.github.andriiryzhkov.gumka",
            "MimeType=image/tiff;image/png;image/jpeg;",
        ] {
            assert!(entry.lines().any(|l| l == line), "{line} in {entry}");
        }
        let main = include_str!("../../crates/gumka-app/src/main.rs");
        assert!(
            main.contains(&format!("const APP_ID: &str = \"{IDENTIFIER}\";")),
            "the windows' app id is the identifier"
        );
    }
}
