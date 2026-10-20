// SPDX-License-Identifier: GPL-3.0-or-later

//! Tester's C4 checks through the public erase API: with a model that returns its input, the
//! fill must give back the original values (within the 8 bits the model works in), whatever
//! space the model saw them in.

use gumka_core::erase::{EraseSettings, InpaintError, Inpainter, erase};
use gumka_core::image::{GrayImage, Luma, Rgb, RgbImage};
use gumka_core::{Mask, Rgb16};
use lcms2::{CIExyY, CIExyYTRIPLE, Profile, ToneCurve};

/// A model that "fills" with exactly what it was given.
struct Identity;

impl Inpainter for Identity {
    fn input_size(&self) -> (u32, u32) {
        (256, 256)
    }

    fn inpaint(&mut self, image: &RgbImage, _mask: &GrayImage) -> Result<RgbImage, InpaintError> {
        Ok(image.clone())
    }
}

fn linear_gray() -> Vec<u8> {
    let d50 = CIExyY {
        x: 0.3457,
        y: 0.3585,
        Y: 1.0,
    };
    Profile::new_gray(&d50, &ToneCurve::new(1.0))
        .unwrap()
        .icc()
        .unwrap()
}

fn linear_srgb() -> Vec<u8> {
    let xy = |x, y| CIExyY { x, y, Y: 1.0 };
    let primaries = CIExyYTRIPLE {
        Red: xy(0.64, 0.33),
        Green: xy(0.30, 0.60),
        Blue: xy(0.15, 0.06),
    };
    let linear = ToneCurve::new(1.0);
    Profile::new_rgb(
        &xy(0.3127, 0.3290),
        &primaries,
        &[&linear, &linear, &linear],
    )
    .unwrap()
    .icc()
    .unwrap()
}

/// A neutral picture whose rows run from black to `top` (linear 16-bit), erased over a square
/// in the middle by the identity model; returns (original, result) inside the square.
fn round_trip(profile: &[u8], top: f64) -> Vec<(f64, f64)> {
    let (w, h) = (256, 256);
    let image = Rgb16::from_fn(w, h, |x, _| {
        Rgb([(f64::from(x) / 255.0 * top).round() as u16; 3])
    });
    let mask = Mask::from_fn(w, h, |x, y| {
        Luma([if (64..192).contains(&x) && (64..192).contains(&y) {
            255
        } else {
            0
        }])
    });
    let mut out = image.clone();
    erase(
        &mut out,
        &mask,
        &mut Identity,
        &EraseSettings::default(),
        Some(profile),
    )
    .unwrap()
    .unwrap();
    (80..176)
        .map(|x| {
            (
                f64::from(image.get_pixel(x, 128).0[0]),
                f64::from(out.get_pixel(x, 128).0[0]),
            )
        })
        .collect()
}

fn mean_ratio(pairs: &[(f64, f64)]) -> f64 {
    let pairs: Vec<_> = pairs.iter().filter(|(a, _)| *a > 50.0).collect();
    pairs.iter().map(|(a, b)| b / a).sum::<f64>() / pairs.len() as f64
}

/// Deep shadows in a linear picture (sRGB 0 to 0.15 is linear 0 to about 0.02) come back as they
/// were, gray or RGB alike.
#[test]
fn linear_shadows_come_back_at_their_level() {
    for (name, profile) in [
        ("linear sRGB", linear_srgb()),
        ("linear gray", linear_gray()),
    ] {
        let pairs = round_trip(&profile, 1300.0);
        let ratio = mean_ratio(&pairs);
        let sample: Vec<_> = pairs.iter().step_by(16).collect();
        assert!(
            (ratio - 1.0).abs() < 0.1,
            "{name}: fill / original {ratio:.3}; {sample:?}"
        );
    }
}

/// The same for mid-tones.
#[test]
fn linear_midtones_come_back_at_their_level() {
    for (name, profile) in [
        ("linear sRGB", linear_srgb()),
        ("linear gray", linear_gray()),
    ] {
        let pairs = round_trip(&profile, 40000.0);
        let ratio = mean_ratio(&pairs);
        let sample: Vec<_> = pairs.iter().step_by(16).collect();
        assert!(
            (ratio - 1.0).abs() < 0.02,
            "{name}: fill / original {ratio:.3}; {sample:?}"
        );
    }
}

