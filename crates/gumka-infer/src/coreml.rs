// SPDX-License-Identifier: GPL-3.0-or-later

//! Core ML backend (macOS).
//!
//! A small safe layer over the objc2 bindings: compile and cache a model, load it with
//! chosen compute units, describe its inputs and outputs, and run it on 8-bit pixel
//! buffers. Every `unsafe` Core ML and Core Video call in Gumka stays in this module.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_foundation::CFRetained;
use objc2_core_ml::{
    MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureDescription, MLFeatureProvider,
    MLFeatureType, MLFeatureValue, MLModel, MLModelConfiguration, MLMultiArray,
    MLMultiArrayDataType,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, CVReturn,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_OneComponent8, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<&NSError> for Error {
    fn from(error: &NSError) -> Self {
        Self(error.localizedDescription().to_string())
    }
}

impl From<Retained<NSError>> for Error {
    fn from(error: Retained<NSError>) -> Self {
        Self::from(&*error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeUnits {
    CpuOnly,
    CpuAndGpu,
    CpuAndNeuralEngine,
    All,
}

impl From<ComputeUnits> for MLComputeUnits {
    fn from(units: ComputeUnits) -> Self {
        match units {
            ComputeUnits::CpuOnly => MLComputeUnits::CPUOnly,
            ComputeUnits::CpuAndGpu => MLComputeUnits::CPUAndGPU,
            ComputeUnits::CpuAndNeuralEngine => MLComputeUnits::CPUAndNeuralEngine,
            ComputeUnits::All => MLComputeUnits::All,
        }
    }
}

/// Loads `package` (an `.mlpackage` or `.mlmodel`) with `units`, compiling it into
/// `cache_dir` the first time. An already compiled `.mlmodelc` (as the app bundle carries) is
/// copied into the cache instead, so it is loaded from the same path wherever the app is.
///
/// A cache entry is named after the package and a fingerprint of its files (paths, sizes,
/// modification times), so replacing the package triggers one recompile. An entry counts only
/// once its [`COMPLETE`] file says so: a compile is copied in under a temporary name, marked
/// complete and renamed into place in one step, so other processes see whole entries or none.
/// An entry that doesn't load is compiled again, once.
///
/// Core ML keeps a compiled plan of its own for every path a model is loaded from (in
/// `~/Library/Caches/<process name>`, never removed), so models are loaded from paths that stay
/// the same from one launch to the next: when `cache_dir` can't take the entry, it goes into a
/// folder of the same kind in the temporary folder, if that folder is the user's alone. Only
/// when neither can is the model loaded from where Core ML compiled it, which is removed with
/// the model. Entries unused for 30 days are removed.
pub fn load_cached(package: &Path, cache_dir: &Path, units: ComputeUnits) -> Result<Model> {
    let stem = package
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::new(format!("bad model path: {}", package.display())))?;
    let fingerprint = format!("{:016x}", fingerprint(package)?);
    let fallback = std::env::temp_dir().join("gumka-models");
    let private = is_private_folder(&fallback);
    let dirs = [cache_dir, &fallback];
    let caches: Vec<Cache> = dirs
        .iter()
        .enumerate()
        .filter(|&(i, dir)| (*dir != fallback || private) && !dirs[..i].contains(dir))
        .map(|(_, dir)| Cache {
            dir,
            stem,
            fingerprint: fingerprint.clone(),
        })
        .collect();
    let compiled = package.extension().is_some_and(|e| e == "mlmodelc");
    let (mut model, temporary) = load_from(
        &caches,
        || {
            if compiled {
                copy_to_temporary(package)
            } else {
                compile(package)
            }
        },
        |compiled| Model::load(compiled, units),
    )?;
    model._temporary = temporary.map(Temporary);
    for cache in &caches {
        cache.clean();
    }
    Ok(model)
}

/// The file in a cache entry that marks it complete. It holds the package's fingerprint, and
/// its modification time is when the entry was last used.
const COMPLETE: &str = ".gumka-complete";

/// Entries are removed once unused this long: another models folder (the repository's for the
/// CLI, the app's own) may still use its version of a package.
const UNUSED: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Temporary and discarded copies are removed once this old: a compile may still be writing.
const LEFTOVER: Duration = Duration::from_secs(60 * 60);

/// [`load_cached`] apart from Core ML: `compile` makes a compiled model (in Core ML's temporary
/// folder) and `load` loads one. The entry is looked for in each of `caches` in turn, and a
/// compile goes into the first that takes it. Returns the model, and the folder it was loaded
/// from if that must be removed with it.
fn load_from<M>(
    caches: &[Cache],
    compile: impl FnOnce() -> Result<PathBuf>,
    load: impl Fn(&Path) -> Result<M>,
) -> Result<(M, Option<PathBuf>)> {
    // Per cache, a broken entry that couldn't be moved aside (by its inode).
    let mut stuck = vec![None; caches.len()];
    for (cache, stuck) in caches.iter().zip(&mut stuck) {
        let target = cache.target();
        if !cache.is_complete(&target) {
            continue;
        }
        touch(&target.join(COMPLETE));
        let error = match load(&target) {
            Ok(model) => return Ok((model, None)),
            Err(error) => error,
        };
        eprintln!(
            "the compiled model {} doesn't load ({error})",
            target.display()
        );
        let broken = inode(&target);
        match cache.discard(&target, broken) {
            Discarded::Gone => {}
            // Another process put a fresh compile there meanwhile.
            Discarded::Restored => return Ok((load(&target)?, None)),
            Discarded::Kept => *stuck = broken,
        }
    }
    let mut compiled = compile()?;
    for (cache, stuck) in caches.iter().zip(&stuck) {
        match cache.publish(&mut compiled, *stuck) {
            Ok(entry) => return Ok((load(&entry)?, None)),
            Err(error) => eprintln!(
                "can't keep the compiled model in {} ({error})",
                cache.dir.display()
            ),
        }
    }
    eprintln!(
        "loading the compiled model from {}, which goes with it",
        compiled.display()
    );
    match load(&compiled) {
        Ok(model) => Ok((model, Some(compiled))),
        Err(error) => {
            remove_quietly(&compiled);
            Err(error)
        }
    }
}

/// The cache entries of one package in one cache folder.
struct Cache<'a> {
    dir: &'a Path,
    stem: &'a str,
    fingerprint: String,
}

/// What [`Cache::discard`] did.
#[derive(Debug, PartialEq, Eq)]
enum Discarded {
    /// Removed, or already gone.
    Gone,
    /// It couldn't be moved aside.
    Kept,
    /// What was there was another process's fresh entry, and stays.
    Restored,
}

impl Cache<'_> {
    fn target(&self) -> PathBuf {
        self.dir
            .join(format!("{}-{}.mlmodelc", self.stem, self.fingerprint))
    }

    fn is_complete(&self, entry: &Path) -> bool {
        fs::read_to_string(entry.join(COMPLETE)).is_ok_and(|f| f == self.fingerprint)
    }

    /// A name no other copy has: `kind` is "tmp" (being made) or "trash" (being removed).
    fn unique(&self, kind: &str) -> PathBuf {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!(
            ".{}-{}.{}-{nanos}-{n}.{kind}",
            self.stem,
            self.fingerprint,
            std::process::id()
        ))
    }

    /// Moves the compiled model at `source` into this cache, complete, and returns the entry.
    /// When it can't, `source` says where the compiled model is now (perhaps in a copy here),
    /// for another cache to take. `stuck` is a broken entry in the way that couldn't be moved
    /// aside: it is never taken for this one.
    fn publish(&self, source: &mut PathBuf, stuck: Option<u64>) -> Result<PathBuf> {
        fs::create_dir_all(self.dir)?;
        let copy = self.unique("tmp");
        if fs::rename(&*source, &copy).is_err() {
            // Another volume: copy, then remove the original.
            if let Err(error) = copy_dir(source, &copy) {
                remove_quietly(&copy);
                return Err(error);
            }
            remove_quietly(source);
        }
        *source = copy.clone();
        fs::write(copy.join(COMPLETE), &self.fingerprint)?;
        let target = self.target();
        for first in [true, false] {
            // A folder only replaces an empty one, so a rename never takes the place of a
            // whole entry; whatever is in the way is looked at again.
            let Err(error) = fs::rename(&copy, &target) else {
                return Ok(target);
            };
            let theirs = self.is_complete(&target) && inode(&target) != stuck;
            if theirs || (first && self.discard(&target, stuck) == Discarded::Restored) {
                // Another process published it meanwhile.
                remove_quietly(&copy);
                return Ok(target);
            }
            if !first {
                return Err(Error::new(format!(
                    "can't replace {}: {error}",
                    target.display()
                )));
            }
        }
        unreachable!("the loop returns")
    }

    /// Takes `entry` out of the cache. It is renamed aside before it is removed, so that this
    /// never removes an entry another process just put under its name: what was moved aside
    /// goes back if it is complete and isn't `broken` (the inode of the one that didn't load).
    fn discard(&self, entry: &Path, broken: Option<u64>) -> Discarded {
        let trash = self.unique("trash");
        match fs::rename(entry, &trash) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Discarded::Gone,
            Err(error) => {
                eprintln!("can't remove {}: {error}", entry.display());
                return Discarded::Kept;
            }
        }
        if self.is_complete(&trash) && inode(&trash) != broken && fs::rename(&trash, entry).is_ok()
        {
            return Discarded::Restored;
        }
        remove_quietly(&trash);
        Discarded::Gone
    }

    /// Removes, best effort, this package's entries unused for 30 days (the one just used was
    /// marked used) and the temporary and discarded copies left over an hour ago.
    fn clean(&self) {
        let Ok(entries) = fs::read_dir(self.dir) else {
            return;
        };
        let older = |path: &Path, age: Duration| {
            fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > age))
        };
        let hex = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit());
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix(self.stem)
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.strip_suffix(".mlmodelc"))
                .is_some_and(hex);
            let leftover = name
                .strip_prefix('.')
                .and_then(|s| s.strip_prefix(self.stem))
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.split_once('.'))
                .is_some_and(|(fingerprint, rest)| {
                    hex(fingerprint) && (rest.ends_with(".tmp") || rest.ends_with(".trash"))
                });
            if version {
                let marker = path.join(COMPLETE);
                let used = if marker.exists() {
                    marker
                } else {
                    path.clone()
                };
                if older(&used, UNUSED) {
                    self.discard(&path, inode(&path));
                }
            } else if leftover && older(&path, LEFTOVER) {
                remove_quietly(&path);
            }
        }
    }
}

