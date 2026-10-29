// SPDX-License-Identifier: GPL-3.0-or-later

//! Gives the app its version, shown on its Config page (see docs/packaging.md): read from git,
//! and read again only when the commit, the checked-out branch or a tag changes, or a shallow
//! clone gets its history, unless a packager sets `GUMKA_VERSION`. A build never fails over
//! it: where git can't say (a shallow clone, no git), it warns why and the version is 0.0.0.

use std::path::PathBuf;

fn main() {
    // Also covers the variable being unset later, which sends the build back to git.
    println!("cargo:rerun-if-env-changed=GUMKA_VERSION");
    let version = match std::env::var("GUMKA_VERSION") {
        Ok(version) if !version.is_empty() => version,
        _ => from_git(),
    };
    println!("cargo:rustc-env=GUMKA_VERSION={version}");
}

fn from_git() -> String {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("../..");
    let root = root.canonicalize().unwrap_or(root);
    for path in gumka_version::watched(&root) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    gumka_version::read(&root).map_or_else(
        |e| {
            // Cargo reads its instructions line by line.
            println!(
                "cargo:warning=the version is 0.0.0: {}",
                e.replace('\n', " ")
            );
            "0.0.0".into()
        },
        |version| version.full,
    )
}
