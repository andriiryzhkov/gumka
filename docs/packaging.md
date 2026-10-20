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
     category, high resolution;
   - `Contents/Resources/AppIcon.icns`, rendered from `assets/logo` (the small logo at 16 and
     32 pixels) and packed with `iconutil`;
   - `Contents/Resources/models/`: every package compiled to `.mlmodelc` with Core ML (no Xcode
     needed; compiling doesn't load the models, so Core ML keeps no plan for them here);
   - `Contents/Resources/licenses/`: see [Licensing](#licensing);
4. makes a compressed, read-only disk image (UDZO, HFS+) holding the app and a link to
   `/Applications`.

The app is about 330 MB, the disk image about 280 MB, almost all of it model weights.

## CI and releases

Three GitHub Actions workflows in `.github/workflows`:

| Workflow | Runs on | Does |
|---|---|---|
| `ci.yml` | every pull request (and when called by the other two) | format, clippy and tests on macOS, Windows and Linux, warnings as errors |
| `nightly.yml` | every push to `main` | CI, then `cargo xtask bundle` on macOS; replaces the rolling **nightly** pre-release (tag `nightly`, `Gumka-nightly-arm64.dmg`) and keeps the disk image as a workflow artifact for 14 days. Only the newest push is built. |
| `release.yml` | a pushed tag `release-<version>` | CI, then `cargo xtask bundle --universal`; publishes the GitHub release `Gumka <version>` with the disk image and generated notes. A version with a hyphen (`0.2.0-beta.1`) is a pre-release. |

The models are downloaded on the runner by `cargo xtask models`, cached by the hash of
`models.toml`, and checked against it every time.

Nothing in the workflows moves on its own:

- Runner images are pinned by label (`macos-26`, `windows-2025-vs2026`, `ubuntu-26.04`), never
  `-latest`. To move to a newer image, change the label and let CI show what breaks.
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

`gumka_infer::models` looks in the bundle's `Contents/Resources/models` right after
`$GUMKA_MODELS`, so a bundled app uses its own models even on a Mac with a checkout. The
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
  app at first ("can't be opened" / "Apple could not verify"). Copy it to `/Applications`, try to
  open it once, then choose **Open Anyway** in System Settings → Privacy & Security. Or remove the
  quarantine from a terminal: `xattr -dr com.apple.quarantine /Applications/Gumka.app`.
- Opening it from Downloads without moving it runs it from a random path (App Translocation);
  that works, but copying it to `/Applications` first is better.

To distribute it properly later: a Developer ID Application certificate (Apple Developer
Program), signing with the hardened runtime and a secure timestamp
(`codesign --options runtime --timestamp`), then `xcrun notarytool submit … --wait` and
`xcrun stapler staple` on the disk image. No entitlements are expected to be needed (no sandbox).
Notarizing needs Xcode selected (`sudo xcode-select -s /Applications/Xcode.app`).

## Still missing for "Edit With"

- The open-documents handler ([opening-and-saving.md](opening-and-saving.md#how-files-arrive)):
  without it, a file sent by Finder, the Dock or a RAW editor doesn't reach the app.
- Then `CFBundleDocumentTypes` in Info.plist (TIFF, PNG, JPEG; role Editor, rank Alternate), so
  Gumka is offered under Open With and Edit With. They are left out until the handler exists, so
  that Open With never opens Gumka without the file.

## Other platforms (later)

`cargo-packager` (a Windows installer, AppImage, deb; Flatpak later for Linux), or the same
`cargo xtask` approach. Windows signing is optional (SmartScreen warnings otherwise).

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

The bundle's `Contents/Resources/licenses/` holds:

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