/// Whether `dir` (made if missing) is a folder of this user's that no one else can write in.
/// Without `$TMPDIR`, the temporary folder is the shared `/tmp`, where another user could have
/// made it, or put a link there.
fn is_private_folder(dir: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let _ = fs::DirBuilder::new().mode(0o700).create(dir);
    // SAFETY: getuid has no preconditions and can't fail.
    let user = unsafe { libc::getuid() };
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && m.uid() == user && m.mode() & 0o022 == 0)
}

/// A folder's inode, which stays the same when it is renamed.
fn inode(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|m| m.ino())
}

/// Marks a cache entry used now.
fn touch(marker: &Path) {
    let _ = fs::File::options()
        .write(true)
        .open(marker)
        .and_then(|f| f.set_modified(SystemTime::now()));
}

fn remove_quietly(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("can't remove {}: {error}", path.display());
    }
}

/// A compiled model outside the cache, removed when the model that was loaded from it is.
struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        remove_quietly(&self.0);
    }
}

/// Compiles a model with Core ML and returns the temporary `.mlmodelc` it produced.
fn compile(package: &Path) -> Result<PathBuf> {
    let url = file_url(package)?;
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |compiled: *mut NSURL, error: *mut NSError| {
        // SAFETY: Core ML passes either a valid compiled-model URL or a valid error, and
        // both stay alive for the duration of the call.
        let (compiled, error) = unsafe { (compiled.as_ref(), error.as_ref()) };
        let result = match (compiled, error) {
            (Some(url), _) => url
                .to_file_path()
                .ok_or_else(|| Error::new("Core ML returned a compiled model that is not a file")),
            (None, Some(error)) => Err(Error::from(error)),
            (None, None) => Err(Error::new("Core ML returned neither a model nor an error")),
        };
        let _ = tx.send(result);
    });
    // SAFETY: `url` is a valid file URL and the handler matches the expected signature.
    unsafe { MLModel::compileModelAtURL_completionHandler(&url, &handler) };
    rx.recv()
        .map_err(|_| Error::new("Core ML dropped the compile completion handler"))?
}

