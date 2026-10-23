// SPDX-License-Identifier: GPL-3.0-or-later

//! Opening and saving pictures.
//!
//! A picture keeps what it needs to be saved back the way it came: bit depth, gray or color,
//! alpha, and its metadata (see meta.rs), which goes into whichever format it is saved as.
//! Pixels are always 16-bit RGB in memory; 8-bit files convert losslessly both ways, so
//! untouched pixels stay bit-identical in TIFF and PNG output. JPEG output is re-encoded.

use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::Rgb16;
use crate::meta::Metadata;
use crate::tiff_io::TiffSource;
use crate::{jpeg_io, png_io, tiff_io};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Tiff(tiff::TiffError),
    Png(String),
    Image(image::ImageError),
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Tiff(e) => write!(f, "TIFF: {e}"),
            Error::Png(e) => write!(f, "PNG: {e}"),
            Error::Image(e) => e.fmt(f),
            Error::Unsupported(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<tiff::TiffError> for Error {
    fn from(e: tiff::TiffError) -> Self {
        Error::Tiff(e)
    }
}

impl From<image::ImageError> for Error {
    fn from(e: image::ImageError) -> Self {
        Error::Image(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Tiff,
    Png,
    Jpeg,
}

impl FileType {
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "tif" | "tiff" => Ok(FileType::Tiff),
            "png" => Ok(FileType::Png),
            "jpg" | "jpeg" => Ok(FileType::Jpeg),
            _ => Err(Error::Unsupported(format!(
                "unsupported file type {:?} (use .tif, .png or .jpg)",
                path.file_name().unwrap_or_default()
            ))),
        }
    }

    /// File extensions for open/save dialogs.
    pub const EXTENSIONS: [&str; 5] = ["tif", "tiff", "png", "jpg", "jpeg"];
}

pub struct Picture {
    pub pixels: Rgb16,
    /// Alpha channel, kept untouched and written back when the format allows.
    alpha: Option<Vec<u16>>,
    /// The alpha is premultiplied (a TIFF's associated alpha); kept as it is.
    associated_alpha: bool,
    /// 8 or 16.
    bit_depth: u8,
    /// A grayscale source: the channels are equal and it is saved as grayscale.
    gray: bool,
    meta: Metadata,
    /// Set when the source is a TIFF, for saving as TIFF again.
    tiff: Option<TiffSource>,
    file_type: FileType,
    /// Width and height as opened: the metadata describes a picture of this size.
    source_size: (u32, u32),
}

/// What a reader gives back.
pub(crate) struct Decoded {
    pub pixels: Rgb16,
    pub alpha: Option<Vec<u16>>,
    pub associated_alpha: bool,
    pub bit_depth: u8,
    pub gray: bool,
    pub meta: Metadata,
    pub tiff: Option<TiffSource>,
}

/// The pixels a writer writes.
pub(crate) struct Image<'a> {
    pub pixels: &'a Rgb16,
    /// One value per pixel.
    pub alpha: Option<&'a [u16]>,
    pub associated_alpha: bool,
    pub bit_depth: u8,
    pub gray: bool,
    /// The size differs from the source's: the metadata's own record of it must follow.
    pub resized: bool,
}

impl Picture {
    pub fn open(path: &Path) -> Result<Self> {
        let file_type = FileType::from_path(path)?;
        let mut decoded = match file_type {
            FileType::Tiff => tiff_io::read(path)?,
            FileType::Png => png_io::read(path)?,
            FileType::Jpeg => jpeg_io::read(path)?,
        };
        // The profile has to fit the pixels as they are saved: a gray picture with an RGB
        // profile is saved as RGB; a profile for anything else, or too short to say, is
        // dropped.
        if let Some(icc) = &decoded.meta.icc {
            match icc.get(16..20) {
                Some(b"RGB ") => decoded.gray = false,
                Some(b"GRAY") if decoded.gray => {}
                space => {
                    let why = match space {
                        Some(other) => format!(
                            "it is for {}, not these pixels",
                            String::from_utf8_lossy(other).trim()
                        ),
                        None => "it is cut short".into(),
                    };
                    eprintln!("color profile left out: {why}");
                    decoded.meta.icc = None;
                }
            }
        }
        Ok(Picture {
            source_size: decoded.pixels.dimensions(),
            pixels: decoded.pixels,
            alpha: decoded.alpha,
            associated_alpha: decoded.associated_alpha,
            bit_depth: decoded.bit_depth,
            gray: decoded.gray,
            meta: decoded.meta,
            tiff: decoded.tiff,
            file_type,
        })
    }

