// SPDX-License-Identifier: GPL-3.0-or-later

//! Saving across TIFF, PNG and JPEG: the metadata goes along, thumbnails don't, grayscale
//! stays grayscale, and odd files are handled.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use tiff::Directory;
use tiff::decoder::Decoder;
use tiff::decoder::ifd::Value;
use tiff::encoder::{Rational, TiffEncoder, colortype};
use tiff::tags::{ResolutionUnit, Tag, Type, ValueBuffer};

use crate::Rgb16;
use crate::meta::{Density, DensityUnit, EXIF_DIRECTORY, encode_raw_profile};
use crate::picture::Picture;
use crate::tiff_io::TagDirectory;

const XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'>gumka</x:xmpmeta>";
/// One IPTC record (caption): 12 bytes.
const IPTC: &[u8] = &[
    0x1c, 0x02, 0x78, 0x00, 0x07, b'c', b'a', b'p', b't', b'i', b'o', b'n',
];
const THUMBNAIL: &[u8] = b"OLD-THUMBNAIL-OLD-THUMBNAIL";
/// Photoshop's resolution resource, to have one that stays.
const RESOLUTION_INFO: &[u8] = &[0, 0x2C, 0, 0, 0, 1, 0, 1, 0, 0x2C, 0, 0, 0, 1, 0, 1];
const DATE: &str = "2026:10:19 12:00:00";