/// On screen, a linear gray picture's shadows look as the same values do in linear sRGB (the
/// screen transform goes through the same lcms2 machinery as the model space).
#[test]
fn linear_gray_shadows_show_like_linear_rgb() {
    use gumka_core::color::{DisplayTransform, Screen};
    let values: Vec<[u16; 3]> = [60u16, 150, 300, 600, 1000, 1300, 3000, 20000]
        .iter()
        .map(|&v| [v; 3])
        .collect();
    let show = |icc: &[u8]| {
        let t = DisplayTransform::new(Some(icc), Screen::Srgb);
        assert!(t.uses_own_profile());
        let mut out = vec![[0u8; 3]; values.len()];
        t.convert(&values, &mut out);
        out
    };
    let (gray, rgb) = (show(&linear_gray()), show(&linear_srgb()));
    let far: Vec<_> = values
        .iter()
        .zip(gray.iter().zip(&rgb))
        .filter(|(_, (g, r))| g[0].abs_diff(r[0]) > 2)
        .collect();
    assert!(
        far.is_empty(),
        "value, (gray shown, linear sRGB shown): {far:?}"
    );
}

/// A "hybrid" camera-style profile, as many of Capture One's are: RGB to XYZ through a table
/// (A2B0, here linear with ProPhoto's primaries) and also matrix and curve tags (here gamma 1.8),
/// with no table back. lcms2 converts out of it through the table but into it through the matrix.
fn hybrid_profile() -> Vec<u8> {
    let s15 = |v: f64| ((v * 65536.0).round() as i32).to_be_bytes();
    let u16s = |out: &mut Vec<u8>, v: &[u16]| out.extend(v.iter().flat_map(|v| v.to_be_bytes()));
    let (r, g, b) = (
        [0.7977, 0.2880, 0.0],
        [0.1352, 0.7119, 0.0],
        [0.0313, 0.0001, 0.8249],
    );
    // lut16: 3 in, 3 out, a 2-point grid, identity matrix, 256-entry linear input curves.
    let mut a2b0 = b"mft2\0\0\0\0\x03\x03\x02\0".to_vec();
    for i in 0..9 {
        a2b0.extend(s15(if i % 4 == 0 { 1.0 } else { 0.0 }));
    }
    u16s(&mut a2b0, &[256, 2]);
    let curve: Vec<u16> = (0..256).map(|i| (i * 257) as u16).collect();
    for _ in 0..3 {
        u16s(&mut a2b0, &curve);
    }
    for i in 0..8 {
        let on = [(i >> 2) & 1, (i >> 1) & 1, i & 1].map(f64::from);
        let xyz: [f64; 3] = std::array::from_fn(|c| on[0] * r[c] + on[1] * g[c] + on[2] * b[c]);
        u16s(
            &mut a2b0,
            &xyz.map(|v| (v * 32768.0).round().min(65535.0) as u16),
        );
    }
    for _ in 0..3 {
        u16s(&mut a2b0, &[0, 65535]);
    }
    let mut white = b"XYZ \0\0\0\0".to_vec();
    for v in [0.9642, 1.0, 0.8249] {
        white.extend(s15(v));
    }
    let tags = [(*b"A2B0", a2b0), (*b"wtpt", white)];
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

    // Add the matrix and curves.
    use lcms2::{CIEXYZ, Tag, TagSignature};
    let mut profile = Profile::new_icc(&icc).unwrap();
    let trc = ToneCurve::new(1.8);
    for (sig, [x, y, z]) in [
        (TagSignature::RedColorantTag, r),
        (TagSignature::GreenColorantTag, g),
        (TagSignature::BlueColorantTag, b),
    ] {
        assert!(profile.write_tag(sig, Tag::CIEXYZ(&CIEXYZ { X: x, Y: y, Z: z })));
    }
    for sig in [
        TagSignature::RedTRCTag,
        TagSignature::GreenTRCTag,
        TagSignature::BlueTRCTag,
    ] {
        assert!(profile.write_tag(sig, Tag::ToneCurve(&trc)));
    }
    assert!(profile.is_matrix_shaper());
    profile.icc().unwrap()
}

/// Whatever the profile, a converted fill must come back at the level it went in at: a profile
/// whose two directions disagree must not be converted (or the fill is a seam).
#[test]
fn a_profile_whose_directions_disagree_gives_back_its_values() {
    let pairs = round_trip(&hybrid_profile(), 40000.0);
    let ratio = mean_ratio(&pairs);
    let sample: Vec<_> = pairs.iter().step_by(16).collect();
    assert!(
        (ratio - 1.0).abs() < 0.02,
        "fill / original {ratio:.3}; {sample:?}"
    );
}