    /// Wraps pixels that did not come from a file (tests, tools).
    pub fn from_pixels(pixels: Rgb16, bit_depth: u8) -> Self {
        Picture {
            source_size: pixels.dimensions(),
            pixels,
            alpha: None,
            associated_alpha: false,
            bit_depth,
            gray: false,
            meta: Metadata::default(),
            tiff: None,
            file_type: FileType::Tiff,
        }
    }

    /// EXIF orientation (1-8) of the source. Pixels are kept as stored; viewers apply this.
    pub fn orientation(&self) -> u8 {
        self.meta.orientation()
    }

    /// Width and height as the picture is viewed, after its orientation.
    pub fn display_size(&self) -> (u32, u32) {
        oriented_size(self.pixels.dimensions(), self.orientation())
    }

    pub fn width(&self) -> u32 {
        self.pixels.width()
    }

    pub fn height(&self) -> u32 {
        self.pixels.height()
    }

    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// A grayscale picture: it is held as RGB with equal channels and saved as grayscale.
    pub fn is_gray(&self) -> bool {
        self.gray
    }

    pub fn has_icc_profile(&self) -> bool {
        self.meta.icc.is_some()
    }

    /// The ICC profile, as stored in the file.
    pub fn icc_profile(&self) -> Option<&[u8]> {
        self.meta.icc.as_deref()
    }

    /// The ICC profile's description, such as "Adobe RGB (1998)".
    pub fn profile_name(&self) -> Option<String> {
        self.icc_profile().and_then(icc_description)
    }

    /// The type of the file this picture was opened from.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    #[cfg(test)]
    pub(crate) fn meta(&self) -> &Metadata {
        &self.meta
    }

    /// The alpha channel, one value per pixel, if the picture has one.
    pub fn alpha(&self) -> Option<&[u16]> {
        self.alpha.as_deref()
    }

    /// Whether the colors are premultiplied by the alpha (a TIFF's associated alpha).
    pub fn associated_alpha(&self) -> bool {
        self.associated_alpha
    }

    /// Swaps the picture's pixels and alpha channel with `pixels` and `alpha`, which may be of
    /// another size; afterwards they hold the picture's old ones. Refused, changing nothing,
    /// when the alpha doesn't fit: one value per pixel when the picture has an alpha channel,
    /// none when it hasn't.
    pub fn swap_pixels(&mut self, pixels: &mut Rgb16, alpha: &mut Option<Vec<u16>>) -> Result<()> {
        let len = pixels.width() as usize * pixels.height() as usize;
        match (&self.alpha, &*alpha) {
            (Some(_), Some(new)) if new.len() == len => {}
            (None, None) => {}
            _ => {
                return Err(Error::Unsupported(
                    "the alpha channel doesn't match the pixels".into(),
                ));
            }
        }
        std::mem::swap(&mut self.pixels, pixels);
        std::mem::swap(&mut self.alpha, alpha);
        Ok(())
    }

