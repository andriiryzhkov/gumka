// SPDX-License-Identifier: GPL-3.0-or-later

//! The model registry and where model files are found.
//!
//! The registry is `models.toml` at the repository root, built in here. `cargo xtask models`
//! downloads what it lists into the repository's `models/`. Packages are looked up in:
//! 1. the directory in `$GUMKA_MODELS`,
//! 2. the package's models, when running from one: the app bundle's `Contents/Resources/models`
//!    on macOS, `usr/share/gumka/models` in the AppImage on Linux,
//! 3. `models/` in the current directory,
//! 4. the repository's `models/` (for builds run from a checkout),
//! 5. the per-user data directory, e.g. `~/Library/Application Support/Gumka/models` on macOS.
//!
//! A directory holds a model only if it has every file the registry lists, each of the listed
//! size (an interrupted download is skipped, and said why). `$GUMKA_MODELS` is the override:
//! there the files must exist, but may differ in size (a model converted differently), with a
//! warning. Checking the SHA-256 sums is left to `cargo xtask models --check`. The macOS app
//! bundle holds the packages compiled (`LaMa.mlmodelc` for `LaMa.mlpackage`; see
//! [`compiled_name`]), made by `cargo xtask bundle` from checked downloads; the AppImage holds
//! them as downloaded.
//!
//! An id can have an entry per platform (Core ML on macOS, ONNX on Linux); [`model`] gives this
//! platform's. A model with a `dir` keeps its files in that folder of a models directory.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

/// The registry's text, as built in.
pub const REGISTRY: &str = include_str!("../../../models.toml");

/// Registry ids of the models the app uses.
pub const LAMA: &str = "lama";
pub const SAM: &str = "sam2.1-small";
pub const UPSCALE_2X: &str = "realplksr-2x";
pub const UPSCALE_4X: &str = "realplksr-4x";

/// The registry id of the upscaler that makes pictures `scale` times larger.
pub fn upscaler_id(scale: u32) -> Result<&'static str, String> {
    match scale {
        2 => Ok(UPSCALE_2X),
        4 => Ok(UPSCALE_4X),
        _ => Err(format!("there is no {scale}× upscaler")),
    }
}

/// The command that downloads the models, for error messages.
pub const FETCH_COMMAND: &str = "cargo xtask models";

#[derive(Debug, Deserialize)]
struct Registry {
    models: Vec<ModelInfo>,
}

/// One model: what it is, where it comes from, and its files.
#[derive(Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// The tool that uses it.
    pub tool: String,
    pub name: String,
    /// What it is good at, in a few words.
    pub summary: String,
    pub format: String,
    pub license: String,
    /// The copyright lines the license's text goes with, for licenses that name the holder
    /// (MIT, BSD).
    #[serde(default)]
    pub copyright: Vec<String>,
    pub source: String,
    pub source_url: String,
    /// The platform it runs on (`"macos"` for Core ML, `"linux"` for ONNX).
    pub platform: String,
    /// The folder of a models directory its files are in, if not the directory itself.
    #[serde(default)]
    pub dir: Option<String>,
    /// What the app opens, relative to [`Self::root_in`].
    pub packages: Vec<String>,
    /// Each file is downloaded from `base_url` + its path.
    pub base_url: String,
    pub files: Vec<ModelFile>,
}

#[derive(Debug, Deserialize)]
pub struct ModelFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl ModelInfo {
    /// Whether this model runs on the platform Gumka was built for.
    pub fn for_this_platform(&self) -> bool {
        self.platform == std::env::consts::OS
    }

    /// Where this model's files are in the models directory `dir`.
    pub fn root_in(&self, dir: &Path) -> PathBuf {
        match &self.dir {
            Some(folder) => dir.join(folder),
            None => dir.to_path_buf(),
        }
    }

    /// Total download size.
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The model's packages, found in the search directories (all in one directory), or an
    /// explanation of what is missing and how to get it.
    pub fn find_packages(&self) -> Result<Vec<PathBuf>, NotFound> {
        let overriding = std::env::var_os("GUMKA_MODELS").map(PathBuf::from);
        // Only Core ML's packages are compiled for the bundle.
        let compiled = bundle_dir().filter(|_| cfg!(target_os = "macos"));
        self.find_packages_in(&search_dirs(), overriding.as_deref(), compiled.as_deref())
    }