/// Compiles `package` into `dest`, a `.mlmodelc` folder that is replaced if it exists. For
/// packaging: the model isn't loaded, so Core ML makes no plan for it here.
pub fn compile_into(package: &Path, dest: &Path) -> Result<()> {
    let compiled = compile(package)?;
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(&compiled, dest).is_err() {
        // Another volume.
        let copied = copy_dir(&compiled, dest);
        remove_quietly(&compiled);
        copied?;
    }
    Ok(())
}

/// A copy of the compiled model at `compiled` in the temporary folder, for the cache to take
/// in place of a compile (the original stays where it is, in the app bundle).
fn copy_to_temporary(compiled: &Path) -> Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let name = compiled.file_name().unwrap_or_default().to_string_lossy();
    let copy = std::env::temp_dir().join(format!(
        "gumka-{}-{}-{name}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    // On APFS, fs::copy clones: no data is copied.
    if let Err(error) = copy_dir(compiled, &copy) {
        remove_quietly(&copy);
        return Err(error);
    }
    Ok(copy)
}

/// FNV-1a over each file's relative path, size and modification time. Stable across Rust
/// versions, unlike `DefaultHasher`.
fn fingerprint(package: &Path) -> Result<u64> {
    let mut files = Vec::new();
    collect_files(package, package, &mut files)?;
    files.sort();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for (path, len, mtime) in files {
        let bytes = path
            .bytes()
            .chain(len.to_le_bytes())
            .chain(mtime.to_le_bytes());
        for byte in bytes {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(hash)
}

fn collect_files(root: &Path, path: &Path, out: &mut Vec<(String, u64, u128)>) -> Result<()> {
    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_files(root, &entry?.path(), out)?;
        }
    } else {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let mtime = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        out.push((relative, meta.len(), mtime));
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn file_url(path: &Path) -> Result<Retained<NSURL>> {
    let path = path.canonicalize()?;
    let url = if path.is_dir() {
        NSURL::from_directory_path(&path)
    } else {
        NSURL::from_file_path(&path)
    };
    url.ok_or_else(|| Error::new(format!("cannot make a file URL for {}", path.display())))
}

/// A model input or output, as the model describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    pub name: String,
    pub kind: FeatureKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureKind {
    /// A pixel buffer. `pixel_format` is a Core Video four-character code.
    Image {
        width: usize,
        height: usize,
        pixel_format: u32,
    },
    MultiArray {
        shape: Vec<usize>,
        data_type: &'static str,
    },
    /// Any other Core ML feature type (int, double, string, dictionary, ...).
    Other(isize),
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FeatureKind::Image {
                width,
                height,
                pixel_format,
            } => write!(
                f,
                "{}: image {width}x{height} {}",
                self.name,
                fourcc(*pixel_format)
            ),
            FeatureKind::MultiArray { shape, data_type } => {
                write!(f, "{}: multi-array {shape:?} {data_type}", self.name)
            }
            FeatureKind::Other(kind) => write!(f, "{}: feature type {kind}", self.name),
        }
    }
}

fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{code:08x}")
    }
}

impl Feature {
    fn from_description(description: &MLFeatureDescription) -> Self {
        // SAFETY: plain property reads on a valid description.
        let (name, kind) = unsafe { (description.name().to_string(), description.r#type()) };
        let kind = match kind {
            MLFeatureType::Image => {
                // SAFETY: image features carry an image constraint.
                let constraint = unsafe { description.imageConstraint() };
                match constraint {
                    // SAFETY: plain property reads.
                    Some(c) => unsafe {
                        FeatureKind::Image {
                            width: c.pixelsWide() as usize,
                            height: c.pixelsHigh() as usize,
                            pixel_format: c.pixelFormatType(),
                        }
                    },
                    None => FeatureKind::Other(kind.0),
                }
            }
            MLFeatureType::MultiArray => {
                // SAFETY: multi-array features carry a multi-array constraint.
                let constraint = unsafe { description.multiArrayConstraint() };
                match constraint {
                    // SAFETY: plain property reads.
                    Some(c) => unsafe {
                        FeatureKind::MultiArray {
                            shape: c
                                .shape()
                                .iter()
                                .map(|n| n.integerValue() as usize)
                                .collect(),
                            data_type: data_type_name(c.dataType()),
                        }
                    },
                    None => FeatureKind::Other(kind.0),
                }
            }
            other => FeatureKind::Other(other.0),
        };
        Self { name, kind }
    }
}

fn data_type_name(data_type: MLMultiArrayDataType) -> &'static str {
    match data_type {
        MLMultiArrayDataType::Float16 => "float16",
        MLMultiArrayDataType::Float32 => "float32",
        MLMultiArrayDataType::Float64 => "float64",
        MLMultiArrayDataType::Int32 => "int32",
        MLMultiArrayDataType::Int8 => "int8",
        _ => "unknown",
    }
}

fn features(by_name: &NSDictionary<NSString, MLFeatureDescription>) -> Vec<Feature> {
    let (_, descriptions) = by_name.to_vecs();
    let mut features: Vec<Feature> = descriptions
        .iter()
        .map(|d| Feature::from_description(d))
        .collect();
    features.sort_by(|a, b| a.name.cmp(&b.name));
    features
}

/// A loaded Core ML model.
pub struct Model {
    inner: Retained<MLModel>,
    /// Where it was loaded from, if that is to go with it; dropped after `inner`.
    _temporary: Option<Temporary>,
}

impl Model {
    /// Loads a compiled `.mlmodelc` (see [`load_cached`], which compiles and caches one).
    pub fn load(compiled: &Path, units: ComputeUnits) -> Result<Self> {
        let url = file_url(compiled)?;
        // SAFETY: a fresh configuration; `setComputeUnits` takes a valid enum value.
        let config = unsafe { MLModelConfiguration::new() };
        // SAFETY: as above.
        unsafe { config.setComputeUnits(units.into()) };
        // SAFETY: `url` and `config` are valid; errors come back as `Err`.
        let inner = unsafe { MLModel::modelWithContentsOfURL_configuration_error(&url, &config) }?;
        Ok(Self {
            inner,
            _temporary: None,
        })
    }

    pub fn inputs(&self) -> Vec<Feature> {
        // SAFETY: plain property reads on a loaded model.
        let by_name = unsafe { self.inner.modelDescription().inputDescriptionsByName() };
        features(&by_name)
    }

