# Packaging, distribution and licensing

## Building the macOS app

```sh
cargo xtask models               # once: the models, checked against models.toml
cargo xtask bundle               # target/bundle/Gumka.app and Gumka-<version>-arm64.dmg
cargo xtask bundle --universal   # one app for Apple Silicon and Intel: …-universal.dmg
cargo xtask bundle --no-dmg      # the app only
```

`cargo xtask bundle` (`xtask/src/bundle.rs`) runs on a Mac and takes about 20 s once the app is
built (the release build itself takes longer the first time; `--universal` builds both
architectures and joins them with `lipo`, and needs `rustup target add x86_64-apple-darwin`
once for the pinned toolchain). It:

1. checks every model file for this platform against `models.toml` (size and SHA-256);
2. builds `gumka-app` in release mode;
3. assembles `Gumka.app`:
   - `Contents/MacOS/Gumka`, the executable;
   - `Contents/Info.plist`: identifier `io.github.andriiryzhkov.gumka`, the workspace version,
     `LSMinimumSystemVersion` 13.0 (matching `MACOSX_DEPLOYMENT_TARGET`), the photography
     category, high resolution, and the document types (TIFF, PNG, JPEG as an editor, rank
     Alternate) that list Gumka under Open With and Edit With;
   - `Contents/Resources/AppIcon.icns`, rendered from `assets/logo` (the small logo at 16 and
     32 pixels) and packed with `iconutil`;
   - `Contents/Resources/models/`: every package compiled to `.mlmodelc` with Core ML (no Xcode
     needed; compiling doesn't load the models, so Core ML keeps no plan for them here);
   - `Contents/Resources/licenses/`: see [Licensing](#licensing);
4. makes a compressed, read-only disk image (UDZO, HFS+) holding the app and a link to
   `/Applications`.

The app is about 360 MB, the disk image about 310 MB, almost all of it model weights.

## Building the Linux AppImage

```sh
cargo xtask models                 # once: the models, checked against models.toml
cargo xtask bundle                 # target/bundle/Gumka.AppDir and Gumka-<version>-x86_64.AppImage
cargo xtask bundle --no-appimage   # the AppDir only
```

`cargo xtask bundle` (`xtask/src/appimage.rs`) runs on x86_64 Linux and needs `mksquashfs`
(`squashfs-tools`), `desktop-file-validate` (`desktop-file-utils`) and `tar`; it takes a few
seconds once the app is built. It:

1. checks every model file for this platform against `models.toml` (size and SHA-256);
2. downloads ONNX Runtime and the AppImage runtime into `target/bundle/downloads`, once, each
   checked against the size and SHA-256 pinned in `appimage.rs`:
   - ONNX Runtime 1.31.0, Microsoft's own CPU build for Linux (MIT; it needs glibc 2.28 and
     libstdc++ from GCC 6 or later);
   - the type 2 AppImage runtime, release `20251108` (MIT), a static program that needs no
     libfuse on the computer it runs on;
3. builds `gumka-app` in release mode and says which glibc it needs (with `objdump`, if
   installed);
4. assembles `Gumka.AppDir`:
   - `usr/bin/gumka`, the executable; it links only libc, libm and libgcc, and opens the
     display, GPU and file-dialog libraries of the computer it runs on;
   - `usr/lib/libonnxruntime.so.1`, ONNX Runtime;
   - `usr/share/gumka/models/`: the models, as downloaded;
   - `usr/share/applications/io.github.andriiryzhkov.gumka.desktop` (checked with
     `desktop-file-validate`): Gumka in the applications menu, offered for TIFF, PNG and JPEG
     under Open With, named after the app id the window gives so the desktop shows it with its
     icon;
   - `usr/share/icons/hicolor/<size>/apps/`: the icon from 16 to 512 pixels, rendered from
     `assets/logo` (the small logo at 16 and 32 pixels);
   - `usr/share/gumka/licenses/`: see [Licensing](#licensing);
   - at the top, what the AppImage format looks for: the desktop file and icon (links), and
     `AppRun`, a shell script that sets `ORT_DISABLE_TELEMETRY=1` (see below) and starts
     `usr/bin/gumka` with its arguments;
5. packs the AppDir into a SquashFS image (zstd, 1 MB blocks) behind the runtime: an
   executable file.

The AppImage is about 435 MB, almost all of it model weights (they compress little).

The app finds what the AppImage carries by where it runs from: for `<prefix>/bin/gumka`,
ONNX Runtime in `<prefix>/lib` and the models in `<prefix>/share/gumka/models`, before the
system's (`$ORT_DYLIB_PATH` and `$GUMKA_MODELS` still come first). A later deb package can use
the same layout.

### Telemetry

Microsoft's own builds of ONNX Runtime record usage data and send it to Microsoft unless told
not to. Gumka turns that off twice: the environment is created with telemetry off
(`with_telemetry(false)`, on every path), and the AppImage's `AppRun` sets
`ORT_DISABLE_TELEMETRY=1` before ONNX Runtime starts. The API alone isn't enough: measured with
1.31.0, it still creates a device id and an event store in
`~/.cache/Microsoft/DeveloperTools/.onnxruntime`; with the variable set, nothing is created and
nothing connects. The app can't set the variable itself without `unsafe`, so someone running a
Microsoft build from source should set it (`ORT_DISABLE_TELEMETRY=1`). Ubuntu's 1.23 package
writes nothing.

### glibc

An AppImage runs on the glibc it was built with or newer. Built on Ubuntu 26.04 it needs glibc
2.43 (libm's newer symbol versions); the nightly and release builds run on `ubuntu-22.04`, so
they need 2.35: Ubuntu 22.04, Debian 12, Fedora 36, RHEL 10 and newer. Building one locally
still works for trying it on the same computer.

## CI and releases

Three GitHub Actions workflows in `.github/workflows`:

| Workflow | Runs on | Does |
|---|---|---|
| `ci.yml` | every pull request (and when called by the other two) | format, clippy and tests on macOS, Windows and Linux, warnings as errors |
| `nightly.yml` | every push to `main` | CI, then `cargo xtask bundle` on macOS and on Linux (`ubuntu-22.04`) side by side; a last job replaces the rolling **nightly** pre-release (tag `nightly`, `Gumka-nightly-arm64.dmg` and `Gumka-nightly-x86_64.AppImage`) once both are built. Each is also kept as a workflow artifact for 14 days. Only the newest push is built. |
| `release.yml` | a pushed tag `release-<version>` | CI and a check of the tag's version, then `cargo xtask bundle --universal` on macOS and `cargo xtask bundle` on Linux; a last job publishes the GitHub release `Gumka <version>` with the disk image, the AppImage and generated notes. A version with a hyphen (`0.2.0-beta.1`) is a pre-release. |

The models are downloaded on the runner by `cargo xtask models`, cached by the hash of
`models.toml` (the Linux key also names the OS, so the two platforms' models don't share a
cache entry), and checked against it every time. The Linux job installs `squashfs-tools` and
`desktop-file-utils` from Ubuntu's archive.

Nothing in the workflows moves on its own:

- Runner images are pinned by label (`macos-26`, `windows-2025-vs2026`, `ubuntu-26.04`, and
  `ubuntu-22.04` for the AppImage, the oldest glibc the runners have), never `-latest`. To move
  to a newer image, change the label and let CI show what breaks; moving the AppImage's raises
  the glibc it needs.
- Actions are pinned to a release's commit, with the version in a comment
  (`actions/checkout@<sha> # v7.0.1`). To update one, take the commit of its new release tag
  (`gh api repos/actions/checkout/commits/<tag> --jq .sha`).
- The Rust version comes from `rust-toolchain.toml`; rustup, preinstalled on the runners,
  installs it. No toolchain action is used.

To release a version:

1. Set `version` under `[workspace.package]` in `Cargo.toml` (this is the app's version and
   the one in `Info.plist`), build once so `Cargo.lock` follows, and commit.
2. Tag that commit and push the tag:
   `git tag release-0.2.0 && git push origin release-0.2.0`.

The workflow refuses a tag whose version isn't the one in `Cargo.toml`. The release jobs write
with the workflow's own token (`contents: write`); nothing else is needed in the repository's
settings unless an organization policy limits what workflows may write.

## How the app finds its models

`gumka_infer::models` looks in the package's models right after `$GUMKA_MODELS` (the bundle's
`Contents/Resources/models` on macOS, the AppImage's `usr/share/gumka/models` on Linux), so a
packaged app uses its own models even on a computer with a checkout. On macOS, the
compiled models are copied into the compiled-model cache before they are loaded (a clone on
APFS, so no space is used), because Core ML keeps a ~200 MB plan of its own for every path a
model is loaded from: loading from the cache keeps that path the same wherever the app is (in
`/Applications`, on the disk image, or at the random path macOS gives an app opened from
Downloads). See [inference.md](inference.md).

## Signing

The app is not signed for now. The Apple Silicon executable keeps the ad-hoc signature the linker
gives it, which Apple Silicon needs to run any code (`codesign -dv` shows `adhoc,linker-signed`);
the Intel half of a universal build has no signature at all, which Intel Macs allow. The bundle
itself carries no signature, and nothing is notarized.

- On the Mac that built it, the app opens normally.
- On another Mac, the downloaded disk image is quarantined, so Gatekeeper refuses to open the
  app at first ("can't be opened" / "Apple could not verify"). Remove the quarantine from the disk
  image before opening it (the app copied from it then isn't quarantined either):
  `xattr -d com.apple.quarantine ~/Downloads/Gumka-*.dmg`. If Gumka is already installed:
  `xattr -dr com.apple.quarantine /Applications/Gumka.app`. Without a terminal: try to open it
  once, then choose **Open Anyway** in System Settings → Privacy & Security. The nightly and
  release notes say the same.
- Opening it from Downloads without moving it runs it from a random path (App Translocation);
  that works, but copying it to `/Applications` first is better.

To distribute it properly later: a Developer ID Application certificate (Apple Developer
Program), signing with the hardened runtime and a secure timestamp
(`codesign --options runtime --timestamp`), then `xcrun notarytool submit … --wait` and
`xcrun stapler staple` on the disk image. No entitlements are expected to be needed (no sandbox).
Notarizing needs Xcode selected (`sudo xcode-select -s /Applications/Xcode.app`).

## Other platforms (later)

A Windows installer, a deb or Flatpak for Linux, and an AppImage for ARM: the same `cargo xtask`
approach. Windows signing is optional (SmartScreen warnings otherwise). The AppImage isn't
signed and carries no update information.

## Distribution

- Personal use: local builds are fine.
- GPL apps generally can't ship on the App Store. If that may matter later, consider a CLA for
  outside contributions (a DCO alone doesn't allow relicensing).

## Licensing

- GPL-3.0-or-later; the `LICENSE` file is in the repository root.
- An SPDX header at the top of every source file:
  `// SPDX-License-Identifier: GPL-3.0-or-later`
- Dependencies must be GPL-compatible (MIT / BSD / Apache-2.0 are fine). No proprietary or
  non-commercial code. The same rule applies to models.
- Bundled fonts are IBM Plex (SIL OFL 1.1, `crates/gumka-app/assets/fonts/OFL.txt`); the logo's
  wordmark is outlined from IBM Plex Sans SemiBold.

The macOS bundle's `Contents/Resources/licenses/` and the AppImage's
`usr/share/gumka/licenses/` hold:

- `README.txt`: Gumka's license and where its source is;
- `Gumka.txt`: the GPL;
- `models/<id>.txt`: each bundled model's name, source and license, with the license text from
  `assets/licenses/<SPDX id>.txt` (the bundle fails if a model's license has no text there);
- `fonts/IBM-Plex.txt`: the OFL;
- `crates/`: every crate compiled into the app (normal dependencies from `cargo tree`, not build
  tools or procedural macros), listed in `crates/README.txt`, each with the license files it was
  published with, plus those of vendored C code (`vendor/`) and bundled fonts (`fonts/*.txt`).
  A crate published without license files (common for workspaces that keep them at the
  repository root, such as egui and objc2) gets the standard text of each license it names that
  `assets/licenses` has (Apache-2.0, MIT; the MIT text with a copyright line naming the crate's
  authors).

The AppImage's also holds:

- `onnxruntime/`: ONNX Runtime's MIT license and its `ThirdPartyNotices.txt`, from its archive;
- `appimage-runtime/`: the runtime's MIT license and those of the libraries built into it
  (libfuse under the LGPL 2.1, squashfuse, zstd, zlib, mimalloc, musl), from
  `assets/licenses/appimage-runtime`, with a README saying where each one's source is.
