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
   - `Contents/Info.plist`: identifier `io.github.andriiryzhkov.gumka`, the version (see
     [Versions and releases](#versions-and-releases)),
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
cargo xtask bundle                 # target/bundle/Gumka.AppDir, Gumka-<version>-x86_64.AppImage and its .zsync
cargo xtask bundle --nightly       # Gumka-nightly-x86_64.AppImage, updating from the nightly release
cargo xtask bundle --no-appimage   # the AppDir only
```

`cargo xtask bundle` (`xtask/src/appimage.rs`) runs on x86_64 Linux and needs `mksquashfs`
(`squashfs-tools`), `desktop-file-validate` (`desktop-file-utils`), `zsyncmake` (`zsync`) and
`tar`; it takes a few seconds once the app is built. It:

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
   executable file. The runtime's `.upd_info` section gets the update information first (the
   downloaded runtime file itself is left as checked);
6. makes `<AppImage>.zsync` beside it with `zsyncmake`.

### Updates

An AppImage updater (AppImageUpdate, or a desktop's AppImage manager) reads the update
information in the AppImage, finds the `.zsync` file it names, and downloads only the blocks
that changed: a new build differs mostly in the app, not the models, so an update is far
smaller than the 435 MB. The update information is
`gh-releases-zsync|andriiryzhkov|gumka|latest|Gumka-*-x86_64.AppImage.zsync` for a release (the
newest GitHub release, never a pre-release) and
`gh-releases-zsync|andriiryzhkov|gumka|latest-pre|Gumka-nightly-x86_64.AppImage.zsync` for a
nightly (`--nightly`), so each follows its own channel. The nightly names the newest pre-release,
not its tag: Gear Lever takes only `latest`, `latest-pre` and `latest-all` (4.6.2: any other
value means the latest stable release, which has no nightly), and looks through the releases for
the first with a matching file. AppImageUpdate takes the newest pre-release as it is, which is
always the nightly: releases are never pre-releases.
Gear Lever compares the `.zsync`'s SHA-1 with the AppImage's to see whether there is an update,
but downloads the whole new AppImage. The `.zsync` file names its AppImage by
file name, so the two are published side by side and not renamed: that is why the nightly is
built under its final name instead of being renamed afterwards. `--appimage-updateinformation`
prints what an AppImage has. Updates aren't signed (the runtime's `.sha256_sig` section stays
empty).

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
| `nightly.yml` | every push to `main` | CI, then `cargo xtask bundle` on macOS and on Linux (`ubuntu-22.04`) side by side; a last job replaces the rolling **nightly** pre-release (tag `nightly`, `Gumka-nightly-arm64.dmg`, and `Gumka-nightly-x86_64.AppImage` with its `.zsync`) once both are built. Each is also kept as a workflow artifact for 14 days. Only the newest push is built. |
| `release.yml` | a pushed tag `release-X.Y.Z` | CI and a check of the tag (see [Versions and releases](#versions-and-releases)), then `cargo xtask bundle --universal` on macOS and `cargo xtask bundle` on Linux; a last job publishes the GitHub release `Gumka X.Y.Z` with the disk image, the AppImage and its `.zsync`, and generated notes. |

The models are downloaded on the runner by `cargo xtask models`, cached by the hash of
`models.toml` (the Linux key also names the OS, so the two platforms' models don't share a
cache entry), and checked against it every time. The Linux job installs `squashfs-tools`,
`desktop-file-utils` and `zsync` from Ubuntu's archive.

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

The release jobs write with the workflow's own token (`contents: write`); nothing else is needed
in the repository's settings unless an organization policy limits what workflows may write.

## Versions and releases

Gumka's version is `X.Y.Z`, and it comes from git tags, never from a file: `version` in
`Cargo.toml` stays `0.0.0` and isn't Gumka's. `crates/gumka-version` reads it, for
`cargo xtask bundle` and for the app's build script, with
`git describe --tags --long --match 'release-[0-9]*'` (so a tag such as `release-notes` isn't
one):

| The commit built | Its version |
|---|---|
| tagged `release-0.2.0` | `0.2.0` |
| 12 commits after that tag | `0.2.0+12~gabc1234`: the commits since, and the commit |
| before the first release tag | `0.0.0+<all commits>~g<commit>` |
| a source archive, without `.git` | `0.0.0` (`cargo xtask bundle` says why) |

The version names the disk image and the AppImage (a nightly's names say `nightly` instead), and
goes into the AppImage's desktop entry (`X-AppImage-Version`) and the licenses' README.

The app shows it at the top of its Config page, whichever way it is built (`cargo run` too):
its build script (`crates/gumka-app/build.rs`) reads it from git the same way, again whenever
the commit, the checked-out branch or a tag changes, or a shallow clone gets its history (other
branches and fetches don't rebuild the app); `cargo xtask bundle` doesn't hand it its version,
as both read the same checkout. A packager can set `GUMKA_VERSION` instead (for a build from a
source archive, say), and git isn't read then; an empty value counts as unset.
`cargo xtask bundle` builds the app without it, so the app always says what the package's
names and `Info.plist` say. A build never fails over its version: where git can't give it (a
shallow clone, git missing or failing), Cargo warns why and the app shows `0.0.0` until the
next commit, checkout or tag; `cargo xtask bundle` refuses to package instead. A release tag
that `git describe` can't read is an error, never `0.0.0`. Uncommitted changes don't show in
it.

macOS's `Info.plist` takes numbers only: `CFBundleShortVersionString` is `X.Y.Z`, and
`CFBundleVersion` is the number of commits up to the one built, which only grows. With several
copies installed (a release and a nightly), macOS opens the one with the highest
`CFBundleVersion` when a photo editor asks for Gumka. A shallow clone has no tags, so
`cargo xtask bundle` refuses one: the nightly and release jobs check out the whole history
(`fetch-depth: 0`), though not its old files (`filter: blob:none`). The rolling `nightly` tag
isn't a release tag, so versions ignore it.

Releases only move forward: each one is tagged on `main` with a higher version than the last,
so it is always the newest build there is. That keeps it GitHub's latest release (where release
AppImages update from) and gives it a higher build number than any earlier build on macOS. For
now, a release line gets fixes only until the next `Y`: fixes come as the next `Z` while `main`
holds nothing but fixes since the last release, and once a new feature is in, the next release
raises `Y`. No release is made for an earlier line, and there are no betas or release
candidates.

A release is tested as nightlies, which carry their own versions. Before one, `main` takes
fixes only (a code freeze, announced with the version it leads to); when no more come in, the
commit of the last nightly is tagged:

```sh
git tag -a release-0.2.0 -m "Gumka 0.2.0"
git push origin release-0.2.0
```

The number after the last release's (the first release is `0.1.0`):

- `Z` (`0.2.1`): fixes only, nothing else since the last release.
- `Y` (`0.3.0`, `Z` back to 0): new tools or features, or changes users notice; the `0.2` line
  ends.
- `X` (`1.0.0`): the first stable release; after it, a change in what users rely on (file
  formats, what a save keeps, defaults).

Before building, the workflow refuses a tag that isn't `release-` and three numbers, one on a
commit that has another release tag, one not on `main`, and one not above every earlier
release. The build jobs fail if the version they read isn't the tag's. A pushed tag stays: if
a release turns out wrong, release the next `Z`. Only a tag that was refused, or whose release
failed before publishing, is deleted (`git push origin --delete release-0.2.0`,
`git tag -d release-0.2.0`) and made again.

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
approach. Windows signing is optional (SmartScreen warnings otherwise). The AppImage and its
updates aren't signed.

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