    pub fn outputs(&self) -> Vec<Feature> {
        // SAFETY: plain property reads on a loaded model.
        let by_name = unsafe { self.inner.modelDescription().outputDescriptionsByName() };
        features(&by_name)
    }

    /// Runs the model on named pixel-buffer inputs.
    pub fn predict(&self, inputs: &[(&str, &PixelBuffer)]) -> Result<Prediction> {
        let inputs: Vec<(&str, Input)> =
            inputs.iter().map(|(n, b)| (*n, Input::Image(b))).collect();
        self.predict_inputs(&inputs)
    }

    /// Runs the model on named inputs of any supported kind.
    pub fn predict_inputs(&self, inputs: &[(&str, Input)]) -> Result<Prediction> {
        let names: Vec<Retained<NSString>> = inputs
            .iter()
            .map(|(name, _)| NSString::from_str(name))
            .collect();
        let values: Vec<Retained<MLFeatureValue>> = inputs
            .iter()
            .map(|(_, input)| match input {
                // SAFETY: a valid, retained CVPixelBuffer.
                Input::Image(buffer) => unsafe {
                    MLFeatureValue::featureValueWithPixelBuffer(&buffer.0)
                },
                // SAFETY: a valid, retained MLMultiArray.
                Input::Array(array) => unsafe {
                    MLFeatureValue::featureValueWithMultiArray(&array.0)
                },
            })
            .collect();
        let names: Vec<&NSString> = names.iter().map(|n| &**n).collect();
        let values: Vec<&AnyObject> = values.iter().map(|v| v.as_ref()).collect();
        let dictionary = NSDictionary::from_slices(&names, &values);
        // SAFETY: the dictionary maps strings to feature values, as required.
        let provider = unsafe {
            MLDictionaryFeatureProvider::initWithDictionary_error(
                MLDictionaryFeatureProvider::alloc(),
                &dictionary,
            )
        }?;
        // SAFETY: a valid provider; Core ML validates inputs and reports errors as `Err`.
        let output = unsafe {
            self.inner
                .predictionFromFeatures_error(ProtocolObject::from_ref(&*provider))
        }?;
        Ok(Prediction(output))
    }
}

/// A model input.
pub enum Input<'a> {
    Image(&'a PixelBuffer),
    Array(&'a MultiArray),
}

/// The outputs of one [`Model::predict`] call.
pub struct Prediction(Retained<ProtocolObject<dyn MLFeatureProvider>>);

impl Prediction {
    /// Returns the multi-array output called `name`. It can go straight into another model.
    pub fn array(&self, name: &str) -> Result<MultiArray> {
        // SAFETY: a valid provider returned by Core ML.
        let value = unsafe { self.0.featureValueForName(&NSString::from_str(name)) }
            .ok_or_else(|| Error::new(format!("model has no output called {name:?}")))?;
        // SAFETY: a valid feature value; returns `None` for other kinds.
        let array = unsafe { value.multiArrayValue() }
            .ok_or_else(|| Error::new(format!("output {name:?} is not a multi-array")))?;
        Ok(MultiArray(array))
    }

    /// Returns the image output called `name`.
    pub fn image(&self, name: &str) -> Result<PixelBuffer> {
        // SAFETY: a valid provider returned by Core ML.
        let value = unsafe { self.0.featureValueForName(&NSString::from_str(name)) }
            .ok_or_else(|| Error::new(format!("model has no output called {name:?}")))?;
        // SAFETY: a valid feature value; returns `None` for non-image values.
        let buffer = unsafe { value.imageBufferValue() }
            .ok_or_else(|| Error::new(format!("output {name:?} is not an image")))?;
        // SAFETY: `buffer` is a live CVPixelBuffer; retaining it keeps it alive after
        // `buffer` is dropped.
        Ok(PixelBuffer(unsafe {
            CFRetained::retain(NonNull::from(&*buffer))
        }))
    }
}

/// An 8-bit Core Video pixel buffer: BGRA for color, one channel for masks.
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

impl PixelBuffer {
    /// Makes a BGRA buffer from tightly packed RGB pixels (alpha set to 255).
    pub fn from_rgb8(width: usize, height: usize, rgb: &[u8]) -> Result<Self> {
        check_len(rgb.len(), width, height, 3)?;
        let buffer = Self::new(width, height, kCVPixelFormatType_32BGRA)?;
        buffer.write_rows(|y, row| {
            let src = &rgb[y * width * 3..(y + 1) * width * 3];
            for (dst, px) in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<3>().0)
            {
                *dst = [px[2], px[1], px[0], 255];
            }
        })?;
        Ok(buffer)
    }

    /// Makes a one-channel buffer from tightly packed 8-bit values.
    pub fn from_gray8(width: usize, height: usize, gray: &[u8]) -> Result<Self> {
        check_len(gray.len(), width, height, 1)?;
        let buffer = Self::new(width, height, kCVPixelFormatType_OneComponent8)?;
        buffer
            .write_rows(|y, row| row[..width].copy_from_slice(&gray[y * width..(y + 1) * width]))?;
        Ok(buffer)
    }

    pub fn width(&self) -> usize {
        CVPixelBufferGetWidth(&self.0)
    }

    pub fn height(&self) -> usize {
        CVPixelBufferGetHeight(&self.0)
    }

    /// Core Video four-character pixel format code.
    pub fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.0)
    }

    /// Returns tightly packed RGB pixels from a BGRA buffer (alpha dropped).
    pub fn to_rgb8(&self) -> Result<Vec<u8>> {
        self.expect_format(kCVPixelFormatType_32BGRA)?;
        let width = self.width();
        let mut rgb = Vec::with_capacity(width * self.height() * 3);
        self.read_rows(|row| {
            for px in row[..width * 4].as_chunks::<4>().0 {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        })?;
        Ok(rgb)
    }

    /// Returns tightly packed values from a one-channel buffer.
    pub fn to_gray8(&self) -> Result<Vec<u8>> {
        self.expect_format(kCVPixelFormatType_OneComponent8)?;
        let width = self.width();
        let mut gray = Vec::with_capacity(width * self.height());
        self.read_rows(|row| gray.extend_from_slice(&row[..width]))?;
        Ok(gray)
    }

    fn new(width: usize, height: usize, format: u32) -> Result<Self> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: `out` is a valid place for the created buffer; no attributes.
        let status = unsafe {
            CVPixelBufferCreate(None, width, height, format, None, NonNull::from(&mut out))
        };
        cv_check(status, "CVPixelBufferCreate")?;
        let out =
            NonNull::new(out).ok_or_else(|| Error::new("CVPixelBufferCreate returned null"))?;
        // SAFETY: a "Create" function returns a +1 reference, which we now own.
        Ok(Self(unsafe { CFRetained::from_raw(out) }))
    }

    fn expect_format(&self, format: u32) -> Result<()> {
        if self.pixel_format() == format {
            Ok(())
        } else {
            Err(Error::new(format!(
                "pixel buffer is {}, expected {}",
                fourcc(self.pixel_format()),
                fourcc(format)
            )))
        }
    }

    /// Calls `f` with each row, including any padding at the end of the row. Only used on
    /// buffers this module just created and has not passed to Core ML yet.
    fn write_rows(&self, mut f: impl FnMut(usize, &mut [u8])) -> Result<()> {
        let height = self.height();
        self.locked(CVPixelBufferLockFlags::empty(), |base, stride| {
            for y in 0..height {
                // SAFETY: while locked, `base` points to `height` rows of `stride` bytes,
                // and nothing else accesses the new buffer.
                f(y, unsafe {
                    std::slice::from_raw_parts_mut(base.add(y * stride), stride)
                });
            }
        })
    }

    /// Calls `f` with each row, including any padding at the end of the row.
    fn read_rows(&self, mut f: impl FnMut(&[u8])) -> Result<()> {
        let height = self.height();
        self.locked(CVPixelBufferLockFlags::ReadOnly, |base, stride| {
            for y in 0..height {
                // SAFETY: while locked, `base` points to `height` rows of `stride` bytes.
                f(unsafe { std::slice::from_raw_parts(base.add(y * stride), stride) });
            }
        })
    }

    /// Locks the buffer and calls `f` with its base address and bytes per row.
    fn locked<R>(
        &self,
        flags: CVPixelBufferLockFlags,
        f: impl FnOnce(*mut u8, usize) -> R,
    ) -> Result<R> {
        // SAFETY: a valid buffer; unlocked below with the same flags.
        let status = unsafe { CVPixelBufferLockBaseAddress(&self.0, flags) };
        cv_check(status, "lock pixel buffer")?;
        let base = CVPixelBufferGetBaseAddress(&self.0).cast::<u8>();
        let result = (!base.is_null()).then(|| f(base, CVPixelBufferGetBytesPerRow(&self.0)));
        // SAFETY: matches the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(&self.0, flags) };
        result.ok_or_else(|| Error::new("pixel buffer has no base address"))
    }
}