const GPS_DIRECTORY: u16 = 34853;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gumka-rt-{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Profile-like bytes for `space` ("RGB " or "GRAY"), which is all saving looks at.
fn profile(space: &[u8; 4]) -> Vec<u8> {
    let mut icc: Vec<u8> = (0..=255u8).cycle().take(560).collect();
    icc[16..20].copy_from_slice(space);
    icc
}

fn icc() -> Vec<u8> {
    profile(b"RGB ")
}

fn pixels(w: u32, h: u32) -> Rgb16 {
    Rgb16::from_fn(w, h, |x, y| {
        image::Rgb([(x * 977 + y) as u16, (y * 499) as u16, (x ^ y) as u16 * 13])
    })
}

/// A Photoshop image resource with no name.
fn resource(id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = b"8BIM".to_vec();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn put(
    dir: &mut tiff::encoder::DirectoryEncoder<
        '_,
        impl std::io::Write + std::io::Seek,
        tiff::encoder::TiffKindStandard,
    >,
    code: u16,
    ty: Type,
    bytes: &[u8],
) {
    let entry = dir.write_entry_bytes(ty, bytes).unwrap();
    dir.extend_from(&Directory::from_iter([(
        Tag::from_u16_exhaustive(code),
        entry,
    )]));
}

/// A 16-bit TIFF with everything: ICC, XMP, IPTC, Photoshop resources (with a thumbnail),
/// orientation 6, 300 dpi, and EXIF and GPS directories. `exif_extra` adds tags to EXIF's.
fn rich_tiff(path: &Path, image: &Rgb16, exif_extra: &[(u16, Type, Vec<u8>)]) {
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
    let mut exif = encoder.extra_directory().unwrap();
    exif.write_tag(Tag::Unknown(36867), DATE).unwrap(); // DateTimeOriginal
    exif.write_tag(Tag::Unknown(34855), 400u16).unwrap(); // ISO
    for (code, ty, bytes) in exif_extra {
        put(&mut exif, *code, *ty, bytes);
    }
    let exif = exif.finish_with_offsets().unwrap().offset;
    let mut gps = encoder.extra_directory().unwrap();
    gps.write_tag(Tag::Unknown(1), "N").unwrap(); // GPSLatitudeRef
    put(&mut gps, 0, Type::BYTE, &[2, 3, 0, 0]); // GPSVersionID
    let gps = gps.finish_with_offsets().unwrap().offset;

    let mut tiff = encoder
        .new_image::<colortype::RGB16>(image.width(), image.height())
        .unwrap();
    let dir = tiff.encoder();
    put(dir, 34675, Type::UNDEFINED, &icc());
    put(dir, 700, Type::BYTE, XMP);
    put(dir, 33723, Type::LONG, IPTC);
    let photoshop = [
        resource(0x040C, THUMBNAIL),
        resource(0x0404, IPTC),
        resource(0x03ED, RESOLUTION_INFO),
    ]
    .concat();
    put(dir, 34377, Type::BYTE, &photoshop);
    dir.write_tag(Tag::Orientation, 6u16).unwrap();
    dir.write_tag(Tag::Software, "Capture One").unwrap();
    dir.write_tag(Tag::Unknown(EXIF_DIRECTORY), exif).unwrap();
    dir.write_tag(Tag::Unknown(GPS_DIRECTORY), gps).unwrap();
    tiff.resolution(ResolutionUnit::Inch, Rational { n: 300, d: 1 });
    tiff.write_data(image.as_raw()).unwrap();
}

fn child(tags: &TagDirectory, code: u16) -> &TagDirectory {
    &tags
        .children
        .iter()
        .find(|(c, _)| *c == code)
        .unwrap_or_else(|| panic!("no directory {code}"))
        .1
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Everything [`rich_tiff`] wrote is there, but its thumbnail (and the file holds none, if
/// Gumka `saved` it).
fn check_rich(path: &Path, saved: bool) {
    let what = path.file_name().unwrap().to_string_lossy();
    let p = Picture::open(path).unwrap_or_else(|e| panic!("{what}: {e}"));
    let meta = p.meta();
    assert_eq!(p.icc_profile(), Some(&icc()[..]), "{what}: ICC");
    assert_eq!(p.orientation(), 6, "{what}: orientation");
    assert_eq!(meta.xmp.as_deref(), Some(XMP), "{what}: XMP");
    assert_eq!(meta.iptc.as_deref(), Some(IPTC), "{what}: IPTC");
    let tags = meta.exif.as_ref().expect("EXIF").to_tags().unwrap();
    let date = child(&tags, EXIF_DIRECTORY)
        .get(36867)
        .expect("DateTimeOriginal");
    assert_eq!(date.bytes, format!("{DATE}\0").as_bytes(), "{what}: EXIF");
    let gps = child(&tags, GPS_DIRECTORY);
    assert_eq!(gps.get(1).unwrap().bytes, b"N\0", "{what}: GPS");
    assert_eq!(
        gps.get(0).unwrap().bytes,
        [2, 3, 0, 0],
        "{what}: GPS version"
    );
    let density = meta.density().expect("resolution");
    assert_eq!(density.unit, DensityUnit::Inch, "{what}");
    assert!((density.x - 300.0).abs() < 0.01, "{what}: {density:?}");
    let photoshop = meta.photoshop.as_deref().expect("Photoshop resources");
    assert!(
        contains(photoshop, RESOLUTION_INFO),
        "{what}: Photoshop resources"
    );
    assert!(
        !contains(photoshop, THUMBNAIL),
        "{what}: Photoshop thumbnail"
    );
    if saved {
        let file = fs::read(path).unwrap();
        assert!(!contains(&file, THUMBNAIL), "{what}: thumbnail");
    }
}

#[test]
fn metadata_survives_every_pair_of_formats() {
    let dir = test_dir("matrix");
    let image = pixels(37, 23);
    let tif = dir.join("source.tif");
    rich_tiff(&tif, &image, &[]);
    // The PNG and JPEG sources are the TIFF as Gumka saves it.
    let source = Picture::open(&tif).unwrap();
    let (png, jpg) = (dir.join("source.png"), dir.join("source.jpg"));
    source.save(&png).unwrap();
    source.save(&jpg).unwrap();
    for src in [&tif, &png, &jpg] {
        check_rich(src, src != &tif);
        let picture = Picture::open(src).unwrap();
        let stem = src.extension().unwrap().to_string_lossy().into_owned();
        for ext in ["tif", "png", "jpg"] {
            let out = dir.join(format!("{stem}-to.{ext}"));
            picture.save(&out).unwrap();
            check_rich(&out, true);
            if stem != "jpg" && ext != "jpg" {
                assert_eq!(Picture::open(&out).unwrap().pixels, image, "{stem} → {ext}");
            }
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFF, marker];
    s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    s.extend_from_slice(payload);
    s
}

/// A JPEG with `segments` right after its start.
fn jpeg_with(segments: &[Vec<u8>], gray: bool) -> Vec<u8> {
    let mut encoded = Vec::new();
    let image = if gray {
        image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(16, 8, |x, y| {
            image::Luma([(x * 16 + y) as u8])
        }))
    } else {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(16, 8, |x, y| {
            image::Rgb([(x * 16) as u8, (y * 32) as u8, 128])
        }))
    };
    image
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new(&mut encoded))
        .unwrap();
    [&encoded[..2], &segments.concat(), &encoded[2..]].concat()
}

/// A little-endian EXIF blob: orientation 6 in IFD0, and IFD1 pointing at [`THUMBNAIL`].
fn exif_with_thumbnail() -> Vec<u8> {
    let mut b = b"II*\0\x08\0\0\0".to_vec();
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
    b.extend_from_slice(&26u32.to_le_bytes());
    let len = THUMBNAIL.len() as u8;
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&[0x01, 0x02, 4, 0, 1, 0, 0, 0, 56, 0, 0, 0]);
    b.extend_from_slice(&[0x02, 0x02, 4, 0, 1, 0, 0, 0, len, 0, 0, 0]);
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(THUMBNAIL);
    b
}

#[test]
fn thumbnails_are_dropped_on_every_save() {
    let dir = test_dir("thumbnails");
    let source = dir.join("source.jpg");
    let photoshop = [
        resource(0x0409, THUMBNAIL),
        resource(0x0404, IPTC),
        resource(0x03ED, RESOLUTION_INFO),
    ]
    .concat();
    let jpeg = jpeg_with(
        &[
            segment(
                0xE1,
                &[b"Exif\0\0".as_slice(), &exif_with_thumbnail()].concat(),
            ),
            segment(0xED, &[b"Photoshop 3.0\0".as_slice(), &photoshop].concat()),
        ],
        false,
    );
    fs::write(&source, &jpeg).unwrap();
    let picture = Picture::open(&source).unwrap();
    assert_eq!(picture.orientation(), 6);
    for ext in ["jpg", "png", "tif"] {
        let out = dir.join(format!("out.{ext}"));
        picture.save(&out).unwrap();
        assert!(!contains(&fs::read(&out).unwrap(), THUMBNAIL), "{ext}");
        let back = Picture::open(&out).unwrap();
        assert_eq!(back.orientation(), 6, "{ext}");
        assert_eq!(back.meta().iptc.as_deref(), Some(IPTC), "{ext}");
        let photoshop = back.meta().photoshop.as_deref().unwrap();
        assert!(!contains(photoshop, THUMBNAIL), "{ext}");
        let blob = back.meta().exif.as_ref().unwrap().to_blob().unwrap();
        assert!(!contains(&blob, THUMBNAIL), "{ext}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// A gray PNG with a profile, through the png crate.
/// A gray PNG with `icc`, through the png crate.
fn gray_png(path: &Path, depth: png::BitDepth, alpha: bool, icc: &[u8]) -> Vec<u16> {
    let (w, h) = (9u32, 5u32);
    let channels = if alpha { 2 } else { 1 };
    let values: Vec<u16> = (0..w * h * channels)
        .map(|i| (i * 1031 % 65536) as u16)
        .map(|v| {
            if depth == png::BitDepth::Eight {
                v / 257 * 257
            } else {
                v
            }
        })
        .collect();
    let mut info = png::Info::with_size(w, h);
    info.bit_depth = depth;
    info.color_type = if alpha {
        png::ColorType::GrayscaleAlpha
    } else {
        png::ColorType::Grayscale
    };
    info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
    let mut writer = png::Encoder::with_info(BufWriter::new(File::create(path).unwrap()), info)
        .unwrap()
        .write_header()
        .unwrap();
    let data: Vec<u8> = if depth == png::BitDepth::Sixteen {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    } else {
        values.iter().map(|&v| (v / 257) as u8).collect()
    };
    writer.write_image_data(&data).unwrap();
    writer.finish().unwrap();
    values
}

#[test]
fn grayscale_stays_grayscale() {
    let dir = test_dir("gray");
    for (depth, alpha) in [
        (png::BitDepth::Sixteen, false),
        (png::BitDepth::Eight, true),
    ] {
        let source = dir.join("source.png");
        gray_png(&source, depth, alpha, &profile(b"GRAY"));
        let picture = Picture::open(&source).unwrap();
        assert!(picture.is_gray());
        let bits = if depth == png::BitDepth::Sixteen {
            16
        } else {
            8
        };

        let png_out = dir.join("out.png");
        picture.save(&png_out).unwrap();
        let reader = png::Decoder::new(BufReader::new(File::open(&png_out).unwrap()))
            .read_info()
            .unwrap();
        let expected = if alpha {
            png::ColorType::GrayscaleAlpha
        } else {
            png::ColorType::Grayscale
        };
        assert_eq!(
            (reader.info().color_type, reader.info().bit_depth),
            (expected, depth)
        );

        let tif_out = dir.join("out.tif");
        picture.save(&tif_out).unwrap();
        let mut decoder = Decoder::new(BufReader::new(File::open(&tif_out).unwrap())).unwrap();
        let samples: u16 = decoder.get_tag_unsigned(Tag::SamplesPerPixel).unwrap();
        assert_eq!(samples, 1 + u16::from(alpha), "TIFF samples");
        assert_eq!(
            decoder
                .get_tag_unsigned::<u16>(Tag::PhotometricInterpretation)
                .unwrap(),
            1
        );

        let jpg_out = dir.join("out.jpg");
        picture.save(&jpg_out).unwrap();
        assert_eq!(image::open(&jpg_out).unwrap().color(), image::ColorType::L8);

        for out in [&png_out, &tif_out, &jpg_out] {
            let back = Picture::open(out).unwrap();
            assert!(back.is_gray(), "{}", out.display());
            let gray = profile(b"GRAY");
            assert_eq!(back.icc_profile(), Some(&gray[..]), "{}", out.display());
            if out != &jpg_out {
                assert_eq!(back.pixels, picture.pixels, "{}", out.display());
                assert_eq!(back.alpha(), picture.alpha(), "{}", out.display());
                assert_eq!(back.bit_depth(), bits, "{}", out.display());
            }
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// A big-endian 2 x 1 RGB TIFF with orientation 6 and IPTC declared as LONG, as Photoshop
/// writes it: a byte stream that must not be swapped four bytes at a time.
fn big_endian_tiff() -> Vec<u8> {
    let entries: [(u16, u16, u32, u32); 11] = [
        (256, 3, 1, 2),                           // width
        (257, 3, 1, 1),                           // length
        (258, 3, 3, 146),                         // bits per sample, at 146
        (259, 3, 1, 1),                           // no compression
        (262, 3, 1, 2),                           // RGB
        (273, 4, 1, 152),                         // strip at 152
        (274, 3, 1, 6),                           // orientation
        (277, 3, 1, 3),                           // samples per pixel
        (278, 3, 1, 1),                           // rows per strip
        (279, 4, 1, 6),                           // strip bytes
        (33723, 4, (IPTC.len() / 4) as u32, 158), // IPTC, at 158
    ];
    let mut b = b"MM\0*\0\0\0\x08".to_vec();
    b.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for (tag, ty, count, value) in entries {
        b.extend_from_slice(&tag.to_be_bytes());
        b.extend_from_slice(&ty.to_be_bytes());
        b.extend_from_slice(&count.to_be_bytes());
        if ty == 3 && count == 1 {
            b.extend_from_slice(&(value as u16).to_be_bytes());
            b.extend_from_slice(&[0, 0]);
        } else {
            b.extend_from_slice(&value.to_be_bytes());
        }
    }
    b.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(b.len(), 146);
    b.extend_from_slice(&[0, 8, 0, 8, 0, 8]);
    b.extend_from_slice(&[10, 20, 30, 40, 50, 60]);
    b.extend_from_slice(IPTC);
    b
}

#[test]
fn big_endian_iptc_keeps_its_bytes() {
    let dir = test_dir("big-endian");
    let source = dir.join("source.tif");
    fs::write(&source, big_endian_tiff()).unwrap();
    let picture = Picture::open(&source).unwrap();
    assert_eq!(picture.orientation(), 6);
    assert_eq!(
        picture.pixels.get_pixel(1, 0).0,
        [40 * 257, 50 * 257, 60 * 257]
    );
    assert_eq!(picture.meta().iptc.as_deref(), Some(IPTC));
    for ext in ["tif", "png", "jpg"] {
        let out = dir.join(format!("out.{ext}"));
        picture.save(&out).unwrap();
        let back = Picture::open(&out).unwrap();
        assert_eq!(back.meta().iptc.as_deref(), Some(IPTC), "{ext}");
        assert_eq!(back.orientation(), 6, "{ext}");
    }
    // In the TIFF written, the bytes are the same, still declared as LONG.
    let mut decoder =
        Decoder::new(BufReader::new(File::open(dir.join("out.tif")).unwrap())).unwrap();
    let mut value = ValueBuffer::empty(Type::BYTE);
    let entry = decoder
        .image_ifd()
        .find_tag_buf(Tag::Unknown(33723), &mut value)
        .unwrap()
        .unwrap();
    assert_eq!((entry.field_type(), value.as_bytes()), (Type::LONG, IPTC));
    fs::remove_dir_all(&dir).unwrap();
}

/// Offset of `tag`'s value field in the little-endian directory at `dir`.
fn entry_value(file: &[u8], dir: usize, tag: u16) -> usize {
    let count = u16::from_le_bytes([file[dir], file[dir + 1]]) as usize;
    (0..count)
        .map(|i| dir + 2 + 12 * i)
        .find(|&at| u16::from_le_bytes([file[at], file[at + 1]]) == tag)
        .expect("tag")
        + 8
}

fn u32_at(file: &[u8], at: usize) -> usize {
    u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize
}

#[test]
fn looping_and_broken_directory_pointers_are_dropped() {
    let dir = test_dir("cycles");
    let path = dir.join("cycle.tif");
    // EXIF with an interoperability pointer, then made to point back at EXIF itself; GPS
    // made to point past the end of the file.
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap())).unwrap();
    let mut exif = encoder.extra_directory().unwrap();
    exif.write_tag(Tag::Unknown(34855), 400u16).unwrap();
    exif.write_tag(Tag::Unknown(40965), 8u32).unwrap();
    let exif = exif.finish_with_offsets().unwrap().offset;
    let mut tiff = encoder.new_image::<colortype::RGB8>(2, 2).unwrap();
    tiff.encoder()
        .write_tag(Tag::Unknown(EXIF_DIRECTORY), exif)
        .unwrap();
    tiff.encoder()
        .write_tag(Tag::Unknown(GPS_DIRECTORY), 8u32)
        .unwrap();
    tiff.write_data(&[0u8; 12]).unwrap();
    drop(encoder);
    let mut file = fs::read(&path).unwrap();
    let ifd0 = u32_at(&file, 4);
    let exif_at = u32_at(&file, entry_value(&file, ifd0, EXIF_DIRECTORY));
    let interop = entry_value(&file, exif_at, 40965);
    file[interop..interop + 4].copy_from_slice(&(exif_at as u32).to_le_bytes());
    let gps = entry_value(&file, ifd0, GPS_DIRECTORY);
    file[gps..gps + 4].copy_from_slice(&1_000_000u32.to_le_bytes());
    fs::write(&path, &file).unwrap();

    let picture = Picture::open(&path).unwrap();
    let tags = picture.meta().exif.as_ref().unwrap().to_tags().unwrap();
    assert_eq!(tags.children.len(), 1, "GPS dropped");
    let exif = child(&tags, EXIF_DIRECTORY);
    assert!(exif.get(34855).is_some());
    assert!(exif.children.is_empty(), "the loop isn't followed");
    for ext in ["tif", "jpg"] {
        picture.save(&dir.join(format!("out.{ext}"))).unwrap();
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exif_too_large_for_jpeg_loses_its_maker_note_first() {
    let dir = test_dir("large-exif");
    let image = pixels(8, 8);
    let big = vec![7u8; 70_000];

    let with_note = dir.join("note.tif");
    rich_tiff(&with_note, &image, &[(37500, Type::UNDEFINED, big.clone())]);
    let picture = Picture::open(&with_note).unwrap();
    let jpg = dir.join("note.jpg");
    picture.save(&jpg).unwrap();
    image::open(&jpg).expect("a valid JPEG");
    let tags = Picture::open(&jpg)
        .unwrap()
        .meta()
        .exif
        .as_ref()
        .unwrap()
        .to_tags()
        .unwrap();
    let exif = child(&tags, EXIF_DIRECTORY);
    assert!(exif.get(34855).is_some() && exif.get(37500).is_none());
    // PNG holds it all.
    let png = dir.join("note.png");
    picture.save(&png).unwrap();
    let tags = Picture::open(&png)
        .unwrap()
        .meta()
        .exif
        .as_ref()
        .unwrap()
        .to_tags()
        .unwrap();
    assert_eq!(child(&tags, EXIF_DIRECTORY).get(37500).unwrap().bytes, big);

    let with_comment = dir.join("comment.tif");
    rich_tiff(&with_comment, &image, &[(37510, Type::UNDEFINED, big)]);
    let jpg = dir.join("comment.jpg");
    Picture::open(&with_comment).unwrap().save(&jpg).unwrap();
    image::open(&jpg).expect("a valid JPEG");
    let back = Picture::open(&jpg).unwrap();
    assert!(back.meta().exif.is_none(), "too large: left out");
    assert_eq!(back.meta().xmp.as_deref(), Some(XMP), "the rest is there");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_tiff_with_an_extra_channel_that_isnt_alpha_is_refused() {
    let dir = test_dir("extra-samples");
    let path = dir.join("extra.tif");
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap())).unwrap();
    let mut tiff = encoder.new_image::<colortype::RGBA8>(2, 2).unwrap();
    tiff.encoder().write_tag(Tag::ExtraSamples, 0u16).unwrap();
    tiff.write_data(&[0u8; 16]).unwrap();
    drop(encoder);
    let error = Picture::open(&path).err().expect("refused").to_string();
    assert!(error.contains("extra channel"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn png_text_and_jpeg_comments_are_kept() {
    let dir = test_dir("text");
    let source = dir.join("text.png");
    let mut info = png::Info::with_size(2, 2);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.uncompressed_latin1_text
        .push(png::text_metadata::TEXtChunk::new("Comment", "hello"));
    info.utf8_text
        .push(png::text_metadata::ITXtChunk::new("Description", "żółw"));
    let mut writer = png::Encoder::with_info(BufWriter::new(File::create(&source).unwrap()), info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&[9; 12]).unwrap();
    writer.finish().unwrap();
    let out = dir.join("out.png");
    Picture::open(&source).unwrap().save(&out).unwrap();
    let mut reader = png::Decoder::new(BufReader::new(File::open(&out).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut buf).unwrap();
    reader.finish().unwrap();
    let info = reader.info();
    assert!(
        info.uncompressed_latin1_text
            .iter()
            .any(|c| c.keyword == "Comment" && c.text == "hello")
    );
    assert!(
        info.utf8_text
            .iter()
            .any(|c| c.keyword == "Description" && c.get_text().unwrap() == "żółw")
    );

    let source = dir.join("comment.jpg");
    fs::write(&source, jpeg_with(&[segment(0xFE, b"a comment")], false)).unwrap();
    let out = dir.join("out.jpg");
    Picture::open(&source).unwrap().save(&out).unwrap();
    assert!(contains(
        &fs::read(&out).unwrap(),
        &segment(0xFE, b"a comment")
    ));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_gray_jpeg_is_saved_gray() {
    let dir = test_dir("gray-jpeg");
    let source = dir.join("gray.jpg");
    fs::write(&source, jpeg_with(&[], true)).unwrap();
    let picture = Picture::open(&source).unwrap();
    assert!(picture.is_gray());
    for ext in ["jpg", "png"] {
        let out = dir.join(format!("out.{ext}"));
        picture.save(&out).unwrap();
        assert!(!image::open(&out).unwrap().color().has_color(), "{ext}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resolution_crosses_formats() {
    let dir = test_dir("density");
    // A PNG at 300 dpi (11811 pixels per meter) and no EXIF.
    let source = dir.join("dpi.png");
    let mut info = png::Info::with_size(2, 2);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.pixel_dims = Some(png::PixelDimensions {
        xppu: 11811,
        yppu: 11811,
        unit: png::Unit::Meter,
    });
    let mut writer = png::Encoder::with_info(BufWriter::new(File::create(&source).unwrap()), info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&[9; 12]).unwrap();
    writer.finish().unwrap();
    let picture = Picture::open(&source).unwrap();
    for ext in ["tif", "jpg", "png"] {
        let out = dir.join(format!("out.{ext}"));
        picture.save(&out).unwrap();
        let density: Density = Picture::open(&out).unwrap().meta().density().unwrap();
        let expected = Density {
            x: 300.0,
            y: 300.0,
            unit: DensityUnit::Inch,
        };
        assert_eq!(density, expected, "{ext}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn saving_over_a_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = test_dir("permissions");
    let path = dir.join("p.png");
    let picture = Picture::from_pixels(pixels(3, 2), 16);
    picture.save(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    picture.save(&path).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    assert!(
        fs::read_dir(&dir).unwrap().count() == 1,
        "no temporary file left"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// A TIFF like [`rich_tiff`]'s, with `xmp` and Photoshop resources `photoshop`.
fn tiff_with(path: &Path, xmp: &[u8], photoshop: &[u8], iptc: Option<&[u8]>) {
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
    let mut tiff = encoder.new_image::<colortype::RGB8>(4, 2).unwrap();
    let dir = tiff.encoder();
    put(dir, 34675, Type::UNDEFINED, &icc());
    put(dir, 700, Type::BYTE, xmp);
    put(dir, 34377, Type::BYTE, photoshop);
    if let Some(iptc) = iptc {
        put(dir, 33723, Type::UNDEFINED, iptc);
    }
    tiff.write_data(&[50u8; 24]).unwrap();
}

#[test]
fn large_xmp_and_photoshop_blocks_fit_into_jpeg() {
    let dir = test_dir("large-blocks");
    // XMP with a lot of padding and a long photoshop:DocumentAncestors, as Photoshop writes.
    let ancestors: String = (0..3000)
        .map(|i| format!("<rdf:li>xmp.did:{i:032}</rdf:li>"))
        .collect();
    let xmp = format!(
        "<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'>\
         <rdf:RDF><photoshop:DocumentAncestors><rdf:Bag>{ancestors}</rdf:Bag>\
         </photoshop:DocumentAncestors><dc:title>Gumka title</dc:title></rdf:RDF></x:xmpmeta>\
         {}<?xpacket end='w'?>",
        " ".repeat(4000)
    );
    // A Photoshop block too large for one segment, of a small and a large resource.
    let photoshop = [
        resource(0x03ED, RESOLUTION_INFO),
        resource(0x0FA0, &vec![5u8; 100_000]),
    ]
    .concat();
    let source = dir.join("large.tif");
    tiff_with(&source, xmp.as_bytes(), &photoshop, None);
    let jpg = dir.join("large.jpg");
    Picture::open(&source).unwrap().save(&jpg).unwrap();
    image::open(&jpg).expect("a valid JPEG");
    let back = Picture::open(&jpg).unwrap();
    let text = String::from_utf8(back.meta().xmp.clone().unwrap()).unwrap();
    assert!(text.contains("Gumka title") && !text.contains("DocumentAncestors"));
    assert_eq!(
        back.meta().photoshop.as_deref(),
        Some(&photoshop[..]),
        "joined again"
    );

    // XMP that can't be made to fit is left out; the rest is saved.
    let xmp = format!(
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'>{}</x:xmpmeta>",
        "y".repeat(70_000)
    );
    tiff_with(&source, xmp.as_bytes(), &photoshop, None);
    Picture::open(&source).unwrap().save(&jpg).unwrap();
    let back = Picture::open(&jpg).unwrap();
    assert!(back.meta().xmp.is_none());
    assert!(back.meta().photoshop.is_some());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn iptc_of_any_length_goes_into_a_tiff() {
    let dir = test_dir("odd-iptc");
    // One record of 6 bytes: 11 bytes in all.
    let odd = [&IPTC[..4], &[6], &IPTC[5..11]].concat();
    assert_eq!(odd.len(), 11);
    let source = dir.join("odd.jpg");
    let block = resource(0x0404, &odd);
    fs::write(
        &source,
        jpeg_with(
            &[segment(
                0xED,
                &[b"Photoshop 3.0\0".as_slice(), &block].concat(),
            )],
            false,
        ),
    )
    .unwrap();
    let tif = dir.join("odd.tif");
    Picture::open(&source).unwrap().save(&tif).unwrap();
    assert_eq!(
        Picture::open(&tif).unwrap().meta().iptc.as_deref(),
        Some(&odd[..])
    );
    // Declared LONG, padded with a zero.
    let mut decoder = Decoder::new(BufReader::new(File::open(&tif).unwrap())).unwrap();
    let mut value = ValueBuffer::empty(Type::BYTE);
    let entry = decoder
        .image_ifd()
        .find_tag_buf(Tag::Unknown(33723), &mut value)
        .unwrap()
        .unwrap();
    assert_eq!(entry.field_type(), Type::LONG);
    assert_eq!(value.as_bytes(), [&odd[..], &[0]].concat());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn profiles_must_fit_the_pixels() {
    let dir = test_dir("profile-space");
    // A gray PNG with an RGB profile is saved as RGB, with its profile.
    let source = dir.join("gray-rgb.png");
    gray_png(&source, png::BitDepth::Eight, false, &profile(b"RGB "));
    let picture = Picture::open(&source).unwrap();
    assert!(!picture.is_gray(), "an RGB profile makes it RGB");
    let out = dir.join("out.png");
    picture.save(&out).unwrap();
    assert!(image::open(&out).unwrap().color().has_color());
    assert_eq!(
        Picture::open(&out).unwrap().icc_profile(),
        Some(&profile(b"RGB ")[..])
    );

    // A profile for other pixels (here CMYK on gray) is dropped.
    let source = dir.join("gray-cmyk.png");
    gray_png(&source, png::BitDepth::Eight, false, &profile(b"CMYK"));
    let picture = Picture::open(&source).unwrap();
    assert!(picture.is_gray() && picture.icc_profile().is_none());
    // So is one too short to say what it is for (review-B #9).
    let source = dir.join("gray-short.png");
    gray_png(&source, png::BitDepth::Eight, false, b"0123456789");
    assert!(Picture::open(&source).unwrap().icc_profile().is_none());

    // CMYK JPEGs aren't opened (only the frame header is looked at).
    let mut jpeg = jpeg_with(&[], false);
    let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    jpeg[sof + 9] = 4;
    let path = dir.join("cmyk.jpg");
    fs::write(&path, jpeg).unwrap();
    let error = Picture::open(&path).err().expect("refused").to_string();
    assert!(error.contains("CMYK"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn png_metadata_quirks_are_read() {
    let dir = test_dir("png-quirks");
    let path = dir.join("quirks.png");
    let block = [resource(0x0404, IPTC), resource(0x03ED, RESOLUTION_INFO)].concat();
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(&exif_with_thumbnail());
    let mut info = png::Info::with_size(2, 2);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.exif_metadata = Some(std::borrow::Cow::Owned(exif));
    info.compressed_latin1_text
        .push(png::text_metadata::ZTXtChunk::new(
            "Raw profile type iptc",
            crate::meta::encode_raw_profile("iptc", &block),
        ));
    let mut writer = png::Encoder::with_info(BufWriter::new(File::create(&path).unwrap()), info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&[9; 12]).unwrap();
    writer.finish().unwrap();
    let picture = Picture::open(&path).unwrap();
    assert_eq!(picture.orientation(), 6, "EXIF after a JPEG header");
    assert_eq!(
        picture.meta().iptc.as_deref(),
        Some(IPTC),
        "a Photoshop block as IPTC"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bigtiff_only_values_are_left_out_of_a_tiff() {
    let dir = test_dir("bigtiff");
    let path = dir.join("big.tif");
    let mut encoder = TiffEncoder::new_big(BufWriter::new(File::create(&path).unwrap())).unwrap();
    let mut tiff = encoder.new_image::<colortype::RGB8>(2, 1).unwrap();
    tiff.encoder()
        .write_tag(Tag::Unknown(65000), 1u64 << 40)
        .unwrap();
    tiff.encoder().write_tag(Tag::Software, "big").unwrap();
    tiff.write_data(&[1, 2, 3, 4, 5, 6]).unwrap();
    drop(encoder);
    let out = dir.join("classic.tif");
    Picture::open(&path).unwrap().save(&out).unwrap();
    let mut decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
    assert!(decoder.find_tag(Tag::Unknown(65000)).unwrap().is_none());
    assert_eq!(decoder.get_tag_ascii_string(Tag::Software).unwrap(), "big");
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn saving_through_a_symbolic_link_replaces_the_file_it_points_to() {
    let dir = test_dir("symlink");
    let (file, link) = (dir.join("real.png"), dir.join("link.png"));
    Picture::from_pixels(pixels(3, 2), 16).save(&file).unwrap();
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let changed = Picture::from_pixels(pixels(5, 4), 16);
    changed.save(&link).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(Picture::open(&file).unwrap().pixels, changed.pixels);
    fs::remove_dir_all(&dir).unwrap();
}

// ---- Added by the tester: inputs whose PNG header fields must not reach the output, and
// metadata blocks broken in ways that must not stop an open or a save. ----

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = data.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        out.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// An Adam7-interlaced 8-bit RGB PNG of `rgb` (`w` by `h`), every scanline unfiltered.
fn interlaced_png(path: &Path, w: u32, h: u32, rgb: &[u8]) {
    const PASSES: [(u32, u32, usize, usize); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let mut raw = Vec::new();
    for (x0, y0, dx, dy) in PASSES {
        if x0 >= w || y0 >= h {
            continue;
        }
        for y in (y0..h).step_by(dy) {
            raw.push(0); // filter type None
            for x in (x0..w).step_by(dx) {
                let i = ((y * w + x) * 3) as usize;
                raw.extend_from_slice(&rgb[i..i + 3]);
            }
        }
    }
    let mut info = png::Info::with_size(w, h);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.interlaced = true;
    let file = BufWriter::new(File::create(path).unwrap());
    let mut writer = png::Encoder::with_info(file, info)
        .unwrap()
        .write_header()
        .unwrap();
    writer
        .write_chunk(png::chunk::IDAT, &zlib_stored(&raw))
        .unwrap();
    drop(writer); // writes IEND
}

/// The PNG's header and the chunks a save must not copy from the source.
fn png_layout(path: &Path) -> (png::ColorType, bool, bool, bool, bool) {
    let reader = png::Decoder::new(BufReader::new(File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let info = reader.info();
    (
        info.color_type,
        info.interlaced,
        info.trns.is_some(),
        info.palette.is_some(),
        info.animation_control.is_some(),
    )
}

#[test]
fn an_interlaced_png_is_read_and_saved_plainly() {
    let dir = test_dir("interlaced");
    let (w, h) = (13, 11); // odd sizes leave some Adam7 passes short or empty
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 37 % 251) as u8).collect();
    let source = dir.join("adam7.png");
    interlaced_png(&source, w, h, &rgb);
    let expected = Rgb16::from_fn(w, h, |x, y| {
        let i = ((y * w + x) * 3) as usize;
        image::Rgb([0, 1, 2].map(|c| u16::from(rgb[i + c]) * 257))
    });
    let picture = Picture::open(&source).unwrap();
    assert_eq!(picture.pixels, expected, "decoded from Adam7");
    let out = dir.join("out.png");
    picture.save(&out).unwrap();
    assert_eq!(
        png_layout(&out),
        (png::ColorType::Rgb, false, false, false, false)
    );
    assert_eq!(Picture::open(&out).unwrap().pixels, expected);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn palette_transparency_and_animation_chunks_are_not_copied() {
    let dir = test_dir("png-header");
    let (w, h) = (6u32, 4u32);
    let save = |path: &Path,
                setup: &dyn Fn(&mut png::Encoder<'_, BufWriter<File>>),
                frames: &[Vec<u8>]| {
        let mut encoder = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), w, h);
        setup(&mut encoder);
        let mut writer = encoder.write_header().unwrap();
        for frame in frames {
            writer.write_image_data(frame).unwrap();
        }
        writer.finish().unwrap();
    };
    // Indexed, 4 colors, the first one transparent.
    let indexed = dir.join("indexed.png");
    let indices: Vec<u8> = (0..w * h).map(|i| (i % 4) as u8).collect();
    save(
        &indexed,
        &|e| {
            e.set_color(png::ColorType::Indexed);
            e.set_depth(png::BitDepth::Eight);
            e.set_palette(vec![10, 20, 30, 200, 0, 0, 0, 200, 0, 0, 0, 200]);
            e.set_trns(vec![0]);
        },
        &[indices],
    );
    // RGB with a transparent color key.
    let keyed = dir.join("keyed.png");
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 11 % 256) as u8).collect();
    let key = rgb[..3].to_vec();
    save(
        &keyed,
        &|e| {
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.set_trns(key.iter().flat_map(|&v| [0, v]).collect::<Vec<u8>>());
        },
        std::slice::from_ref(&rgb),
    );
    // Animated: two frames.
    let animated = dir.join("animated.png");
    save(
        &animated,
        &|e| {
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.set_animated(2, 0).unwrap();
        },
        &[rgb.clone(), rgb.iter().map(|v| 255 - v).collect()],
    );

    for source in [&indexed, &keyed, &animated] {
        let what = source.file_name().unwrap().to_string_lossy().into_owned();
        let picture = Picture::open(source).unwrap_or_else(|e| panic!("{what}: {e}"));
        let out = dir.join(format!("out-{what}"));
        picture.save(&out).unwrap_or_else(|e| panic!("{what}: {e}"));
        let (color, interlaced, trns, palette, animation) = png_layout(&out);
        assert!(
            !interlaced && !trns && !palette && !animation,
            "{what}: header chunks copied"
        );
        assert!(
            matches!(color, png::ColorType::Rgb | png::ColorType::Rgba),
            "{what}: {color:?}"
        );
        // The saved pixels look like the source's (first frame), transparency included.
        let before = image::open(source).unwrap().to_rgba16();
        let after = image::open(&out).unwrap().to_rgba16();
        assert_eq!(before, after, "{what}: pixels");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn white_is_zero_gray_keeps_its_appearance() {
    let dir = test_dir("white-is-zero");
    let (w, h) = (5u32, 3u32);
    let stored: Vec<u8> = (0..w * h).map(|i| (i * 17) as u8).collect();
    let source = dir.join("wiz.tif");
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&source).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::Gray8>(w, h).unwrap();
        image
            .encoder()
            .write_tag(Tag::PhotometricInterpretation, 0u16)
            .unwrap();
        image.write_data(&stored).unwrap();
    }
    let picture = Picture::open(&source).unwrap();
    assert!(picture.is_gray());
    let shown = |i: usize| u16::from(255 - stored[i]) * 257;
    assert_eq!(
        picture.pixels.get_pixel(1, 0).0,
        [shown(1); 3],
        "white is zero"
    );
    for ext in ["tif", "png", "jpg"] {
        let out = dir.join(format!("out.{ext}"));
        picture.save(&out).unwrap();
        let back = Picture::open(&out).unwrap();
        assert!(back.is_gray(), "{ext}");
        if ext != "jpg" {
            assert_eq!(back.pixels, picture.pixels, "{ext}: same appearance");
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn broken_metadata_never_stops_an_open_or_a_save() {
    let dir = test_dir("broken-meta");
    // EXIF whose IFD0 lies past its end; a Photoshop block whose second resource is cut off,
    // followed by a segment of garbage.
    let mut bad_exif = b"Exif\0\0II*\0".to_vec();
    bad_exif.extend_from_slice(&4000u32.to_le_bytes());
    bad_exif.extend_from_slice(&[0; 20]);
    let mut cut = b"Photoshop 3.0\0".to_vec();
    cut.extend_from_slice(&resource(0x03ED, RESOLUTION_INFO));
    cut.extend_from_slice(b"8BIM\x04\x04\0\0\0\x01\x00\x00");
    cut.extend_from_slice(IPTC);
    let jpeg = jpeg_with(
        &[
            segment(0xE1, &bad_exif),
            segment(0xED, &cut),
            segment(0xED, b"Photoshop 3.0\0garbage"),
        ],
        false,
    );
    let jpeg_path = dir.join("broken.jpg");
    fs::write(&jpeg_path, &jpeg).unwrap();
    // A PNG whose eXIf chunk is garbage.
    let png_path = dir.join("broken.png");
    {
        let mut encoder = png::Encoder::new(BufWriter::new(File::create(&png_path).unwrap()), 4, 3);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_chunk(
                png::chunk::ChunkType(*b"eXIf"),
                b"MM\0*\xff\xff\xff\xff\xff",
            )
            .unwrap();
        writer.write_image_data(&[7; 36]).unwrap();
        writer.finish().unwrap();
    }
    // A TIFF whose Photoshop block, IPTC and XMP are all unreadable (the XMP is UTF-16 with
    // an odd byte count).
    let tiff_path = dir.join("broken.tif");
    {
        let mut encoder =
            TiffEncoder::new(BufWriter::new(File::create(&tiff_path).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::RGB8>(4, 3).unwrap();
        let dir = image.encoder();
        put(dir, 34377, Type::BYTE, b"8BIM\x04");
        put(dir, 33723, Type::UNDEFINED, b"\x1c\x02\x78\xff\xff garbage");
        put(dir, 700, Type::BYTE, b"\xff\xfe<\0x\0:\0");
        image.write_data(&[9; 36]).unwrap();
    }
    for source in [&jpeg_path, &png_path, &tiff_path] {
        let what = source.file_name().unwrap().to_string_lossy().into_owned();
        let picture = Picture::open(source).unwrap_or_else(|e| panic!("{what}: open: {e}"));
        for ext in ["tif", "png", "jpg"] {
            let out = dir.join(format!("{what}.{ext}"));
            picture
                .save(&out)
                .unwrap_or_else(|e| panic!("{what} -> {ext}: {e}"));
            Picture::open(&out).unwrap_or_else(|e| panic!("{what} -> {ext}: reopen: {e}"));
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// A `width` × `height` TIFF whose alpha is associated, said with Matteing (32995), the
/// obsolete form of ExtraSamples.
fn matteing_tiff<C: colortype::ColorType>(path: &Path, width: u32, height: u32, data: &[C::Inner])
where
    [C::Inner]: tiff::encoder::TiffValue,
{
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
        let mut image = encoder.new_image::<C>(width, height).unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(32995), 1u16)
            .unwrap();
        image.write_data(data).unwrap();
    }
    // If the encoder also wrote ExtraSamples, hide it (tag 338 renamed to a private 65000).
    let mut bytes = fs::read(path).unwrap();
    if let Some(at) = bytes.windows(4).position(|w| w == [0x52, 0x01, 3, 0]) {
        bytes[at..at + 2].copy_from_slice(&65000u16.to_le_bytes());
        fs::write(path, &bytes).unwrap();
    }
}

/// TIFF 32995 (Matteing, the obsolete form of ExtraSamples) means associated alpha.
#[test]
fn matteing_means_associated_alpha() {
    let dir = test_dir("matteing");
    let source = dir.join("matteing.tif");
    matteing_tiff::<colortype::RGBA8>(&source, 2, 1, &[10, 20, 30, 128, 1, 2, 3, 255]);
    let out = dir.join("out.tif");
    let picture = Picture::open(&source).unwrap();
    assert!(picture.associated_alpha());
    picture.save(&out).unwrap();
    let mut decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
    let extra: Vec<u16> = decoder.get_tag_u16_vec(Tag::ExtraSamples).unwrap();
    assert_eq!(extra, [1], "associated alpha, as Matteing said");
    fs::remove_dir_all(&dir).unwrap();
}

/// PNG has straight alpha only, so an associated alpha's colors are divided by it on the way;
/// fully transparent pixels become black (review round 2: they were written premultiplied, and
/// soft edges came out dark). A TIFF keeps the colors as they are, still associated.
#[test]
fn an_associated_alpha_is_saved_straight_in_a_png() {
    let dir = test_dir("associated-png");
    let source = dir.join("associated.tif");
    let rgba = [10000, 20000, 30000, 40000, 1, 2, 3, 65535, 500, 600, 700, 0];
    matteing_tiff::<colortype::RGBA16>(&source, 3, 1, &rgba);
    let picture = Picture::open(&source).unwrap();
    assert!(picture.associated_alpha());
    let png = dir.join("straight.png");
    picture.save(&png).unwrap();
    let straight = image::open(&png).unwrap().into_rgba16();
    assert_eq!(
        straight.as_raw(),
        &[16384, 32768, 49151, 40000, 1, 2, 3, 65535, 0, 0, 0, 0]
    );
    assert!(!Picture::open(&png).unwrap().associated_alpha());

    let tif = dir.join("again.tif");
    picture.save(&tif).unwrap();
    let again = Picture::open(&tif).unwrap();
    assert!(again.associated_alpha());
    assert_eq!(again.pixels, picture.pixels, "bit-identical");
    assert_eq!(again.alpha(), picture.alpha());

    // At 8 bits: 10 under an alpha of 128 is 20 straight.
    let source = dir.join("associated-8.tif");
    matteing_tiff::<colortype::RGBA8>(&source, 2, 1, &[10, 20, 30, 128, 1, 2, 3, 255]);
    Picture::open(&source).unwrap().save(&png).unwrap();
    let straight = image::open(&png).unwrap().into_rgba8();
    assert_eq!(straight.as_raw(), &[20, 40, 60, 128, 1, 2, 3, 255]);
    fs::remove_dir_all(&dir).unwrap();
}

/// A source that gives no resolution must not become a 1-dpi TIFF: macOS (ImageIO, so
/// Preview, Pages, printing) reads XResolution 1 with ResolutionUnit None as 1 dpi, where
/// the source read as the usual 72.
#[test]
fn no_resolution_does_not_become_one_dpi() {
    let dir = test_dir("no-resolution");
    let source = dir.join("plain.png");
    Picture::from_pixels(pixels(4, 3), 8).save(&source).unwrap();
    let out = dir.join("out.tif");
    Picture::open(&source).unwrap().save(&out).unwrap();
    let mut decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
    let unit: Option<u16> = decoder.find_tag_unsigned(Tag::ResolutionUnit).unwrap();
    let x = decoder.find_tag(Tag::XResolution).unwrap();
    let one_dpi =
        unit.unwrap_or(2) != 3 && matches!(&x, Some(tiff::decoder::ifd::Value::Rational(1, 1)));
    assert!(!one_dpi, "XResolution {x:?}, ResolutionUnit {unit:?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// An EXIF block that can't be read is dropped from a JPEG saved as JPEG too, not copied
/// byte for byte (review-B-design #9).
#[test]
fn unreadable_exif_is_dropped_from_a_jpeg() {
    let dir = test_dir("bad-exif-jpeg");
    let mut bad = b"Exif\0\0II*\0".to_vec();
    bad.extend_from_slice(&4000u32.to_le_bytes());
    bad.extend_from_slice(&[0; 20]);
    let source = dir.join("bad.jpg");
    fs::write(&source, jpeg_with(&[segment(0xE1, &bad)], false)).unwrap();
    let out = dir.join("out.jpg");
    Picture::open(&source).unwrap().save(&out).unwrap();
    assert!(
        !contains(&fs::read(&out).unwrap(), b"Exif\0\0II*\0\xa0\x0f"),
        "broken EXIF copied"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// Values that claim more than their file holds are left out, quickly (review-B #1, #2): an
/// IPTC record whose extended length wraps the offset around, and a TIFF tag and an EXIF tag
/// each counting 4 billion doubles.
#[test]
fn values_claiming_more_than_their_file_are_left_out() {
    let dir = test_dir("claims");
    let wrapping = b"\x1c\x02\x78\x80\x08\xff\xff\xff\xff\xff\xff\xff\xf3";
    // DOUBLE (12), count 1: patched to 0xFFFFFFFF below.
    let huge = |file: &mut Vec<u8>| {
        let at = file
            .windows(8)
            .position(|w| w == [0xE8, 0xFD, 12, 0, 1, 0, 0, 0])
            .expect("tag 65000");
        file[at + 4..at + 8].copy_from_slice(&[0xFF; 4]);
    };
    let tif = dir.join("claims.tif");
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&tif).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::RGB8>(2, 1).unwrap();
        let dir = image.encoder();
        put(dir, 33723, Type::UNDEFINED, &[IPTC, wrapping].concat());
        dir.write_tag(Tag::Unknown(65000), 0.5f64).unwrap();
        dir.write_tag(Tag::Orientation, 6u16).unwrap();
        image.write_data(&[1, 2, 3, 4, 5, 6]).unwrap();
    }
    let mut file = fs::read(&tif).unwrap();
    huge(&mut file);
    fs::write(&tif, file).unwrap();

    // EXIF: Orientation 6, and tag 65000 pointing at offset 8.
    let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x02\0".to_vec();
    exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
    exif.extend_from_slice(&[0xE8, 0xFD, 12, 0, 1, 0, 0, 0, 8, 0, 0, 0]);
    exif.extend_from_slice(&[0; 4]);
    huge(&mut exif);
    let jpg = dir.join("claims.jpg");
    fs::write(&jpg, jpeg_with(&[segment(0xE1, &exif)], false)).unwrap();

    for source in [&tif, &jpg] {
        let what = source.file_name().unwrap().to_string_lossy().into_owned();
        let picture = Picture::open(source).unwrap();
        assert_eq!(picture.orientation(), 6, "{what}");
        if source == &tif {
            assert_eq!(
                picture.meta().iptc.as_deref(),
                Some(IPTC),
                "the records that read"
            );
        }
        for ext in ["tif", "png", "jpg"] {
            let out = dir.join(format!("{what}.{ext}"));
            picture.save(&out).unwrap();
            assert_eq!(
                Picture::open(&out).unwrap().orientation(),
                6,
                "{what} -> {ext}"
            );
        }
        let mut decoder = Decoder::new(BufReader::new(
            File::open(dir.join(format!("{what}.tif"))).unwrap(),
        ))
        .unwrap();
        assert!(
            decoder.find_tag(Tag::Unknown(65000)).unwrap().is_none(),
            "{what}"
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// A PNG's raw profiles of a kind with a home of their own are always taken from its text
/// (review-B #3): a second EXIF, with its thumbnail, isn't written back as text, nor is one
/// that doesn't read. Unknown ones stay.
#[test]
fn known_raw_profiles_never_stay_as_text() {
    let dir = test_dir("raw-profiles");
    let path = dir.join("raw.png");
    let raw = |name: &str, text: String| {
        png::text_metadata::ZTXtChunk::new(format!("Raw profile type {name}"), text)
    };
    let mut info = png::Info::with_size(2, 2);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.exif_metadata = Some(std::borrow::Cow::Owned(exif_with_thumbnail()));
    info.compressed_latin1_text.extend([
        raw("exif", encode_raw_profile("exif", &exif_with_thumbnail())),
        raw("xmp", encode_raw_profile("xmp", XMP)),
        raw("8bim", "\n8bim\n       4\nnot hex\n".into()),
        raw("icc", encode_raw_profile("icc", b"kept")),
    ]);
    let mut writer = png::Encoder::with_info(BufWriter::new(File::create(&path).unwrap()), info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(&[9; 12]).unwrap();
    writer.finish().unwrap();

    let out = dir.join("out.png");
    Picture::open(&path).unwrap().save(&out).unwrap();
    let mut decoder = png::Decoder::new(BufReader::new(File::open(&out).unwrap()));
    decoder.set_ignore_text_chunk(false);
    let reader = decoder.read_info().unwrap();
    let info = reader.info();
    let keywords: Vec<&str> = info
        .uncompressed_latin1_text
        .iter()
        .map(|c| c.keyword.as_str())
        .chain(
            info.compressed_latin1_text
                .iter()
                .map(|c| c.keyword.as_str()),
        )
        .collect();
    assert_eq!(keywords, ["Raw profile type icc"]);
    assert!(!contains(&fs::read(&out).unwrap(), THUMBNAIL));
    let back = Picture::open(&out).unwrap();
    assert_eq!(back.meta().xmp.as_deref(), Some(XMP));
    assert_eq!(back.orientation(), 6);
    fs::remove_dir_all(&dir).unwrap();
}

/// A TIFF's resolution always has a unit: inches when the source's resolution had none
/// (TIFF's default, review-B #6), and 72 pixels per inch when it had no resolution at all
/// (test-B #2).
#[test]
fn a_tiff_resolution_always_has_a_unit() {
    let dir = test_dir("resolution-unit");
    let source = dir.join("no-unit.tif");
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&source).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::RGB8>(2, 1).unwrap();
        image.x_resolution(Rational { n: 300, d: 1 });
        image.y_resolution(Rational { n: 300, d: 1 });
        image.write_data(&[1, 2, 3, 4, 5, 6]).unwrap();
    }
    // Hide the unit the encoder wrote (tag 296 renamed to a private 65001).
    let mut bytes = fs::read(&source).unwrap();
    let at = bytes
        .windows(4)
        .position(|w| w == [0x28, 0x01, 3, 0])
        .unwrap();
    bytes[at..at + 2].copy_from_slice(&65001u16.to_le_bytes());
    fs::write(&source, &bytes).unwrap();
    let resolution = |path: &Path| {
        let mut decoder = Decoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
        (
            decoder.find_tag(Tag::XResolution).unwrap(),
            decoder
                .find_tag_unsigned::<u16>(Tag::ResolutionUnit)
                .unwrap(),
        )
    };
    let out = dir.join("out.tif");
    Picture::open(&source).unwrap().save(&out).unwrap();
    assert_eq!(resolution(&out), (Some(Value::Rational(300, 1)), Some(2)));

    let plain = dir.join("plain.png");
    Picture::from_pixels(pixels(2, 1), 8).save(&plain).unwrap();
    Picture::open(&plain).unwrap().save(&out).unwrap();
    assert_eq!(resolution(&out), (Some(Value::Rational(72, 1)), Some(2)));
    // JFIF's usual "square pixels" (1:1, no unit) says no more.
    let jpeg = dir.join("square.jpg");
    fs::write(&jpeg, jpeg_with(&[], false)).unwrap();
    Picture::open(&jpeg).unwrap().save(&out).unwrap();
    assert_eq!(resolution(&out), (Some(Value::Rational(72, 1)), Some(2)));

    // Nor does a TIFF's own (the tiff crate writes 1/1 with no unit when told nothing, as
    // earlier Gumka builds did); pixels that aren't square keep their ratio.
    let ratio = |x: u32, y: u32| {
        let path = dir.join(format!("ratio-{x}-{y}.tif"));
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::RGB8>(2, 1).unwrap();
        image.x_resolution(Rational { n: x, d: 1 });
        image.y_resolution(Rational { n: y, d: 1 });
        image.write_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        path
    };
    for (x, y, expected) in [(1, 1, (72, 2)), (300, 300, (72, 2)), (2, 1, (2, 1))] {
        Picture::open(&ratio(x, y)).unwrap().save(&out).unwrap();
        let (x, unit) = resolution(&out);
        assert_eq!(
            (x, unit),
            (Some(Value::Rational(expected.0, 1)), Some(expected.1))
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

thread_local! {
    /// An allocation size this thread looks out for, and whether it was asked for.
    static WATCHED: std::cell::Cell<(usize, bool)> = const { std::cell::Cell::new((0, false)) };
}

/// The system allocator, noting allocations of the size the thread watches for.
struct Watching;

impl Watching {
    fn note(size: usize) {
        let _ = WATCHED.try_with(|w| {
            if w.get().0 == size {
                w.set((size, true));
            }
        });
    }
}

// SAFETY: every call goes straight to the system allocator with the same arguments; noting a
// size only touches a thread-local cell, which allocates nothing.
unsafe impl std::alloc::GlobalAlloc for Watching {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        Watching::note(layout.size());
        // SAFETY: the caller's guarantees for `alloc` hold for the system allocator too.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        Watching::note(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        Watching::note(size);
        // SAFETY: `ptr` came from this allocator, which is the system's.
        unsafe { std::alloc::System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: `ptr` came from this allocator, which is the system's.
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Watching = Watching;

/// Whether `work` allocates a block of exactly `size` bytes on this thread.
fn allocates(size: usize, work: impl FnOnce()) -> bool {
    WATCHED.with(|w| w.set((size, false)));
    work();
    WATCHED.with(|w| w.replace((0, false)).1)
}

/// A tag claiming more than the file holds is never allocated, even when the decoded picture
/// is larger still (review-B2 #1): only the picture may be.
#[test]
fn a_tag_claiming_more_than_the_file_is_never_allocated() {
    let dir = test_dir("tag-claims");
    let path = dir.join("packed.tif");
    let (w, h) = (600u32, 600u32);
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap()))
            .unwrap()
            .with_compression(tiff::encoder::Compression::Packbits);
        let mut image = encoder.new_image::<colortype::RGB16>(w, h).unwrap();
        put(image.encoder(), 65001, Type::UNDEFINED, &[7]);
        image.write_data(&vec![0u16; (w * h * 3) as usize]).unwrap();
    }
    // UNDEFINED (7), count 1: made to claim 1.5 MB, more than the file, less than the picture.
    let claim = 1_500_001usize;
    let mut file = fs::read(&path).unwrap();
    let at = file
        .windows(8)
        .position(|e| e == [0xE9, 0xFD, 7, 0, 1, 0, 0, 0])
        .unwrap();
    file[at + 4..at + 8].copy_from_slice(&(claim as u32).to_le_bytes());
    assert!(file.len() < claim && claim < (w * h * 6) as usize);
    fs::write(&path, file).unwrap();
    assert!(!allocates(claim, || {
        Picture::open(&path).unwrap();
    }));
    fs::remove_dir_all(&dir).unwrap();
}

/// Where IPTC goes (review-B #5): a TIFF keeps its Photoshop block as it was, records and
/// digest included, as Photoshop writes it. PNG, and a TIFF made from another format, hold the
/// records apart, and the block without them and their digest. JPEG holds them in the block.
#[test]
fn iptc_goes_where_each_format_keeps_it() {
    let dir = test_dir("iptc-homes");
    let other = resource(0x03ED, RESOLUTION_INFO);
    let block = [
        resource(0x0404, IPTC),
        resource(0x0425, &[7; 16]),
        other.clone(),
    ]
    .concat();
    let source = dir.join("source.tif");
    tiff_with(&source, XMP, &block, Some(IPTC));
    let picture = Picture::open(&source).unwrap();
    let (tif, png, jpg) = (
        dir.join("out.tif"),
        dir.join("out.png"),
        dir.join("out.jpg"),
    );

    picture.save(&tif).unwrap();
    assert!(
        contains(&fs::read(&tif).unwrap(), &block),
        "TIFF: the block as it was"
    );
    picture.save(&jpg).unwrap();
    let back = Picture::open(&jpg).unwrap();
    assert_eq!(back.meta().photoshop.as_deref(), Some(&block[..]), "JPEG");
    picture.save(&png).unwrap();
    let back = Picture::open(&png).unwrap();
    assert_eq!(back.meta().photoshop.as_deref(), Some(&other[..]), "PNG");
    assert_eq!(back.meta().iptc.as_deref(), Some(IPTC), "PNG");
    back.save(&tif).unwrap();
    let back = Picture::open(&tif).unwrap();
    assert_eq!(
        back.meta().photoshop.as_deref(),
        Some(&other[..]),
        "PNG → TIFF"
    );
    assert_eq!(back.meta().iptc.as_deref(), Some(IPTC), "PNG → TIFF");
    fs::remove_dir_all(&dir).unwrap();
}

/// A file the user can't write isn't saved over, as Preview does (test-B #4); saving under
/// another name still works.
#[cfg(unix)]
#[test]
fn a_read_only_file_isnt_saved_over() {
    use std::os::unix::fs::PermissionsExt;
    let dir = test_dir("read-only");
    let path = dir.join("locked.png");
    let picture = Picture::from_pixels(pixels(3, 2), 8);
    picture.save(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    // Root can write it anyway.
    if fs::OpenOptions::new().write(true).open(&path).is_err() {
        let before = fs::read(&path).unwrap();
        let error = Picture::from_pixels(pixels(2, 2), 8).save(&path).err();
        assert_eq!(
            error.expect("refused").to_string(),
            "locked.png is read-only"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "untouched");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary file");
        picture.save(&dir.join("copy.png")).unwrap();
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// A folder the user can't write in says so, rather than "permission denied" (review-B2 #3).
#[cfg(unix)]
#[test]
fn a_folder_that_isnt_writable_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let dir = test_dir("locked-folder");
    let folder = dir.join("locked");
    fs::create_dir_all(&folder).unwrap();
    let path = folder.join("p.png");
    let picture = Picture::from_pixels(pixels(3, 2), 8);
    picture.save(&path).unwrap();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
    // Root can write there anyway.
    if File::create(folder.join("probe")).is_err() {
        let error = picture.save(&path).expect_err("refused").to_string();
        assert!(
            error.starts_with("Gumka can't save in ") && error.contains("locked"),
            "{error}"
        );
        picture.save(&dir.join("elsewhere.png")).unwrap();
    }
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir_all(&dir).unwrap();
}

/// Planar TIFFs (each channel stored apart) are refused with a message that says so.
#[test]
fn a_planar_tiff_is_refused_clearly() {
    let dir = test_dir("planar");
    // By hand, as tiff's encoder writes only interleaved samples: 2 x 1 RGB, one strip per
    // channel. The values follow the directory at 8 (2 + 10 entries + 4 bytes = 126).
    let entry = |tag: u16, ty: u16, count: u32, value: u32| {
        [tag.to_le_bytes(), ty.to_le_bytes()]
            .concat()
            .into_iter()
            .chain(count.to_le_bytes())
            .chain(value.to_le_bytes())
            .collect::<Vec<u8>>()
    };
    let (bits, offsets, counts, data) = (134, 140, 152, 164u32);
    let file = [
        b"II*\0\x08\0\0\0\x0a\0".to_vec(),
        entry(256, 3, 1, 2),       // width
        entry(257, 3, 1, 1),       // length
        entry(258, 3, 3, bits),    // bits per sample
        entry(259, 3, 1, 1),       // no compression
        entry(262, 3, 1, 2),       // RGB
        entry(273, 4, 3, offsets), // strip offsets
        entry(277, 3, 1, 3),       // samples per pixel
        entry(278, 3, 1, 1),       // rows per strip
        entry(279, 4, 3, counts),  // strip byte counts
        entry(284, 3, 1, 2),       // planar
        vec![0; 4],
        [8u16, 8, 8].iter().flat_map(|v| v.to_le_bytes()).collect(),
        [data, data + 2, data + 4]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
        [2u32, 2, 2].iter().flat_map(|v| v.to_le_bytes()).collect(),
        vec![10, 11, 20, 21, 30, 31],
    ]
    .concat();
    assert_eq!(file.len(), data as usize + 6);
    let path = dir.join("planar.tif");
    fs::write(&path, file).unwrap();
    let error = Picture::open(&path).err().expect("refused").to_string();
    assert!(error.contains("planar TIFFs"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}

/// EXIF's record of the pixel size (40962, 40963) in the file at `path`.
fn exif_size(path: &Path) -> (u64, u64) {
    let picture = Picture::open(path).unwrap();
    let tags = picture.meta().exif.as_ref().unwrap().to_tags().unwrap();
    let exif = child(&tags, EXIF_DIRECTORY);
    let number = |tag| {
        let v = exif.get(tag).unwrap();
        match v.ty {
            Type::SHORT => u64::from(u16::from_ne_bytes([v.bytes[0], v.bytes[1]])),
            _ => u64::from(u32::from_ne_bytes(v.bytes[..4].try_into().unwrap())),
        }
    };
    (number(40962), number(40963))
}

/// A picture that changes size says so in its EXIF in every format, through EXIF kept as tags
/// (from a TIFF) and as bytes (from a PNG); one that doesn't keeps EXIF's values as they were.
#[test]
fn a_resized_picture_saves_its_new_size_in_exif() {
    let dir = test_dir("resized");
    let tif = dir.join("source.tif");
    // A size the file doesn't have, as a camera's EXIF says after a crop elsewhere.
    let sizes = [
        (40962, Type::SHORT, 37u16.to_ne_bytes().to_vec()),
        (40963, Type::LONG, 23u32.to_ne_bytes().to_vec()),
    ];
    rich_tiff(&tif, &pixels(30, 20), &sizes);
    for ext in ["tif", "png", "jpg"] {
        let out = dir.join(format!("kept.{ext}"));
        Picture::open(&tif).unwrap().save(&out).unwrap();
        assert_eq!(
            exif_size(&out),
            (37, 23),
            "{ext}: not resized, kept as it was"
        );
    }

    let bigger = pixels(60, 40);
    let mut picture = Picture::open(&tif).unwrap();
    picture.swap_pixels(&mut bigger.clone(), &mut None).unwrap();
    for ext in ["tif", "png", "jpg"] {
        let out = dir.join(format!("bigger.{ext}"));
        picture.save(&out).unwrap();
        assert_eq!(exif_size(&out), (60, 40), "{ext}");
        check_rich(&out, true);
        if ext != "jpg" {
            assert_eq!(Picture::open(&out).unwrap().pixels, bigger, "{ext}");
        }
    }

    let mut picture = Picture::open(&dir.join("bigger.png")).unwrap();
    picture
        .swap_pixels(&mut pixels(120, 80), &mut None)
        .unwrap();
    let out = dir.join("again.jpg");
    picture.save(&out).unwrap();
    assert_eq!(exif_size(&out), (120, 80), "EXIF bytes from a PNG");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn pixels_of_another_size_bring_an_alpha_to_match() {
    let dir = test_dir("resized-alpha");
    let source = dir.join("rgba.png");
    let rgba = image::ImageBuffer::<image::Rgba<u16>, _>::from_fn(6, 4, |x, y| {
        image::Rgba([
            x as u16 * 9000,
            y as u16 * 9000,
            500,
            60000 - x as u16 * 1000,
        ])
    });
    rgba.save(&source).unwrap();
    let mut picture = Picture::open(&source).unwrap();
    assert_eq!(picture.alpha().map(<[u16]>::len), Some(24));

    // Refused, changing nothing on either side: no alpha, or one of the old size.
    let mut new = pixels(12, 8);
    assert!(picture.swap_pixels(&mut new, &mut None).is_err());
    let mut short = Some(vec![0; 24]);
    assert!(picture.swap_pixels(&mut new, &mut short).is_err());
    assert_eq!(picture.pixels.dimensions(), (6, 4));
    assert_eq!(new.dimensions(), (12, 8));
    assert_eq!(short.map(|a| a.len()), Some(24));

    let alpha: Vec<u16> = (0..96).map(|i| i * 600).collect();
    let mut old_alpha = Some(alpha.clone());
    picture.swap_pixels(&mut new, &mut old_alpha).unwrap();
    assert_eq!(new.dimensions(), (6, 4), "the old pixels come back");
    assert_eq!(old_alpha.map(|a| a.len()), Some(24));
    for ext in ["png", "tif"] {
        let out = dir.join(format!("bigger.{ext}"));
        picture.save(&out).unwrap();
        let back = Picture::open(&out).unwrap();
        assert_eq!(back.pixels, pixels(12, 8), "{ext}");
        assert_eq!(back.alpha(), Some(&alpha[..]), "{ext}");
    }

    // A picture without alpha takes none.
    let mut plain = Picture::from_pixels(pixels(3, 3), 16);
    assert!(
        plain
            .swap_pixels(&mut pixels(6, 6), &mut Some(vec![0; 36]))
            .is_err()
    );
    assert!(plain.swap_pixels(&mut pixels(6, 6), &mut None).is_ok());
    fs::remove_dir_all(&dir).unwrap();
}