    /// [`Self::find_packages`] in `dirs`, of which `overriding` (if among them) only needs the
    /// files, whatever their sizes, and `bundled` holds the packages compiled.
    fn find_packages_in(
        &self,
        dirs: &[PathBuf],
        overriding: Option<&Path>,
        bundled: Option<&Path>,
    ) -> Result<Vec<PathBuf>, NotFound> {
        let mut problems = Vec::new();
        for dir in dirs {
            if Some(dir.as_path()) == bundled {
                match self.check_compiled(dir) {
                    Ok(paths) => return Ok(paths),
                    Err(incomplete) => {
                        if let Some(why) = incomplete.explain() {
                            eprintln!("{} in {} skipped: {why}", self.name, dir.display());
                            problems.push(format!("  {}: {why}", dir.display()));
                        }
                        continue;
                    }
                }
            }
            let check = self.check_files(dir);
            let found = match (&check, Some(dir.as_path()) == overriding) {
                (Ok(()), _) => true,
                (Err(Incomplete::Files { missing, sizes }), true) if missing.is_empty() => {
                    for size in sizes {
                        eprintln!(
                            "{}: {} is {}, where models.toml says {}; using it, as \
                             $GUMKA_MODELS says",
                            self.name, size.path, size.actual, size.listed
                        );
                    }
                    true
                }
                _ => false,
            };
            if found {
                let root = self.root_in(dir);
                return Ok(self.packages.iter().map(|p| root.join(p)).collect());
            }
            if let Err(incomplete) = check
                && let Some(why) = incomplete.explain()
            {
                eprintln!("{} in {} skipped: {why}", self.name, dir.display());
                problems.push(format!("  {}: {why}", dir.display()));
            }
        }
        let searched: Vec<String> = dirs.iter().map(|d| format!("  {}", d.display())).collect();
        let incomplete = !problems.is_empty();
        let mut message = format!(
            "{} is {}. Run `{FETCH_COMMAND}` in the Gumka repository.",
            self.name,
            if incomplete {
                "only partly downloaded"
            } else {
                "not downloaded"
            }
        );
        if incomplete {
            message += &format!("\nIncomplete copies:\n{}", problems.join("\n"));
        }
        message += &format!("\nLooked in:\n{}", searched.join("\n"));
        Err(NotFound {
            message,
            incomplete,
        })
    }

    /// The compiled packages in `dir`, an app bundle's models folder: each must be there, with
    /// the file every compiled Core ML model has.
    fn check_compiled(&self, dir: &Path) -> Result<Vec<PathBuf>, Incomplete<'_>> {
        let paths: Vec<PathBuf> = self
            .packages
            .iter()
            .map(|p| dir.join(compiled_name(p)))
            .collect();
        let missing: Vec<String> = paths
            .iter()
            .filter(|path| !path.join("coremldata.bin").is_file())
            .map(|path| {
                format!(
                    "{} (compiled)",
                    path.file_name().unwrap_or_default().display()
                )
            })
            .collect();
        if missing.is_empty() {
            Ok(paths)
        } else if missing.len() == paths.len() {
            Err(Incomplete::Absent)
        } else {
            Err(Incomplete::Files {
                missing,
                sizes: Vec::new(),
            })
        }
    }

    /// Whether `dir` has every file of this model, each the registry's size.
    fn check_files(&self, dir: &Path) -> Result<(), Incomplete<'_>> {
        let (mut missing, mut sizes) = (Vec::new(), Vec::new());
        let root = self.root_in(dir);
        for file in &self.files {
            match std::fs::metadata(root.join(&file.path)) {
                Ok(meta) if meta.len() == file.size => {}
                Ok(meta) => sizes.push(OtherSize {
                    path: &file.path,
                    actual: megabytes(meta.len()),
                    listed: megabytes(file.size),
                }),
                Err(_) => missing.push(file.path.clone()),
            }
        }
        if missing.is_empty() && sizes.is_empty() {
            Ok(())
        } else if missing.len() == self.files.len() {
            Err(Incomplete::Absent)
        } else {
            Err(Incomplete::Files { missing, sizes })
        }
    }
}