/// A Core ML multi-array: an input built here, or an output handed to another model as is.
pub struct MultiArray(Retained<MLMultiArray>);

impl MultiArray {
    /// A float32 array of `shape`, filled from `data` in row-major order.
    pub fn from_f32(shape: &[usize], data: &[f32]) -> Result<Self> {
        let count: usize = shape.iter().product();
        if data.len() != count {
            return Err(Error::new(format!(
                "multi-array of shape {shape:?} needs {count} values, got {}",
                data.len()
            )));
        }
        let dims: Vec<Retained<NSNumber>> = shape
            .iter()
            .map(|&d| NSNumber::new_isize(d as isize))
            .collect();
        let dims = NSArray::from_retained_slice(&dims);
        // SAFETY: a valid shape and data type; errors come back as `Err`.
        let array = unsafe {
            MLMultiArray::initWithShape_dataType_error(
                MLMultiArray::alloc(),
                &dims,
                MLMultiArrayDataType::Float32,
            )
        }?;
        let array = Self(array);
        let strides = array.strides();
        let (starts, len, step) = rows(shape, &strides);
        let handler = RcBlock::new(|bytes: NonNull<std::ffi::c_void>, size: isize, _strides| {
            // SAFETY: Core ML passes the array's writable float32 bytes, `size` long, for this
            // call; nothing else touches them meanwhile.
            let floats = unsafe {
                std::slice::from_raw_parts_mut(bytes.as_ptr().cast::<f32>(), size as usize / 4)
            };
            for (row, &start) in starts.iter().enumerate() {
                let values = &data[row * len..(row + 1) * len];
                match floats.get_mut(start..start + len).filter(|_| step == 1) {
                    Some(dst) => dst.copy_from_slice(values),
                    None => {
                        for (i, &v) in values.iter().enumerate() {
                            if let Some(dst) = floats.get_mut(start + i * step) {
                                *dst = v;
                            }
                        }
                    }
                }
            }
        });
        // SAFETY: the handler is called synchronously with the array's writable bytes.
        unsafe { array.0.getMutableBytesWithHandler(&handler) };
        Ok(array)
    }

    pub fn shape(&self) -> Vec<usize> {
        // SAFETY: a plain property read.
        let shape = unsafe { self.0.shape() };
        numbers(&shape)
    }

    fn strides(&self) -> Vec<usize> {
        // SAFETY: a plain property read.
        let strides = unsafe { self.0.strides() };
        numbers(&strides)
    }

    /// The values as float32, in row-major order of the shape (strides applied).
    pub fn to_f32(&self) -> Result<Vec<f32>> {
        let (shape, strides) = (self.shape(), self.strides());
        // SAFETY: a plain property read.
        let data_type = unsafe { self.0.dataType() };
        let width = match data_type {
            MLMultiArrayDataType::Float16 => 2,
            MLMultiArrayDataType::Float32 | MLMultiArrayDataType::Int32 => 4,
            MLMultiArrayDataType::Float64 => 8,
            other => {
                return Err(Error::new(format!(
                    "unsupported multi-array type {other:?}"
                )));
            }
        };
        let decode: fn(&[u8]) -> f32 = match data_type {
            MLMultiArrayDataType::Float16 => |raw| f16_to_f32(u16::from_ne_bytes([raw[0], raw[1]])),
            MLMultiArrayDataType::Float32 => {
                |raw| f32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]])
            }
            MLMultiArrayDataType::Int32 => {
                |raw| i32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32
            }
            _ => |raw| f64::from_ne_bytes(raw.try_into().unwrap_or([0; 8])) as f32,
        };
        let (starts, len, step) = rows(&shape, &strides);
        let out = std::cell::RefCell::new(Vec::with_capacity(shape.iter().product()));
        let handler = RcBlock::new(|bytes: NonNull<std::ffi::c_void>, size: isize| {
            // SAFETY: Core ML passes the array's bytes, `size` long, for this call.
            let bytes =
                unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), size as usize) };
            let mut out = out.borrow_mut();
            for &start in &starts {
                // A row of contiguous values (the usual case) is read in one go.
                let whole = bytes.get(start * width..(start + len) * width);
                match whole.filter(|_| step == 1) {
                    Some(row) => out.extend(row.chunks_exact(width).map(decode)),
                    None => {
                        for i in 0..len {
                            let at = (start + i * step) * width;
                            out.push(bytes.get(at..at + width).map_or(f32::NAN, decode));
                        }
                    }
                }
            }
        });
        // SAFETY: the handler is called synchronously with the array's bytes.
        unsafe { self.0.getBytesWithHandler(&handler) };
        drop(handler);
        Ok(out.into_inner())
    }
}

