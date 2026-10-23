// SPDX-License-Identifier: GPL-3.0-or-later

//! Color with Little CMS: converting a picture's stored values into the screen's color space,
//! and, for pictures whose values would mislead a model, into the model's.
//!
//! Display conversion only affects what is shown. Saving writes the stored values back with the
//! file's own profile, and the models get them as they are, except where [`ModelSpace`] says.

use std::sync::Mutex;

use lcms2::{
    CIExyY, CIExyYTRIPLE, ColorSpaceSignature, DisallowCache, Flags, GlobalContext, Intent,
    PixelFormat, Profile, TagSignature, ToneCurve, Transform,
};

use crate::Rgb16;

/// The color space the window is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Srgb,
    /// Apple's wide-gamut space: P3 primaries, D65 white, the sRGB curve.
    DisplayP3,
}

/// Grayscale pictures are held as RGB with equal channels: read the first, skip the other two.
const GRAY_16_IN_RGB: PixelFormat = PixelFormat(PixelFormat::GRAY_16.0 | (2 << 7));

/// Converts 16-bit RGB in the picture's profile to 8-bit RGB in the screen's.
pub struct DisplayTransform {
    conversion: Conversion,
    own_profile: bool,
}

enum Conversion {
    Lcms(Transform<[u16; 3], [u8; 3], GlobalContext, DisallowCache>),
    /// A gray picture: what each of the 65,536 values shows as, converted once without lcms2's
    /// optimization. Optimized, lcms2 samples gray on a grid too coarse for linear shadows
    /// (they show half as bright); unoptimized, it is ten times slower per pixel.
    Gray(Box<[[u8; 3]]>),
}

impl DisplayTransform {
    /// A transform from the picture's ICC profile, or from sRGB when there is none or it can't
    /// be used.
    pub fn new(icc: Option<&[u8]>, screen: Screen) -> Self {
        let target = screen_profile(screen);
        if let Some(icc) = icc {
            match Profile::new_icc(icc).and_then(|source| build(&source, &target)) {
                Ok(conversion) => {
                    return Self {
                        conversion,
                        own_profile: true,
                    };
                }
                Err(error) => eprintln!(
                    "can't use the picture's color profile ({}); showing it as sRGB",
                    one_line(&error)
                ),
            }
        }
        Self {
            conversion: build(&Profile::new_srgb(), &target)
                .expect("sRGB to the screen is always possible"),
            own_profile: false,
        }
    }

    /// Whether the picture's own profile is used, rather than sRGB in its place.
    pub fn uses_own_profile(&self) -> bool {
        self.own_profile
    }

    /// Converts `src` into `dst`, which must be the same length.
    pub fn convert(&self, src: &[[u16; 3]], dst: &mut [[u8; 3]]) {
        match &self.conversion {
            Conversion::Lcms(transform) => transform.transform_pixels(src, dst),
            Conversion::Gray(table) => {
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = table[usize::from(s[0])];
                }
            }
        }
    }
}

/// Relative colorimetric with black point compensation, as photo editors preview. No cache
/// makes the transform shareable between threads.
fn build(source: &Profile, target: &Profile) -> lcms2::LCMSResult<Conversion> {
    let intent = Intent::RelativeColorimetric;
    match source.color_space() {
        ColorSpaceSignature::RgbData => Ok(Conversion::Lcms(Transform::new_flags_context(
            GlobalContext::new(),
            source,
            PixelFormat::RGB_16,
            target,
            PixelFormat::RGB_8,
            intent,
            Flags::NO_CACHE | Flags::BLACKPOINT_COMPENSATION,
        )?)),
        ColorSpaceSignature::GrayData => {
            let gray: Transform<u16, [u8; 3]> = Transform::new_flags(
                source,
                PixelFormat::GRAY_16,
                target,
                PixelFormat::RGB_8,
                intent,
                Flags::BLACKPOINT_COMPENSATION | Flags::NO_OPTIMIZE,
            )?;
            let values: Vec<u16> = (0..=u16::MAX).collect();
            let mut table = vec![[0; 3]; values.len()];
            gray.transform_pixels(&values, &mut table);
            Ok(Conversion::Gray(table.into_boxed_slice()))
        }
        _ => Err(lcms2::Error::ObjectCreationError),
    }
}

