<img src="assets/logo/gumka-icon.svg" width="96" height="96" alt="">

# Gumka

Extensible AI photo retouching, on your Mac, in 16 bits, keeping the file's color profile and
metadata:

- **Erase:** paint over an object and Gumka fills it in with
  [LaMa](https://github.com/advimman/lama). Pixels outside the erased area stay bit-identical in
  TIFF and PNG; a JPEG is re-encoded when saved.
- **Upscale:** make the photo 1.25× to 4× larger with
  [RealPLKSR](https://github.com/dslisleedh/PLKSR), which sharpens what is there without
  inventing detail. Undo brings back the original size.

Early development: macOS 13 or later (Apple Silicon) only for now.

## Running from source

Needs Rust (via [rustup](https://rustup.rs)). Models are not in git: download them once (about
340 MB) with `cargo xtask models`, which reads [`models.toml`](models.toml) and checks every file.

```sh
cargo xtask models                               # once, and after models.toml changes
cargo run --release -p gumka-app                 # the app
cargo run --release -p gumka-app -- photo.tif    # open a file directly
cargo run --release -p gumka-cli -- erase in.tif mask.png out.tif
cargo run --release -p gumka-cli -- upscale in.tif out.tif --scale 2
cargo xtask bundle                               # Gumka.app and a disk image in target/bundle
```

The app built by `cargo xtask bundle` carries the models and isn't signed, so on another Mac
macOS blocks it at first. Before opening a downloaded disk image, remove the quarantine from it:

```sh
xattr -d com.apple.quarantine ~/Downloads/Gumka-*.dmg
```

If Gumka is already installed, run:

```sh
xattr -dr com.apple.quarantine /Applications/Gumka.app
```

In the app: open a TIFF, PNG or JPEG (⌘O or drag it onto the window), paint over what to
remove, press Return. Option-drag removes from the mask, Esc clears it, `[` and `]` change the
brush size, Space-drag or two-finger scroll pans, pinch or ⌘-scroll zooms. ⌘Z / ⇧⌘Z undo and
redo, ⌘S saves in place (not over a read-only file), ⇧⌘S saves under a new name and goes on
there.

## Contributing

Developer documentation is in [`docs/`](docs/README.md); [AGENTS.md](AGENTS.md) has the
commands, rules and conventions for working on the code.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