/// The name of a package once compiled: `LaMa.mlpackage` becomes `LaMa.mlmodelc`.
pub fn compiled_name(package: &str) -> String {
    let path = Path::new(package);
    let stem = path.file_stem().unwrap_or(path.as_os_str());
    format!("{}.mlmodelc", stem.to_string_lossy())
}

/// The models folder of the package this program runs from, if it is one.
pub fn bundle_dir() -> Option<PathBuf> {
    let dir = package_models(&std::env::current_exe().ok()?)?;
    dir.is_dir().then_some(dir)
}

/// Where a package puts its models, for the executable `exe`: an app bundle's
/// `Contents/Resources/models` next to its `Contents/MacOS/<exe>` on macOS, else
/// `<prefix>/share/gumka/models` for `<prefix>/bin/<exe>` (the AppImage's `usr/`).
fn package_models(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if cfg!(target_os = "macos") {
        let contents = Some(bin)
            .filter(|d| d.ends_with("Contents/MacOS"))?
            .parent()?;
        Some(contents.join("Resources/models"))
    } else {
        let prefix = Some(bin).filter(|d| d.ends_with("bin"))?.parent()?;
        Some(prefix.join("share/gumka/models"))
    }
}

/// A model that isn't in any models folder, complete.
#[derive(Debug)]
pub struct NotFound {
    /// What is missing where, and how to get it.
    pub message: String,
    /// Some folder holds part of it: a download was interrupted, say.
    pub incomplete: bool,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NotFound {}

impl From<NotFound> for String {
    fn from(error: NotFound) -> Self {
        error.message
    }
}

/// Why a directory doesn't hold a model.
enum Incomplete<'a> {
    /// None of its files: not worth a word.
    Absent,
    /// Some files are missing or of another size.
    Files {
        missing: Vec<String>,
        sizes: Vec<OtherSize<'a>>,
    },
}

/// A file of another size than the registry's.
struct OtherSize<'a> {
    path: &'a str,
    actual: String,
    listed: String,
}

impl Incomplete<'_> {
    fn explain(&self) -> Option<String> {
        let Incomplete::Files { missing, sizes } = self else {
            return None;
        };
        let missing = missing.iter().map(|path| format!("{path} is missing"));
        let sizes = sizes.iter().map(|s| {
            format!(
                "{} is {} of {} (an interrupted download?)",
                s.path, s.actual, s.listed
            )
        });
        Some(missing.chain(sizes).collect::<Vec<_>>().join("; "))
    }
}

fn megabytes(bytes: u64) -> String {
    match bytes {
        0..1_000_000 => format!("{bytes} bytes"),
        _ => format!("{:.0} MB", bytes as f64 / 1e6),
    }
}

/// Every model in the registry, in tool order.
pub fn registry() -> &'static [ModelInfo] {
    static PARSED: OnceLock<Vec<ModelInfo>> = OnceLock::new();
    PARSED.get_or_init(|| {
        // The registry is built in and checked by a test, so this only fails during development.
        toml::from_str::<Registry>(REGISTRY)
            .expect("models.toml is invalid")
            .models
    })
}

/// This platform's model of registry id `id`.
pub fn model(id: &str) -> Result<&'static ModelInfo, String> {
    registry()
        .iter()
        .find(|m| m.id == id && m.for_this_platform())
        .ok_or_else(|| {
            format!(
                "models.toml has no model {id:?} for {}",
                std::env::consts::OS
            )
        })
}

/// The repository's `models/` directory, where `cargo xtask models` puts files.
pub fn repository_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models")
}

/// Directories searched for models, in order.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("GUMKA_MODELS") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.extend(bundle_dir());
    dirs.push(PathBuf::from("models"));
    dirs.push(repository_dir());
    if let Some(project) = project_dirs() {
        dirs.push(project.data_dir().join("models"));
    }
    dirs
}

/// Total size of a file, or of every file in a directory (such as an .mlpackage).
pub fn disk_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| disk_size(&e.path())).sum())
        .unwrap_or(0)
}