    /// Saves to `path` in the format its extension names. Writes a temporary file next to it,
    /// flushed to the disk, and renames it into place, so a failed save never leaves a
    /// half-written file. The file it replaces keeps its permissions; through a symbolic link,
    /// the file it points to is replaced. A file the user can't write isn't saved over.
    pub fn save(&self, path: &Path) -> Result<()> {
        let file_type = FileType::from_path(path)?;
        let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let old = fs::metadata(path).ok().filter(fs::Metadata::is_file);
        // Renaming over a file needs only its folder to be writable; as Preview does, a file
        // that is read-only to the user is left alone (opening it to write changes nothing).
        if old.is_some()
            && let Err(e) = fs::OpenOptions::new().write(true).open(path)
            && e.kind() == std::io::ErrorKind::PermissionDenied
        {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            return Err(Error::Unsupported(format!("{name} is read-only")));
        }
        let tmp = temp_path(path);
        let result = self.write(&tmp, file_type, old.as_ref()).and_then(|()| {
            if let Some(old) = &old {
                fs::set_permissions(&tmp, old.permissions())?;
            }
            fs::rename(&tmp, path)?;
            sync_directory(path);
            Ok(())
        });
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Writes the file and flushes it to the disk. It is created with the permissions of `like`
    /// (the file it will replace), so that a private file isn't readable by others meanwhile.
    fn write(&self, path: &Path, file_type: FileType, like: Option<&fs::Metadata>) -> Result<()> {
        let _ = fs::remove_file(path); // a temporary file left by a crash: its mode would stay
        let mut options = File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(like) = like {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(like.permissions().mode() & 0o777);
        }
        #[cfg(not(unix))]
        let _ = like;
        let file = options.open(path).map_err(|e| match e.kind() {
            // Made next to the file it replaces: its folder doesn't let the user write.
            std::io::ErrorKind::PermissionDenied => {
                let folder = path.parent().unwrap_or(Path::new("."));
                Error::Unsupported(format!(
                    "Gumka can't save in {}: the folder isn't writable",
                    folder.display()
                ))
            }
            _ => Error::Io(e),
        })?;
        let mut out = BufWriter::new(file);
        let image = Image {
            pixels: &self.pixels,
            alpha: self.alpha.as_deref(),
            associated_alpha: self.associated_alpha,
            bit_depth: self.bit_depth,
            gray: self.gray,
            resized: self.pixels.dimensions() != self.source_size,
        };
        match file_type {
            FileType::Tiff => tiff_io::write(&mut out, &image, self.tiff.as_ref(), &self.meta)
                .map_err(|e| match e {
                    // Offsets past 4 GB, which a TIFF can't hold.
                    Error::Tiff(tiff::TiffError::IntSizeError) => Error::Unsupported(
                        "This photo is too large for a TIFF (over 4 GB). Save it as PNG.".into(),
                    ),
                    e => e,
                })?,
            FileType::Png => png_io::write(&mut out, &image, &self.meta)?,
            FileType::Jpeg => jpeg_io::write(&mut out, &image, &self.meta)?,
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        Ok(())
    }
}

/// Makes a rename in `path`'s directory durable. Best effort, and only where a directory can
/// be opened like a file.
fn sync_directory(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(dir) = File::open(if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        })
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The gray value of an RGB pixel of a grayscale picture: exact where the channels are equal.
pub(crate) fn gray_value([r, g, b]: [u16; 3]) -> u16 {
    ((u32::from(r) + u32::from(g) + u32::from(b) + 1) / 3) as u16
}

/// Whether an EXIF orientation turns the picture by 90°, swapping width and height.
pub fn swaps_axes(orientation: u8) -> bool {
    (5..=8).contains(&orientation)
}

/// Width and height as viewed under an EXIF orientation.
pub fn oriented_size((w, h): (u32, u32), orientation: u8) -> (u32, u32) {
    if swaps_axes(orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// Maps a point of the viewed picture to the stored pixels, both as fractions (0-1) of the
/// width and height, for an EXIF orientation 1-8.
pub fn viewed_to_stored(orientation: u8, u: f32, v: f32) -> (f32, f32) {
    match orientation {
        2 => (1.0 - u, v),       // flipped horizontally
        3 => (1.0 - u, 1.0 - v), // turned 180°
        4 => (u, 1.0 - v),       // flipped vertically
        5 => (v, u),             // transposed
        6 => (v, 1.0 - u),       // turned 90° clockwise
        7 => (1.0 - v, 1.0 - u), // transversed
        8 => (1.0 - v, u),       // turned 90° counterclockwise
        _ => (u, v),
    }
}

/// The inverse of [`viewed_to_stored`]: a stored point to where it is viewed.
pub fn stored_to_viewed(orientation: u8, s: f32, t: f32) -> (f32, f32) {
    match orientation {
        2 => (1.0 - s, t),
        3 => (1.0 - s, 1.0 - t),
        4 => (s, 1.0 - t),
        5 => (t, s),
        6 => (1.0 - t, s),
        7 => (1.0 - t, 1.0 - s),
        8 => (t, 1.0 - s),
        _ => (s, t),
    }
}

/// 16-bit to 8-bit with rounding. Exact inverse of `v * 257` for values that came from 8 bits.
pub fn to_u8(v: u16) -> u8 {
    ((u32::from(v) + 128) / 257) as u8
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.gumka-tmp"))
}

/// The description of an ICC profile, such as "Adobe RGB (1998)", in English: Apple's generic
/// profiles name themselves in many languages, and lcms2 picks the first (Slovak) without one.
pub fn icc_description(icc: &[u8]) -> Option<String> {
    let profile = lcms2::Profile::new_icc(icc).ok()?;
    let text = profile.info(lcms2::InfoType::Description, lcms2::Locale::new("en_US"))?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_round_trip_is_exact() {
        for v in 0..=255u8 {
            assert_eq!(to_u8(u16::from(v) * 257), v);
        }
    }

    #[test]
    fn png_round_trip_keeps_pixels_and_depth() {
        let dir = std::env::temp_dir().join(format!("gumka-png-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for depth in [8u8, 16] {
            // Values an 8-bit file can hold exactly.
            let pixels = Rgb16::from_fn(7, 5, |x, y| {
                let v = ((x * 31 + y * 17) % 256) as u16;
                image::Rgb([v * 257, (v / 2) * 257, (255 - v) * 257])
            });
            let path = dir.join(format!("p{depth}.png"));
            Picture::from_pixels(pixels.clone(), depth)
                .save(&path)
                .unwrap();
            let back = Picture::open(&path).unwrap();
            assert_eq!(back.bit_depth(), depth);
            assert_eq!(back.pixels, pixels);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Viewing a stored image through `viewed_to_stored` must match the image crate's own
    /// `apply_orientation` for every EXIF orientation.
    #[test]
    fn orientation_maps_match_the_image_crate() {
        let (w, h) = (5u32, 3u32);
        let stored = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 0]));
        for o in 1..=8u8 {
            let mut viewed = image::DynamicImage::ImageRgb8(stored.clone());
            viewed.apply_orientation(image::metadata::Orientation::from_exif(o).unwrap());
            let viewed = viewed.to_rgb8();
            assert_eq!(
                viewed.dimensions(),
                oriented_size((w, h), o),
                "size, orientation {o}"
            );
            let (vw, vh) = viewed.dimensions();
            for (x, y, p) in viewed.enumerate_pixels() {
                // Pixel centers, as fractions.
                let (u, v) = ((x as f32 + 0.5) / vw as f32, (y as f32 + 0.5) / vh as f32);
                let (s, t) = viewed_to_stored(o, u, v);
                let (sx, sy) = ((s * w as f32) as u8, (t * h as f32) as u8);
                assert_eq!(p.0[..2], [sx, sy], "orientation {o}, viewed ({x}, {y})");
            }
        }
    }

    #[test]
    fn orientation_maps_invert() {
        for o in 1..=8u8 {
            for (u, v) in [(0.1, 0.7), (0.9, 0.2), (0.5, 0.5)] {
                let (s, t) = viewed_to_stored(o, u, v);
                let (u2, v2) = stored_to_viewed(o, s, t);
                assert!(
                    (u - u2).abs() < 1e-6 && (v - v2).abs() < 1e-6,
                    "orientation {o}"
                );
            }
        }
    }

    /// The temporary file has the mode of the file it replaces from the start (review-B #8),
    /// even where a crash left one behind.
    #[cfg(unix)]
    #[test]
    fn the_temporary_file_starts_with_the_originals_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("gumka-tmp-mode-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("private.png");
        let picture = Picture::from_pixels(Rgb16::new(3, 2), 8);
        picture.save(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let tmp = temp_path(&path);
        fs::write(&tmp, b"left by a crash").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        let old = fs::metadata(&path).unwrap();
        picture.write(&tmp, FileType::Png, Some(&old)).unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_extension_is_rejected() {
        assert!(FileType::from_path(Path::new("a.webp")).is_err());
    }

    /// Profiles named `name`, as ICC v2 writes it (a `desc` tag) and as v4 does (`mluc`).
    fn named_profiles(name: &str) -> [Vec<u8>; 2] {
        let mut text = lcms2::MLU::new(1);
        text.set_text(name, lcms2::Locale::new("en_US"));
        [2.1, 4.3].map(|version| {
            let mut profile = lcms2::Profile::new_srgb();
            profile.write_tag(
                lcms2::TagSignature::ProfileDescriptionTag,
                lcms2::Tag::MLU(&text),
            );
            profile.set_version(version);
            profile.icc().unwrap()
        })
    }

    #[test]
    fn reads_v2_and_v4_profile_names() {
        for (icc, kind) in named_profiles("Adobe RGB (1998)")
            .iter()
            .zip(["desc", "mluc"])
        {
            let mluc = icc.windows(4).any(|w| w == b"mluc");
            assert_eq!(mluc, kind == "mluc", "{kind}");
            assert_eq!(
                icc_description(icc).as_deref(),
                Some("Adobe RGB (1998)"),
                "{kind}"
            );
        }
        let [v2, _] = named_profiles("  ");
        assert_eq!(icc_description(&v2), None, "blank");
        assert_eq!(icc_description(&v2[..v2.len() / 2]), None, "cut short");
        assert_eq!(icc_description(b"too short"), None);
    }

    /// Apple's generic profiles name themselves in many languages; the English name is used.
    #[cfg(target_os = "macos")]
    #[test]
    fn apples_profiles_are_named_in_english() {
        let path = Path::new("/System/Library/ColorSync/Profiles/Generic RGB Profile.icc");
        if let Ok(icc) = fs::read(path) {
            assert_eq!(
                icc_description(&icc).as_deref(),
                Some("Generic RGB Profile")
            );
        }
    }
}
