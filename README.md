<img src="assets/logo/gumka-icon.svg" width="96" height="96" alt="">

# Gumka

Extensible AI photo retouching. The first tool is an eraser: paint over an object and Gumka
fills it in with [LaMa](https://github.com/advimman/lama), on your Mac, in 16 bits, keeping the
file's color profile and metadata. Pixels outside the erased area stay bit-identical in TIFF and
PNG; a JPEG is re-encoded when saved.

Early development: macOS 13 or later (Apple Silicon) only for now.

## Running from source

Needs Rust (via [rustup](https://rustup.rs)). Models are not in git: download them once (about
310 MB) with `cargo xtask models`, which reads [`models.toml`](models.toml) and checks every file.

```sh
cargo xtask models                               # once, and after models.toml changes
cargo run --release -p gumka-app                 # the app
cargo run --release -p gumka-app -- photo.tif    # open a file directly
cargo run --release -p gumka-cli -- erase in.tif mask.png out.tif
cargo xtask bundle                               # Gumka.app and a disk image in target/bundle
```

The app built by `cargo xtask bundle` carries the models and isn't signed: on another Mac, open it
once, then choose Open Anyway in System Settings → Privacy & Security.

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