/// An lcms2 error on one line: its text holds a line break.
fn one_line(error: &lcms2::Error) -> String {
    error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The color space models see a picture in when the picture's own would mislead them: linear
/// values (they would see them far too dark) or very wide primaries (far too dull). They were
/// trained on ordinary photos, so they get Display P3, which holds them and is close to most
/// pictures. Both ways relative colorimetric with black point compensation, 16 bits.
pub(crate) struct ModelSpace {
    to_model: Transform<[u16; 3], [u16; 3], GlobalContext, DisallowCache>,
    to_picture: Transform<[u16; 3], [u16; 3], GlobalContext, DisallowCache>,
    gray: bool,
    /// Colors beyond P3 may be clipped: the primaries are wider than P3, or unknown (a linear
    /// color profile that converts through a table).
    may_clip: bool,
}

/// Linear: a mid-gray value this far below the usual gamma of about 2.2.
const LINEAR_GAMMA: f64 = 1.5;
/// Very wide: an xy triangle of the primaries larger than this, as Rec. 2020 and ProPhoto have
/// (Adobe RGB and P3 are under 0.15).
const WIDE_AREA: f64 = 0.18;

/// Tags holding a conversion as a table. lcms2 prefers them over a matrix in the same profile,
/// which many camera profiles also carry.
const TABLE_TAGS: [TagSignature; 9] = [
    TagSignature::AToB0Tag,
    TagSignature::AToB1Tag,
    TagSignature::AToB2Tag,
    TagSignature::BToA0Tag,
    TagSignature::BToA1Tag,
    TagSignature::BToA2Tag,
    TagSignature::DToB0Tag,
    TagSignature::DToB1Tag,
    TagSignature::DToB2Tag,
];

/// How far (as a fraction of the value) a round trip may come back off: well under one 8-bit
/// level, while a profile whose two ways disagree is off by far more.
const ROUND_TRIP_TOLERANCE: f64 = 0.02;

/// Values a conversion must give back after going to the model's space and back: neutrals
/// from deep shadow to light, and (for RGB) mild colors well inside P3.
const ROUND_TRIP: [[u16; 3]; 9] = [
    [300; 3],
    [1300; 3],
    [6500; 3],
    [20000; 3],
    [32768; 3],
    [52000; 3],
    [36000, 32768, 32768],
    [32768, 36000, 32768],
    [32768, 32768, 36000],
];

impl ModelSpace {
    /// The model's space for a picture in `icc`, or `None` when its values go to the model as
    /// they are: no profile, not RGB or gray, an ordinary one, or one that doesn't convert both
    /// ways and back to what it was (many camera profiles only convert from the camera, others
    /// leave by a table and come back by a matrix).
    ///
    /// A profile's primaries are only measured when it is a matrix and nothing else: through a
    /// table (camera profiles) pure red, green and blue say nothing about the gamut. Its curve
    /// always is.
    pub fn for_profile(icc: Option<&[u8]>) -> Option<Self> {
        let icc = icc?;
        let profile = Profile::new_icc(icc).ok()?;
        let (format, gray) = match profile.color_space() {
            ColorSpaceSignature::RgbData => (PixelFormat::RGB_16, false),
            ColorSpaceSignature::GrayData => (GRAY_16_IN_RGB, true),
            _ => return None,
        };
        let table = TABLE_TAGS.iter().any(|&tag| profile.has_tag(tag));
        let matrix = profile.is_matrix_shaper() && !table;
        let (gamma, area) = measure(&profile, gray)?;
        let linear = gamma < LINEAR_GAMMA;
        let wide = matrix && area.is_some_and(|a| a > WIDE_AREA);
        if !linear && !wide {
            return None;
        }
        // A table's gamut can't be measured, so a linear one may hold colors beyond P3; gray
        // never does.
        let may_clip = !gray && (wide || (linear && !matrix));
        let skip = |why: String| {
            let what = if linear { "linear" } else { "very wide" };
            log_once(
                icc,
                format!(
                    "the picture's profile is {what}, but {why}; the model gets its values as they are"
                ),
            );
            None
        };
        let p3 = display_p3();
        let both = convert(&profile, format, &p3, PixelFormat::RGB_16)
            .and_then(|to| Ok((to, convert(&p3, PixelFormat::RGB_16, &profile, format)?)));
        let space = match both {
            Ok((to_model, to_picture)) => Self {
                to_model,
                to_picture,
                gray,
                may_clip,
            },
            Err(error) => {
                return skip(format!(
                    "lcms2 can't convert it both ways ({})",
                    one_line(&error)
                ));
            }
        };
        // A fill comes back the way the picture's values went: they must agree, or it shows.
        let mut probe =
            Rgb16::from_raw(ROUND_TRIP.len() as u32, 1, ROUND_TRIP.concat()).expect("probe size");
        space.to_model(&mut probe);
        space.to_picture(&mut probe);
        let probes = if gray {
            &ROUND_TRIP[..6]
        } else {
            &ROUND_TRIP[..]
        };
        let worst = probes
            .iter()
            .flatten()
            .zip(probe.as_raw())
            .map(|(&a, &b)| f64::from(a.abs_diff(b)) / f64::from(a))
            .fold(0.0, f64::max);
        if worst > ROUND_TRIP_TOLERANCE {
            return skip(format!(
                "a round trip through P3 comes back {:.0}% off",
                worst * 100.0
            ));
        }
        Some(space)
    }

    /// Converts pixels from the picture's space into the model's.
    pub fn to_model(&self, pixels: &mut Rgb16) {
        apply(&self.to_model, pixels);
    }

    /// Converts the model's pixels into the picture's space (gray for a gray profile).
    pub fn to_picture(&self, pixels: &mut Rgb16) {
        apply(&self.to_picture, pixels);
        if self.gray {
            for p in pixels.pixels_mut() {
                p.0 = [p.0[0]; 3];
            }
        }
    }
}

/// Whether models see pictures with this profile converted to Display P3 while its colors may
/// reach beyond P3, so that those may be clipped in what the models change: a very wide matrix
/// profile (ProPhoto, Rec. 2020), or a linear one that converts through a table, whose gamut
/// can't be told. A linear matrix profile that P3 holds (linear sRGB) loses nothing, and a
/// profile that isn't converted (it fails the round trip) keeps its values.
pub fn may_clip_beyond_p3(icc: Option<&[u8]>) -> bool {
    ModelSpace::for_profile(icc).is_some_and(|space| space.may_clip)
}

/// Converts `pixels` in place, split over the cores by rows when there are many: Upscale
/// converts whole tiles, at the model's output size too.
fn apply(
    transform: &Transform<[u16; 3], [u16; 3], GlobalContext, DisallowCache>,
    pixels: &mut Rgb16,
) {
    let (width, height) = pixels.dimensions();
    let read = u64::from(width) * u64::from(height);
    let values = pixels.as_chunks_mut::<3>().0;
    crate::by_rows(values, width as usize, read, |_, rows| {
        let src = rows.to_vec();
        transform.transform_pixels(&src, rows);
    });
}

/// Between a picture's profile and P3 (either way), shareable between threads. Unoptimized:
/// lcms2's optimization loses shadows (gray and tables are sampled on a coarse grid, and even
/// matrix ProPhoto comes back 9% dark near black). That costs more per pixel, so `apply` uses
/// every core on large pictures.
fn convert(
    from: &Profile,
    from_format: PixelFormat,
    to: &Profile,
    to_format: PixelFormat,
) -> lcms2::LCMSResult<Transform<[u16; 3], [u16; 3], GlobalContext, DisallowCache>> {
    Transform::new_flags_context(
        GlobalContext::new(),
        from,
        from_format,
        to,
        to_format,
        Intent::RelativeColorimetric,
        Flags::NO_CACHE | Flags::BLACKPOINT_COMPENSATION | Flags::NO_OPTIMIZE,
    )
}

/// Prints `message` about the profile `icc` once, not on every erase of the picture.
fn log_once(icc: &[u8], message: String) {
    use std::hash::{DefaultHasher, Hash, Hasher};
    static LOGGED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let mut hasher = DefaultHasher::new();
    icc.hash(&mut hasher);
    let id = hasher.finish();
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if !logged.contains(&id) {
        logged.push(id);
        eprintln!("{message}");
    }
}

/// A profile's effective gamma at mid-gray, `ln(Y(0.5) / Y(1)) / ln(0.5)`, and for RGB the area
/// of the xy triangle of its pure red, green and blue, both through lcms2 into XYZ.
fn measure(profile: &Profile, gray: bool) -> Option<(f64, Option<f64>)> {
    let xyz = Profile::new_xyz();
    let intent = Intent::RelativeColorimetric;
    let gamma = |mid: [f64; 3], white: [f64; 3]| (mid[1] / white[1]).ln() / 0.5f64.ln();
    if gray {
        let t: Transform<f64, [f64; 3]> = Transform::new(
            profile,
            PixelFormat::GRAY_DBL,
            &xyz,
            PixelFormat::XYZ_DBL,
            intent,
        )
        .ok()?;
        let mut out = [[0.0; 3]; 2];
        t.transform_pixels(&[0.5, 1.0], &mut out);
        return Some((gamma(out[0], out[1]), None));
    }
    let t: Transform<[f64; 3], [f64; 3]> = Transform::new(
        profile,
        PixelFormat::RGB_DBL,
        &xyz,
        PixelFormat::XYZ_DBL,
        intent,
    )
    .ok()?;
    let probes = [
        [0.5; 3],
        [1.0; 3],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
    ];
    let mut out = [[0.0; 3]; 5];
    t.transform_pixels(&probes, &mut out);
    let xy = |[x, y, z]: [f64; 3]| (x / (x + y + z), y / (x + y + z));
    let ((rx, ry), (gx, gy), (bx, by)) = (xy(out[2]), xy(out[3]), xy(out[4]));
    let area = ((gx - rx) * (by - ry) - (bx - rx) * (gy - ry)).abs() / 2.0;
    Some((gamma(out[0], out[1]), Some(area)))
}

fn screen_profile(screen: Screen) -> Profile {
    match screen {
        Screen::Srgb => Profile::new_srgb(),
        Screen::DisplayP3 => display_p3(),
    }
}

/// Display P3, as macOS defines it.
fn display_p3() -> Profile {
    let xy = |x, y| CIExyY { x, y, Y: 1.0 };
    let d65 = xy(0.3127, 0.3290);
    let primaries = CIExyYTRIPLE {
        Red: xy(0.680, 0.320),
        Green: xy(0.265, 0.690),
        Blue: xy(0.150, 0.060),
    };
    // The sRGB curve: (a·x + b)^g above d, c·x below.
    let curve =
        ToneCurve::new_parametric(4, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
            .expect("valid curve parameters");
    Profile::new_rgb(&d65, &primaries, &[&curve, &curve, &curve]).expect("valid primaries")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn xy(x: f64, y: f64) -> CIExyY {
        CIExyY { x, y, Y: 1.0 }
    }

    const D65: (f64, f64) = (0.3127, 0.3290);
    const D50: (f64, f64) = (0.3457, 0.3585);
    const SRGB: [(f64, f64); 3] = [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)];
    const ADOBE: [(f64, f64); 3] = [(0.64, 0.33), (0.21, 0.71), (0.15, 0.06)];
    const REC2020: [(f64, f64); 3] = [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];
    const PROPHOTO: [(f64, f64); 3] = [(0.7347, 0.2653), (0.1596, 0.8404), (0.0366, 0.0001)];

    /// A matrix-shaper RGB profile with one gamma for all three channels.
    pub(crate) fn rgb_profile(
        white: (f64, f64),
        primaries: [(f64, f64); 3],
        gamma: f64,
    ) -> Vec<u8> {
        let [r, g, b] = primaries.map(|(x, y)| xy(x, y));
        let curve = ToneCurve::new(gamma);
        let triple = CIExyYTRIPLE {
            Red: r,
            Green: g,
            Blue: b,
        };
        Profile::new_rgb(&xy(white.0, white.1), &triple, &[&curve, &curve, &curve])
            .unwrap()
            .icc()
            .unwrap()
    }

    /// sRGB's primaries with linear values, as from a raw converter's linear export.
    pub(crate) fn linear_srgb() -> Vec<u8> {
        rgb_profile(D65, SRGB, 1.0)
    }

    pub(crate) fn prophoto() -> Vec<u8> {
        rgb_profile(D50, PROPHOTO, 1.8)
    }

    fn gray_profile(gamma: f64) -> Vec<u8> {
        Profile::new_gray(&xy(D50.0, D50.1), &ToneCurve::new(gamma))
            .unwrap()
            .icc()
            .unwrap()
    }

    /// What a table profile has for converting into it.
    #[derive(Clone, Copy, PartialEq)]
    enum Back {
        Nothing,
        /// A table that maps everything to black.
        Meaningless,
        /// The inverse of its way out.
        Inverse,
    }

    /// An input (camera-like) ICC v2 profile whose RGB to XYZ goes through a table (lut16), not
    /// a matrix: `gamma` curves, then ProPhoto's primaries. `back` says what table it has back,
    /// for lcms2 to convert into it.
    fn table_profile(gamma: f64, back: Back) -> Vec<u8> {
        let s15 = |v: f64| ((v * 65536.0).round() as i32).to_be_bytes();
        let u16s = |values: &mut Vec<u8>, v: &[u16]| {
            values.extend(v.iter().flat_map(|v| v.to_be_bytes()));
        };
        // lut16: 3 in, 3 out, a 2-point grid, a matrix, `entries`-long input curves, then the
        // grid (as stored), then 2-entry (identity) output curves.
        let lut16 =
            |matrix: [f64; 9], entries: usize, curve: &dyn Fn(f64) -> f64, grid: &[[u16; 3]; 8]| {
                let mut t = b"mft2\0\0\0\0\x03\x03\x02\0".to_vec();
                for v in matrix {
                    t.extend(s15(v));
                }
                u16s(&mut t, &[entries as u16, 2]);
                let input: Vec<u16> = (0..entries)
                    .map(|i| (curve(i as f64 / (entries - 1) as f64) * 65535.0).round() as u16)
                    .collect();
                for _ in 0..3 {
                    u16s(&mut t, &input);
                }
                for corner in grid {
                    u16s(&mut t, corner);
                }
                for _ in 0..3 {
                    u16s(&mut t, &[0, 65535]);
                }
                t
            };
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        // ProPhoto's red, green and blue in XYZ (D50); grid corners in r, g, b order.
        let (r, g, b) = (
            [0.7977, 0.2880, 0.0],
            [0.1352, 0.7119, 0.0],
            [0.0313, 0.0001, 0.8249],
        );
        let corners: [[f64; 3]; 8] =
            std::array::from_fn(|i| [(i >> 2) & 1, (i >> 1) & 1, i & 1].map(|c| c as f64));
        // PCS XYZ in lut16: 1.0 is 0x8000.
        let grid = corners.map(|on| {
            [0, 1, 2].map(|c| {
                let v = on[0] * r[c] + on[1] * g[c] + on[2] * b[c];
                (v * 32768.0).round().min(65535.0) as u16
            })
        });
        let mut tags = vec![(*b"A2B0", lut16(identity, 256, &|x| x.powf(gamma), &grid))];
        match back {
            Back::Nothing => {}
            Back::Meaningless => {
                tags.push((*b"B2A0", lut16(identity, 2, &|x| x, &[[0; 3]; 8])));
            }
            Back::Inverse => {
                // XYZ to RGB is the primaries' matrix inverted, on XYZ as lut16 holds it (1.0
                // at 0x8000 of 0xFFFF), then the curves undone through 4096 entries, then an
                // identity grid.
                let m = [r[0], g[0], b[0], r[1], g[1], b[1], r[2], g[2], b[2]];
                let cof = |i: usize, j: usize| {
                    let (a, b) = ([(i + 1) % 3, (i + 2) % 3], [(j + 1) % 3, (j + 2) % 3]);
                    m[a[0] * 3 + b[0]] * m[a[1] * 3 + b[1]]
                        - m[a[0] * 3 + b[1]] * m[a[1] * 3 + b[0]]
                };
                let det = (0..3).map(|j| m[j] * cof(0, j)).sum::<f64>();
                let scale = 65535.0 / 32768.0 / det;
                let inverse: [f64; 9] = std::array::from_fn(|k| cof(k % 3, k / 3) * scale);
                let identity_grid = corners.map(|on| on.map(|v| (v * 65535.0) as u16));
                let curve = |x: f64| x.powf(1.0 / gamma);
                tags.push((*b"B2A0", lut16(inverse, 4096, &curve, &identity_grid)));
            }
        }
        let mut white = b"XYZ \0\0\0\0".to_vec();
        for v in [0.9642, 1.0, 0.8249] {
            white.extend(s15(v));
        }
        tags.push((*b"wtpt", white));

        let mut header = vec![0u8; 128];
        header[8..12].copy_from_slice(&[2, 0x10, 0, 0]);
        header[12..24].copy_from_slice(b"scnrRGB XYZ ");
        header[36..40].copy_from_slice(b"acsp");
        for (i, v) in [0.9642, 1.0, 0.8249].into_iter().enumerate() {
            header[68 + 4 * i..72 + 4 * i].copy_from_slice(&s15(v));
        }
        let mut table = (tags.len() as u32).to_be_bytes().to_vec();
        let mut data = Vec::new();
        let start = 128 + 4 + 12 * tags.len();
        for (signature, tag) in &tags {
            table.extend_from_slice(signature);
            table.extend(((start + data.len()) as u32).to_be_bytes());
            table.extend((tag.len() as u32).to_be_bytes());
            data.extend_from_slice(tag);
            data.resize(data.len().next_multiple_of(4), 0);
        }
        let mut icc = [header, table, data].concat();
        let size = (icc.len() as u32).to_be_bytes();
        icc[0..4].copy_from_slice(&size);
        icc
    }

    /// Which profiles get the model's space: linear ones, and very wide matrix ones (review-C
    /// #1, #2: never one lcms2 can't convert into or back from, and not by the primaries of a
    /// table).
    #[test]
    fn linear_and_very_wide_profiles_are_converted_for_the_model() {
        let p3 = display_p3().icc().unwrap();
        let cases: [(&str, Vec<u8>, bool); 12] = [
            ("sRGB", rgb_profile(D65, SRGB, 2.2), false),
            ("Adobe RGB", rgb_profile(D65, ADOBE, 2.2), false),
            ("Display P3", p3, false),
            ("Generic RGB, gamma 1.8", rgb_profile(D65, SRGB, 1.8), false),
            ("linear sRGB", linear_srgb(), true),
            ("ProPhoto", prophoto(), true),
            ("Rec. 2020", rgb_profile(D65, REC2020, 2.2), true),
            ("gray 2.2", gray_profile(2.2), false),
            ("linear gray", gray_profile(1.0), true),
            // Its way back is meaningless: the round trip doesn't come back (round 2).
            ("linear table", table_profile(1.0, Back::Meaningless), false),
            ("wide table", table_profile(2.2, Back::Meaningless), false),
            (
                "linear table, both ways",
                table_profile(1.0, Back::Inverse),
                true,
            ),
        ];
        for (name, icc, converted) in cases {
            let space = ModelSpace::for_profile(Some(&icc));
            assert_eq!(space.is_some(), converted, "{name}");
        }
        let table = table_profile(1.0, Back::Nothing);
        assert!(!Profile::new_icc(&table).unwrap().is_matrix_shaper());
        assert!(
            ModelSpace::for_profile(Some(&table)).is_none(),
            "linear, but there's no way back into it"
        );
        let lab = Profile::new_lab4_context(GlobalContext::new(), &xy(D50.0, D50.1)).unwrap();
        for icc in [
            None,
            Some(b"not a profile".to_vec()),
            Some(lab.icc().unwrap()),
        ] {
            assert!(ModelSpace::for_profile(icc.as_deref()).is_none());
        }
    }

    /// Colors beyond P3 may be clipped for primaries wider than P3, and for a linear table,
    /// whose gamut can't be told; a linear matrix profile within P3 loses nothing (review: the
    /// Upscale hint showed for linear sRGB).
    #[test]
    fn profiles_that_may_clip_beyond_p3() {
        for (name, icc, clips) in [
            ("linear sRGB", linear_srgb(), false),
            ("linear gray", gray_profile(1.0), false),
            ("Adobe RGB", rgb_profile(D65, ADOBE, 2.2), false),
            ("sRGB", rgb_profile(D65, SRGB, 2.2), false),
            ("ProPhoto", prophoto(), true),
            ("linear ProPhoto", rgb_profile(D50, PROPHOTO, 1.0), true),
            ("Rec. 2020", rgb_profile(D65, REC2020, 2.2), true),
            (
                "wide table, not converted",
                table_profile(2.2, Back::Inverse),
                false,
            ),
            // Converted for its curve, and a table doesn't tell its gamut (review round 2).
            ("linear table", table_profile(1.0, Back::Inverse), true),
        ] {
            assert_eq!(may_clip_beyond_p3(Some(&icc)), clips, "{name}");
        }
        assert!(!may_clip_beyond_p3(None));
    }

    /// Large pictures are converted on every core, to the same values as on one.
    #[test]
    fn a_large_conversion_is_the_same_on_many_threads() {
        let space = ModelSpace::for_profile(Some(&prophoto())).unwrap();
        let pixels = Rgb16::from_fn(331, 257, |x, y| {
            image::Rgb([(x * 197) as u16, (y * 251) as u16, ((x ^ y) * 89) as u16])
        });
        assert!(pixels.len() as u64 / 3 >= crate::PARALLEL_FROM);
        for transform in [&space.to_model, &space.to_picture] {
            let mut threaded = pixels.clone();
            apply(transform, &mut threaded);
            let mut one = pixels.as_raw().as_chunks::<3>().0.to_vec();
            transform.transform_pixels(pixels.as_raw().as_chunks::<3>().0, &mut one);
            assert_eq!(threaded.as_raw().as_chunks::<3>().0, &one[..]);
        }
    }

    /// Real profiles: Apple's, as shipped with macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profiles_are_judged_as_expected() {
        let dir = std::path::Path::new("/System/Library/ColorSync/Profiles");
        for (file, converted) in [
            ("sRGB Profile.icc", false),
            ("Display P3.icc", false),
            ("AdobeRGB1998.icc", false),
            ("DCI(P3) RGB.icc", false),
            ("ITU-709.icc", false),
            ("Generic RGB Profile.icc", false),
            ("Generic Gray Gamma 2.2 Profile.icc", false),
            ("Generic Gray Profile.icc", false),
            ("ITU-2020.icc", true),
            ("ROMM RGB.icc", true),
            ("ACESCG Linear.icc", true),
        ] {
            let Ok(icc) = std::fs::read(dir.join(file)) else {
                continue;
            };
            let space = ModelSpace::for_profile(Some(&icc));
            assert_eq!(space.is_some(), converted, "{file}");
        }
    }

    /// Linear values go to the model as P3 values and come back as they were, give or take
    /// the 8 bits the model works in.
    #[test]
    fn the_model_space_round_trips() {
        let space = ModelSpace::for_profile(Some(&linear_srgb())).unwrap();
        // Linear 0.214 is sRGB's (and P3's) 0.5; white and black stay.
        let mut pixels = Rgb16::from_raw(
            3,
            1,
            [14025, 14025, 14025, 0, 0, 0, 65535, 65535, 65535].to_vec(),
        )
        .unwrap();
        let before = pixels.clone();
        space.to_model(&mut pixels);
        assert!(
            pixels.as_raw()[..3]
                .iter()
                .all(|&v| v.abs_diff(32896) < 300),
            "{pixels:?}"
        );
        assert_eq!(pixels.as_raw()[3..6], [0, 0, 0]);
        assert!(
            pixels.as_raw()[6..].iter().all(|&v| v > 65400),
            "{pixels:?}"
        );
        space.to_picture(&mut pixels);
        for (a, b) in pixels.as_raw().iter().zip(before.as_raw()) {
            assert!(a.abs_diff(*b) < 100, "{a} vs {b}");
        }

        // A gray picture comes back gray.
        let space = ModelSpace::for_profile(Some(&gray_profile(1.0))).unwrap();
        let mut gray = Rgb16::from_raw(1, 1, vec![20000; 3]).unwrap();
        space.to_model(&mut gray);
        gray.as_mut()[0] += 500; // the model's fill needn't be neutral
        space.to_picture(&mut gray);
        let p = gray.as_raw();
        assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
    }

    fn show(transform: &DisplayTransform, colors: &[[u16; 3]]) -> Vec<[u8; 3]> {
        let mut out = vec![[0; 3]; colors.len()];
        transform.convert(colors, &mut out);
        out
    }

    fn icc(profile: &Profile) -> Vec<u8> {
        profile.icc().unwrap()
    }

    fn close(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tolerance)
    }

    const PROBES: [[u16; 3]; 6] = [
        [0, 0, 0],
        [65535, 65535, 65535],
        [32896, 32896, 32896],
        [65535, 0, 0],
        [0, 65535, 0],
        [12850, 25700, 51400],
    ];

    #[test]
    fn srgb_on_an_srgb_screen_is_unchanged() {
        let t = DisplayTransform::new(None, Screen::Srgb);
        assert!(!t.uses_own_profile());
        for (input, output) in PROBES.iter().zip(show(&t, &PROBES)) {
            let expected = input.map(|v| ((u32::from(v) + 128) / 257) as u8);
            assert!(close(output, expected, 1), "{input:?} -> {output:?}");
        }
    }

    #[test]
    fn display_p3_matches_reference_values() {
        // sRGB red is inside P3; its P3 value is well known.
        let t = DisplayTransform::new(None, Screen::DisplayP3);
        let [red, gray] = show(&t, &[[65535, 0, 0], [32896, 32896, 32896]])[..] else {
            unreachable!()
        };
        assert!(close(red, [234, 51, 35], 1), "{red:?}");
        // Same white and curve: neutrals stay as they are.
        assert!(close(gray, [128, 128, 128], 1), "{gray:?}");
    }

    #[test]
    fn a_picture_in_display_p3_shows_as_is_on_a_p3_screen() {
        let p3 = icc(&display_p3());
        let t = DisplayTransform::new(Some(&p3), Screen::DisplayP3);
        assert!(t.uses_own_profile());
        for (input, output) in PROBES.iter().zip(show(&t, &PROBES)) {
            let expected = input.map(|v| ((u32::from(v) + 128) / 257) as u8);
            assert!(close(output, expected, 1), "{input:?} -> {output:?}");
        }
    }

    #[test]
    fn grayscale_profiles_keep_grays_neutral() {
        let d50 = CIExyY {
            x: 0.3457,
            y: 0.3585,
            Y: 1.0,
        };
        let gray = Profile::new_gray(&d50, &ToneCurve::new(2.2)).unwrap();
        let t = DisplayTransform::new(Some(&icc(&gray)), Screen::DisplayP3);
        assert!(t.uses_own_profile());
        let [black, mid, white] = show(&t, &[[0; 3], [32896; 3], [65535; 3]])[..] else {
            unreachable!()
        };
        assert_eq!((black, white), ([0; 3], [255; 3]));
        assert!(mid[0] == mid[1] && mid[1] == mid[2], "{mid:?}");
    }

    #[test]
    fn unusable_profiles_fall_back_to_srgb() {
        let t = DisplayTransform::new(Some(b"not a profile"), Screen::Srgb);
        assert!(!t.uses_own_profile());
        assert_eq!(show(&t, &[[65535, 0, 0]]), [[255, 0, 0]]);
    }
}