fn numbers(array: &NSArray<NSNumber>) -> Vec<usize> {
    array
        .iter()
        .map(|n| n.integerValue().max(0) as usize)
        .collect()
}

/// Where each innermost row of `shape` starts (in elements, row-major order), the rows'
/// length, and the step between their values: rows let the arrays be copied a row at a time,
/// where Core ML pads only between rows.
fn rows(shape: &[usize], strides: &[usize]) -> (Vec<usize>, usize, usize) {
    let (Some((&len, outer)), Some((&step, outer_strides))) =
        (shape.split_last(), strides.split_last())
    else {
        return (vec![0], 1, 1);
    };
    (offsets(outer, outer_strides).collect(), len, step)
}

/// Element offsets (in elements, not bytes) of every index of `shape`, in row-major order.
fn offsets<'a>(shape: &'a [usize], strides: &'a [usize]) -> impl Iterator<Item = usize> + 'a {
    let count: usize = shape.iter().product();
    (0..count).map(move |mut flat| {
        let mut offset = 0;
        for (dim, stride) in shape.iter().zip(strides).rev() {
            offset += (flat % dim) * stride;
            flat /= dim;
        }
        offset
    })
}

/// IEEE 754 half precision to single precision.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let value = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: normalize the mantissa.
            let shift = mantissa.leading_zeros() - 21;
            let mantissa = (mantissa << shift) & 0x3ff;
            sign | ((127 - 15 + 1 - shift) << 23) | (mantissa << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

fn check_len(len: usize, width: usize, height: usize, channels: usize) -> Result<()> {
    if len == width * height * channels {
        Ok(())
    } else {
        Err(Error::new(format!(
            "expected {width}x{height}x{channels} = {} bytes, got {len}",
            width * height * channels
        )))
    }
}

fn cv_check(status: CVReturn, what: &str) -> Result<()> {
    if status == kCVReturnSuccess {
        Ok(())
    } else {
        Err(Error::new(format!("{what} failed (CVReturn {status})")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_round_trip_through_bgra() {
        let (w, h) = (5, 3); // odd width, so rows are likely padded
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 256) as u8).collect();
        let buffer = PixelBuffer::from_rgb8(w, h, &rgb).unwrap();
        assert_eq!((buffer.width(), buffer.height()), (w, h));
        assert_eq!(fourcc(buffer.pixel_format()), "BGRA");
        assert_eq!(buffer.to_rgb8().unwrap(), rgb);
    }

    #[test]
    fn gray_round_trip() {
        let (w, h) = (7, 4);
        let gray: Vec<u8> = (0..w * h).map(|i| (i * 13 % 256) as u8).collect();
        let buffer = PixelBuffer::from_gray8(w, h, &gray).unwrap();
        assert_eq!(fourcc(buffer.pixel_format()), "L008");
        assert_eq!(buffer.to_gray8().unwrap(), gray);
        assert!(buffer.to_rgb8().is_err());
    }

    #[test]
    fn half_floats_convert() {
        for (bits, value) in [
            (0x0000, 0.0),
            (0x3c00, 1.0),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),
            (0x7bff, 65504.0),
            (0x0001, 5.960_464_5e-8),
            (0x0400, 6.103_515_6e-5),
        ] {
            assert_eq!(f16_to_f32(bits), value, "{bits:#06x}");
        }
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn multi_array_round_trip() {
        let data: Vec<f32> = (0..24).map(|i| i as f32 * 0.5).collect();
        let array = MultiArray::from_f32(&[2, 3, 4], &data).unwrap();
        assert_eq!(array.shape(), vec![2, 3, 4]);
        assert_eq!(array.to_f32().unwrap(), data);
    }

    #[test]
    fn rows_cover_every_offset_in_order() {
        for (shape, strides) in [
            (vec![1, 3, 4, 5], vec![60, 20, 5, 1]),
            // Rows padded to 8 values, as Core ML lays out some outputs.
            (vec![1, 3, 4, 5], vec![96, 32, 8, 1]),
            (vec![2, 3], vec![1, 2]),
            (vec![7], vec![1]),
        ] {
            let (starts, len, step) = rows(&shape, &strides);
            let by_rows: Vec<usize> = starts
                .iter()
                .flat_map(|&s| (0..len).map(move |i| s + i * step))
                .collect();
            let all: Vec<usize> = offsets(&shape, &strides).collect();
            assert_eq!(by_rows, all, "{shape:?} {strides:?}");
        }
        assert_eq!(rows(&[], &[]), (vec![0], 1, 1));
    }

    #[test]
    fn a_tile_sized_array_round_trips() {
        let shape = [1, 3, 512, 512];
        let data: Vec<f32> = (0..3 * 512 * 512)
            .map(|i| (i % 1000) as f32 / 999.0)
            .collect();
        let array = MultiArray::from_f32(&shape, &data).unwrap();
        assert_eq!(array.to_f32().unwrap(), data);
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert!(PixelBuffer::from_rgb8(4, 4, &[0; 10]).is_err());
    }

    /// A cache folder, its fallback, and stand-ins for Core ML: a "compile" makes a folder with
    /// a model file in a scratch folder (as Core ML does in its temporary folder), and a "load"
    /// works on a folder that has the file and no "broken" one, and says which folder it was.
    struct Bench {
        root: PathBuf,
        dir: PathBuf,
        fallback: PathBuf,
        compiles: std::cell::Cell<usize>,
    }

    impl Bench {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("gumka-cache-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("coreml")).unwrap();
            Self {
                dir: root.join("cache"),
                fallback: root.join("fallback"),
                root,
                compiles: std::cell::Cell::new(0),
            }
        }

        fn caches(&self) -> [Cache<'_>; 2] {
            [&self.dir, &self.fallback].map(|dir| Cache {
                dir,
                stem: "Model",
                fingerprint: "00000000000000aa".into(),
            })
        }

        fn target(&self) -> PathBuf {
            self.dir.join(ENTRY)
        }

        fn compile(&self) -> Result<PathBuf> {
            let n = self.compiles.get() + 1;
            self.compiles.set(n);
            let out = self.root.join(format!("coreml/out-{n}.mlmodelc"));
            fs::create_dir_all(&out)?;
            fs::write(out.join("model.bin"), format!("compile {n}"))?;
            Ok(out)
        }

        fn load(&self) -> (PathBuf, Option<PathBuf>) {
            load_from(&self.caches(), || self.compile(), load_stand_in).unwrap()
        }

        /// What is left in Core ML's folder.
        fn left_by_coreml(&self) -> usize {
            fs::read_dir(self.root.join("coreml")).unwrap().count()
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.dir)
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            set_mode(&self.root, 0o755);
            set_mode(&self.dir, 0o755);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn load_stand_in(path: &Path) -> Result<PathBuf> {
        if path.join("model.bin").exists() && !path.join("broken").exists() {
            Ok(path.to_path_buf())
        } else {
            Err(Error::new(format!("{} doesn't load", path.display())))
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }

    fn age(path: &Path, by: Duration) {
        let file = fs::File::options().read(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    /// Whether the user can write in `dir` (root can anywhere).
    fn writable(dir: &Path) -> bool {
        let probe = dir.join("probe");
        let made = fs::File::create(&probe).is_ok();
        let _ = fs::remove_file(probe);
        made
    }

    /// Plays another process: publishes a complete entry of its own at `target`.
    fn publish_theirs(target: &Path) {
        fs::create_dir_all(target).unwrap();
        fs::write(target.join("model.bin"), "theirs").unwrap();
        fs::write(target.join(COMPLETE), "00000000000000aa").unwrap();
    }

    const ENTRY: &str = "Model-00000000000000aa.mlmodelc";

    #[test]
    fn a_compile_is_published_complete_and_used_from_then_on() {
        let bench = Bench::new("publish");
        let (loaded, temporary) = bench.load();
        assert_eq!((loaded, temporary), (bench.target(), None));
        assert_eq!(bench.names(), [ENTRY]);
        let marker = fs::read_to_string(bench.target().join(COMPLETE)).unwrap();
        assert_eq!(marker, "00000000000000aa");
        assert_eq!(
            bench.left_by_coreml(),
            0,
            "Core ML's output moved into the cache"
        );
        let marker = bench.target().join(COMPLETE);
        age(&marker, UNUSED * 2);
        bench.load();
        assert_eq!(bench.compiles.get(), 1, "the entry is used again");
        let used = fs::metadata(&marker).unwrap().modified().unwrap();
        assert!(used.elapsed().unwrap() < LEFTOVER, "and marked used");
    }

    /// An entry without the mark (half copied, or from an older Gumka) is compiled again.
    #[test]
    fn an_entry_without_the_mark_is_compiled_again() {
        let bench = Bench::new("unmarked");
        let target = bench.target();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("model.bin"), "half").unwrap();
        let (loaded, temporary) = bench.load();
        assert_eq!((loaded, temporary.is_none()), (target.clone(), true));
        assert_eq!(bench.compiles.get(), 1);
        assert_eq!(
            fs::read_to_string(target.join("model.bin")).unwrap(),
            "compile 1"
        );
        assert_eq!(bench.names(), [ENTRY], "the old one is gone");
    }

    /// An entry that doesn't load is compiled again, once (review-C #3); a fresh one that
    /// doesn't load is an error.
    #[test]
    fn an_entry_that_doesnt_load_is_compiled_again_once() {
        let bench = Bench::new("broken");
        bench.load();
        fs::write(bench.target().join("broken"), "").unwrap();
        let (loaded, _) = bench.load();
        assert_eq!(loaded, bench.target());
        assert_eq!(bench.compiles.get(), 2);
        assert!(!loaded.join("broken").exists());

        let compile = || {
            let out = bench.compile()?;
            fs::write(out.join("broken"), "")?;
            Ok(out)
        };
        fs::write(bench.target().join("broken"), "").unwrap();
        assert!(load_from(&bench.caches(), compile, load_stand_in).is_err());
        assert_eq!(bench.compiles.get(), 3, "no second retry");
    }

    /// When another process published the entry while this one compiled, its entry is used and
    /// this compile is dropped, also after recovering from a broken entry (round 2 #4).
    #[test]
    fn the_first_complete_entry_wins() {
        for broken_first in [false, true] {
            let bench = Bench::new(&format!("race-{broken_first}"));
            if broken_first {
                bench.load();
                fs::write(bench.target().join("broken"), "").unwrap();
            }
            let compile = || {
                let out = bench.compile()?;
                publish_theirs(&bench.target()); // meanwhile
                Ok(out)
            };
            let (loaded, temporary) = load_from(&bench.caches(), compile, load_stand_in).unwrap();
            assert_eq!((loaded.clone(), temporary), (bench.target(), None));
            let model = fs::read_to_string(loaded.join("model.bin")).unwrap();
            assert_eq!(model, "theirs", "broken first: {broken_first}");
            assert_eq!(bench.names(), [ENTRY], "no copy left behind");
            assert_eq!(bench.left_by_coreml(), 0);
        }
    }

    /// Moving an entry aside never takes away a fresh one another process put there: it goes
    /// back. The broken one (by inode) doesn't.
    #[test]
    fn a_fresh_entry_moved_aside_goes_back() {
        let bench = Bench::new("restore");
        let [cache, _] = bench.caches();
        fs::create_dir_all(&bench.dir).unwrap();
        publish_theirs(&bench.target());
        let theirs = inode(&bench.target());
        assert_eq!(cache.discard(&bench.target(), None), Discarded::Restored);
        assert_eq!(cache.discard(&bench.target(), Some(0)), Discarded::Restored);
        assert_eq!(bench.names(), [ENTRY]);
        assert_eq!(cache.discard(&bench.target(), theirs), Discarded::Gone);
        assert!(bench.names().is_empty());
        assert_eq!(cache.discard(&bench.target(), None), Discarded::Gone);
    }

    /// A cache that can't be written doesn't stop the model, nor make a new path for Core ML
    /// to keep a plan of every launch (round 2 #3): the entry goes into the fallback folder and
    /// is used from there again. Only with nowhere to keep it is it loaded from Core ML's
    /// output, which goes with it (review-C #4).
    #[test]
    fn a_cache_that_cant_be_written_still_gives_a_model() {
        let bench = Bench::new("read-only");
        fs::create_dir_all(&bench.dir).unwrap();
        set_mode(&bench.dir, 0o555);
        if writable(&bench.dir) {
            return; // root writes anywhere
        }
        let stable = bench.fallback.join(ENTRY);
        assert_eq!(bench.load(), (stable.clone(), None));
        assert_eq!(bench.load(), (stable.clone(), None), "the same path again");
        assert_eq!(bench.compiles.get(), 1);
        assert_eq!(bench.left_by_coreml(), 0);

        // Nowhere to keep it: from Core ML's output.
        fs::remove_dir_all(&bench.fallback).unwrap();
        set_mode(&bench.root, 0o555);
        let (loaded, temporary) = bench.load();
        set_mode(&bench.root, 0o755);
        assert_eq!(loaded, bench.root.join("coreml/out-2.mlmodelc"));
        assert_eq!(
            temporary,
            Some(loaded.clone()),
            "to be removed with the model"
        );
        drop(Temporary(loaded.clone()));
        assert!(!loaded.exists());
    }

    /// A broken entry that can't be moved away (here: locked) is never taken for the fresh
    /// compile (review-C #3), which goes into the fallback folder and is used from there.
    #[test]
    fn a_broken_entry_that_stays_is_never_loaded_again() {
        let bench = Bench::new("locked");
        bench.load();
        let target = bench.target();
        fs::write(target.join("broken"), "").unwrap();
        let lock = |flag: &str| {
            std::process::Command::new("chflags")
                .args([flag, &target.to_string_lossy()])
                .status()
                .is_ok_and(|s| s.success())
        };
        if !lock("uchg") {
            return;
        }
        let first = load_from(&bench.caches(), || bench.compile(), load_stand_in);
        let again = load_from(&bench.caches(), || bench.compile(), load_stand_in);
        lock("nouchg");
        let stable = bench.fallback.join(ENTRY);
        assert_eq!(first.unwrap(), (stable.clone(), None));
        assert_eq!(again.unwrap(), (stable, None));
        assert_eq!(
            bench.compiles.get(),
            2,
            "compiled once more, not on every launch"
        );
        assert_eq!(bench.names(), [ENTRY], "no copies left in the cache");
    }

    /// The fallback folder is used only if it is the user's alone (round 3 #1).
    #[test]
    fn the_fallback_folder_must_be_the_users_alone() {
        let bench = Bench::new("private");
        let fallback = bench.root.join("fallback");
        assert!(is_private_folder(&fallback), "made, for this user only");
        let mode = fs::metadata(&fallback).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o700
        );
        set_mode(&fallback, 0o777);
        assert!(!is_private_folder(&fallback), "anyone could write in it");
        set_mode(&fallback, 0o775);
        assert!(!is_private_folder(&fallback), "the group could");
        set_mode(&fallback, 0o755);
        assert!(is_private_folder(&fallback));
        let link = bench.root.join("link");
        std::os::unix::fs::symlink(&fallback, &link).unwrap();
        assert!(!is_private_folder(&link), "a link, perhaps someone else's");
        // Another user's folder can't be made without root; /tmp itself stands in.
        if fs::metadata("/tmp").is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0) {
            assert!(!is_private_folder(Path::new("/private/tmp")));
        }
    }

    /// Cleaning removes entries unused for 30 days (another models folder may use a recent
    /// one: review-C #9) and leftovers over an hour old, and nothing else.
    #[test]
    fn cleaning_leaves_what_may_still_be_used() {
        let bench = Bench::new("clean");
        bench.load();
        let dir = &bench.dir;
        let entry = |name: &str, marker: bool| {
            let path = dir.join(name);
            fs::create_dir_all(&path).unwrap();
            if marker {
                fs::write(path.join(COMPLETE), "x").unwrap();
            }
            path
        };
        let old_version = entry("Model-00000000000000bb.mlmodelc", true);
        age(
            &old_version.join(COMPLETE),
            UNUSED + Duration::from_secs(60),
        );
        entry("Model-00000000000000cc.mlmodelc", true);
        let old_tmp = entry(".Model-00000000000000aa.1-2-3.tmp", false);
        age(&old_tmp, LEFTOVER + Duration::from_secs(60));
        entry(".Model-00000000000000aa.4-5-6.tmp", false);
        let other = entry("Other-00000000000000bb.mlmodelc", true);
        age(&other.join(COMPLETE), UNUSED + Duration::from_secs(60));
        let [cache, fallback] = bench.caches();
        cache.clean();
        assert_eq!(
            bench.names(),
            [
                ".Model-00000000000000aa.4-5-6.tmp",
                ENTRY,
                "Model-00000000000000cc.mlmodelc",
                "Other-00000000000000bb.mlmodelc",
            ]
        );

        // An entry in the fallback folder that has gone unused, the cache being writable again.
        let unused = bench.fallback.join(ENTRY);
        publish_theirs(&unused);
        age(&unused.join(COMPLETE), UNUSED + Duration::from_secs(60));
        fallback.clean();
        assert!(!unused.exists());
    }

    /// A compiled model from the app bundle goes into the cache as a compile would, and the
    /// bundle keeps its own.
    #[test]
    fn a_bundled_model_is_copied_into_the_cache() {
        let bench = Bench::new("bundled");
        let bundled = bench
            .root
            .join("Gumka.app/Contents/Resources/models/Model.mlmodelc");
        fs::create_dir_all(bundled.join("weights")).unwrap();
        fs::write(bundled.join("model.bin"), "bundled").unwrap();
        fs::write(bundled.join("weights/weight.bin"), "weights").unwrap();

        let (loaded, temporary) = load_from(
            &bench.caches(),
            || copy_to_temporary(&bundled),
            load_stand_in,
        )
        .unwrap();
        assert_eq!((loaded, temporary), (bench.target(), None));
        assert_eq!(
            fs::read_to_string(bench.target().join("weights/weight.bin")).unwrap(),
            "weights"
        );
        assert!(
            bundled.join("model.bin").exists(),
            "the bundle keeps its copy"
        );
    }
}