/// Where compiled models are cached.
pub fn cache_dir() -> PathBuf {
    project_dirs()
        .map(|p| p.cache_dir().join("models"))
        .unwrap_or_else(|| std::env::temp_dir().join("gumka-models"))
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "Gumka")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_valid() {
        let models = registry();
        // Every model the app uses, on each platform with a backend.
        for platform in ["macos", "linux"] {
            for id in [LAMA, SAM, UPSCALE_2X, UPSCALE_4X] {
                assert!(
                    models.iter().any(|m| m.id == id && m.platform == platform),
                    "{id} on {platform}"
                );
            }
        }
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let lama = model(LAMA).unwrap();
            assert_eq!(lama.platform, std::env::consts::OS, "this platform's entry");
        }
        for m in models {
            if m.license == "MIT" || m.license.starts_with("BSD") {
                assert!(
                    !m.copyright.is_empty(),
                    "{}: {} needs its copyright",
                    m.id,
                    m.license
                );
            }
            assert!(
                m.base_url.starts_with("https://") && m.base_url.ends_with('/'),
                "{}",
                m.id
            );
            assert!(!m.packages.is_empty() && !m.files.is_empty(), "{}", m.id);
            for f in &m.files {
                assert_eq!(f.sha256.len(), 64, "{} {}", m.id, f.path);
                assert!(
                    f.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{} {}",
                    m.id,
                    f.path
                );
                // Inside a package, a package itself, or an ONNX package's external weights.
                assert!(
                    m.packages
                        .iter()
                        .any(|p| f.path.starts_with(&format!("{p}/"))
                            || f.path == *p
                            || f.path == format!("{p}_data")),
                    "{} lies outside the model's packages",
                    f.path
                );
            }
        }
        let mut ids: Vec<(&str, &str)> = models
            .iter()
            .map(|m| (m.id.as_str(), m.platform.as_str()))
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), models.len(), "an id twice for one platform");
        // The app names a model by its id wherever it runs.
        for m in models {
            assert!(
                models
                    .iter()
                    .all(|other| other.id != m.id || other.name == m.name),
                "{} has more than one name",
                m.id
            );
        }
    }

    /// A small model: one package of two files.
    fn tiny() -> ModelInfo {
        let file = |path: &str, size| ModelFile {
            path: path.into(),
            size,
            sha256: "0".repeat(64),
        };
        ModelInfo {
            id: "tiny".into(),
            tool: "Erase".into(),
            name: "Tiny".into(),
            summary: String::new(),
            format: String::new(),
            license: String::new(),
            copyright: Vec::new(),
            source: String::new(),
            source_url: String::new(),
            platform: std::env::consts::OS.into(),
            dir: None,
            packages: vec!["Tiny.mlpackage".into()],
            base_url: "https://example.invalid/".into(),
            files: vec![
                file("Tiny.mlpackage/Manifest.json", 4),
                file("Tiny.mlpackage/weight.bin", 2_000_000),
            ],
        }
    }

    /// Writes the model's files into `dir`, the weights `weights` bytes long, or not at all.
    fn put(dir: &Path, manifest: bool, weights: Option<u64>) {
        let package = dir.join("Tiny.mlpackage");
        std::fs::create_dir_all(&package).unwrap();
        if manifest {
            std::fs::write(package.join("Manifest.json"), b"{}\n\n").unwrap();
        }
        if let Some(len) = weights {
            let file = std::fs::File::create(package.join("weight.bin")).unwrap();
            file.set_len(len).unwrap();
        }
    }

    /// An interrupted download is passed over for a complete copy, and said why; the override
    /// may differ in size but not miss files (C2, review-C #5).
    #[test]
    fn only_complete_copies_are_used() {
        let root = std::env::temp_dir().join(format!("gumka-models-{}", std::process::id()));
        let dirs: Vec<PathBuf> = ["override", "partial", "complete", "empty"]
            .iter()
            .map(|d| root.join(d))
            .collect();
        for dir in &dirs {
            std::fs::create_dir_all(dir).unwrap();
        }
        let model = tiny();
        put(&dirs[1], true, Some(12_000));
        put(&dirs[2], true, Some(2_000_000));

        let found = model.find_packages_in(&dirs, None, None).unwrap();
        assert_eq!(found, [dirs[2].join("Tiny.mlpackage")], "the complete copy");

        // The override wins with other sizes, not with files missing.
        put(&dirs[0], true, Some(5));
        let found = model.find_packages_in(&dirs, Some(&dirs[0]), None).unwrap();
        assert_eq!(found, [dirs[0].join("Tiny.mlpackage")]);
        assert_eq!(
            model.find_packages_in(&dirs, None, None).unwrap(),
            [dirs[2].join("Tiny.mlpackage")],
            "elsewhere, sizes count"
        );
        std::fs::remove_file(dirs[0].join("Tiny.mlpackage/weight.bin")).unwrap();
        let found = model.find_packages_in(&dirs, Some(&dirs[0]), None).unwrap();
        assert_eq!(found, [dirs[2].join("Tiny.mlpackage")]);

        // Nothing complete: the error says what is wrong where.
        let error = model
            .find_packages_in(&dirs[..2], Some(&dirs[0]), None)
            .unwrap_err();
        assert!(error.incomplete);
        let error = error.message;
        assert!(
            error.contains("Tiny.mlpackage/weight.bin is missing"),
            "{error}"
        );
        assert!(
            error.contains("weight.bin is 12000 bytes of 2 MB (an interrupted download?)"),
            "{error}"
        );
        assert!(
            error.contains(FETCH_COMMAND) && error.contains("Looked in"),
            "{error}"
        );
        let error = model.find_packages_in(&dirs[3..], None, None).unwrap_err();
        assert!(
            !error.incomplete,
            "an empty folder isn't incomplete: {error}"
        );
        assert!(error.message.contains("is not downloaded"), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A model with a folder of its own is looked for in that folder of each directory.
    #[test]
    fn a_model_with_a_dir_is_found_inside_it() {
        let root = std::env::temp_dir().join(format!("gumka-dir-{}", std::process::id()));
        let (outside, inside) = (root.join("outside"), root.join("inside"));
        put(&outside, true, Some(2_000_000));
        put(&inside.join("Tiny-ONNX"), true, Some(2_000_000));
        let model = ModelInfo {
            dir: Some("Tiny-ONNX".into()),
            ..tiny()
        };
        let found = model
            .find_packages_in(&[outside.clone(), inside.clone()], None, None)
            .unwrap();
        assert_eq!(found, [inside.join("Tiny-ONNX/Tiny.mlpackage")]);
        assert!(
            model.find_packages_in(&[outside], None, None).is_err(),
            "files outside the folder don't count"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A package's models are next to its executable, as each platform's package lays them out;
    /// a build in `target/` has none.
    #[test]
    fn a_package_keeps_its_models_by_its_executable() {
        let (exe, models) = if cfg!(target_os = "macos") {
            (
                "/Applications/Gumka.app/Contents/MacOS/Gumka",
                "/Applications/Gumka.app/Contents/Resources/models",
            )
        } else {
            (
                "/tmp/.mount_Gumka/usr/bin/gumka",
                "/tmp/.mount_Gumka/usr/share/gumka/models",
            )
        };
        assert_eq!(package_models(Path::new(exe)), Some(PathBuf::from(models)));
        assert_eq!(
            package_models(Path::new("/src/gumka/target/release/gumka")),
            None
        );
        assert_eq!(package_models(Path::new("gumka")), None);
    }

    /// An app bundle holds the packages compiled; one that lacks some is passed over and said
    /// why, and the override still comes first.
    #[test]
    fn a_bundle_holds_compiled_packages() {
        assert_eq!(compiled_name("LaMa.mlpackage"), "LaMa.mlmodelc");
        let root = std::env::temp_dir().join(format!("gumka-bundle-{}", std::process::id()));
        let (bundle, complete) = (root.join("bundle"), root.join("complete"));
        put(&complete, true, Some(2_000_000));
        let compiled = bundle.join("Tiny.mlmodelc");
        std::fs::create_dir_all(&compiled).unwrap();
        let model = tiny();
        let dirs = [bundle.clone(), complete.clone()];

        // Not compiled yet: the next folder is used.
        let found = model.find_packages_in(&dirs, None, Some(&bundle)).unwrap();
        assert_eq!(found, [complete.join("Tiny.mlpackage")]);

        std::fs::write(compiled.join("coremldata.bin"), b"x").unwrap();
        let found = model.find_packages_in(&dirs, None, Some(&bundle)).unwrap();
        assert_eq!(
            found,
            std::slice::from_ref(&compiled),
            "the compiled package"
        );

        // A folder that isn't the bundle isn't read as compiled.
        let found = model.find_packages_in(&dirs, None, None).unwrap();
        assert_eq!(found, [complete.join("Tiny.mlpackage")]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
