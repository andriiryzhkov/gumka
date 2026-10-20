// SPDX-License-Identifier: GPL-3.0-or-later

//! An open picture, its mask and its undo history.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gumka_core::picture::Picture;
use gumka_core::{Mask, Rect, Rgb16, mask_bounds, swap_region};

/// Pixel generations for all documents, so that one never names the pixels of two pictures.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATIONS.fetch_add(1, Ordering::Relaxed)
}

/// One undoable change. Undo and redo both swap the stored regions with the document's, so
/// the same edit moves back and forth between the two stacks.
struct Edit {
    id: u64,
    label: String,
    /// Shown on the right of the history row: the model and time for an erase.
    detail: String,
    pixels: Option<(Rect, Rgb16)>,
    mask: Option<(Rect, Mask)>,
    /// The mask was inverted, which inverting again undoes: nothing needs storing.
    inverted: bool,
}

/// Regions that changed and need redrawing.
#[derive(Default)]
pub struct Changed {
    pub pixels: Option<Rect>,
    pub mask: Option<Rect>,
}

/// One row of the History panel.
pub struct HistoryRow {
    pub label: String,
    pub detail: String,
    /// The state the picture is in now.
    pub current: bool,
    /// Undone; redo brings it back.
    pub undone: bool,
}

pub struct Document {
    pub path: PathBuf,
    /// Another app sent it (a RAW editor's Edit With, Finder, the Dock, or a command line):
    /// the toolbar offers Save & Return.
    pub sent: bool,
    pub picture: Picture,
    /// Brush strokes are painted straight into it, and count for [`Self::has_mask`] once they
    /// are recorded with [`Self::push_stroke`].
    pub mask: Mask,
    /// Whether any mask pixel is set; kept up to date by every change of the mask, so that it
    /// needn't be scanned each frame.
    has_mask: bool,
    /// File name when opened, for the first history row.
    opened_as: String,
    width: u32,
    height: u32,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    /// Id of the newest pixel edit when last saved (0 = as opened).
    saved: u64,
    /// Renewed whenever the pixels change (erase, undo, redo), so cached work on them, such
    /// as the selection model's encoding, knows when it is stale. Unique across documents.
    generation: u64,
}

impl Document {
    pub fn open(path: PathBuf) -> Result<Self, gumka_core::picture::Error> {
        let picture = Picture::open(&path)?;
        Ok(Self::new(path, picture))
    }

    fn new(path: PathBuf, picture: Picture) -> Self {
        let (width, height) = (picture.width(), picture.height());
        Self {
            opened_as: file_name(&path),
            path,
            sent: false,
            picture,
            mask: Mask::new(width, height),
            has_mask: false,
            width,
            height,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved: 0,
            generation: next_generation(),
        }
    }

    /// Size in pixels. Valid even while the pixels are away at the worker.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Size as viewed, after the picture's orientation.
    pub fn viewed_size(&self) -> (u32, u32) {
        gumka_core::picture::oriented_size(self.size(), self.picture.orientation())
    }

    pub fn name(&self) -> String {
        file_name(&self.path)
    }

    /// Unsaved pixel changes. Mask edits do not count: the mask is not saved.
    pub fn is_dirty(&self) -> bool {
        self.pixel_version() != self.saved
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.pixel_version();
    }

    fn pixel_version(&self) -> u64 {
        self.undo
            .iter()
            .rev()
            .find(|e| e.pixels.is_some())
            .map_or(0, |e| e.id)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Changes whenever the pixels do.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn has_mask(&self) -> bool {
        self.has_mask
    }

    /// The History panel: the opened file, the edits, then undone edits.
    pub fn history(&self) -> Vec<HistoryRow> {
        let open = HistoryRow {
            label: format!("Open {}", self.opened_as),
            detail: String::new(),
            current: self.undo.is_empty(),
            undone: false,
        };
        let done = self.undo.iter().enumerate().map(|(i, e)| HistoryRow {
            label: e.label.clone(),
            detail: e.detail.clone(),
            current: i + 1 == self.undo.len(),
            undone: false,
        });
        let undone = self.redo.iter().rev().map(|e| HistoryRow {
            label: e.label.clone(),
            detail: e.detail.clone(),
            current: false,
            undone: true,
        });
        std::iter::once(open).chain(done).chain(undone).collect()
    }

    /// Records a finished brush stroke: `before` is the mask before the stroke and `rect`
    /// covers everything the stroke touched.
    pub fn push_stroke(&mut self, before: &Mask, rect: Rect, removed: bool) {
        let old = image_crop(before, rect);
        let label = if removed {
            "Remove from mask"
        } else {
            "Brush stroke"
        };
        self.push(label, "mask", None, Some((rect, old)));
        // A stroke that adds can only have set pixels inside its region.
        self.has_mask = if removed {
            any_set(&self.mask, self.whole())
        } else {
            self.has_mask || any_set(&self.mask, rect)
        };
    }

    /// Clears the mask, undoably. Returns the cleared region.
    pub fn clear_mask(&mut self) -> Option<Rect> {
        let rect = mask_bounds(&self.mask)?;
        let old = image_crop(&self.mask, rect);
        fill_mask(&mut self.mask, rect, 0);
        self.push("Clear mask", "mask", None, Some((rect, old)));
        self.has_mask = false;
        Some(rect)
    }

    /// Replaces the mask (a selection from clicks), undoably; only the region that changes is
    /// stored. Returns that region, empty (and no step in the history) if nothing changes, or
    /// `None` (and keeps the mask) if `mask` isn't the picture's size.
    pub fn replace_mask(&mut self, mask: Mask, label: &str, detail: &str) -> Option<Rect> {
        if mask.dimensions() != self.size() {
            return None;
        }
        let Some(rect) = changed_bounds(&self.mask, &mask) else {
            return Some(Rect::new(0, 0, 0, 0));
        };
        let old = image_crop(&self.mask, rect);
        self.mask = mask;
        self.push(label, detail, None, Some((rect, old)));
        self.has_mask = any_set(&self.mask, self.whole());
        Some(rect)
    }

    /// Inverts the mask over the whole picture, undoably. Returns the changed region.
    pub fn invert_mask(&mut self) -> Rect {
        invert(&mut self.mask);
        self.record(Edit {
            id: 0,
            label: "Invert mask".into(),
            detail: "mask".into(),
            pixels: None,
            mask: None,
            inverted: true,
        });
        self.has_mask = any_set(&self.mask, self.whole());
        self.whole()
    }

    /// Records an erase whose result is already in the pixels: `old` holds the previous pixels
    /// of `rect`. Also clears the mask that was used. Returns the regions to redraw.
    pub fn push_erase(&mut self, rect: Rect, old: Rgb16, detail: String) -> Changed {
        let mask = mask_bounds(&self.mask).map(|mask_rect| {
            let old_mask = image_crop(&self.mask, mask_rect);
            fill_mask(&mut self.mask, mask_rect, 0);
            (mask_rect, old_mask)
        });
        let changed = Changed {
            pixels: Some(rect),
            mask: mask.as_ref().map(|(r, _)| *r),
        };
        self.push("Erase", &detail, Some((rect, old)), mask);
        self.has_mask = false;
        self.generation = next_generation();
        changed
    }

    pub fn undo(&mut self) -> Changed {
        self.step(true)
    }

    pub fn redo(&mut self) -> Changed {
        self.step(false)
    }

    fn step(&mut self, undo: bool) -> Changed {
        let whole = self.whole();
        let (from, to) = if undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let Some(mut edit) = from.pop() else {
            return Changed::default();
        };
        let mut changed = Changed::default();
        if let Some((rect, pixels)) = &mut edit.pixels {
            swap_region(&mut self.picture.pixels, *rect, pixels);
            changed.pixels = Some(*rect);
            self.generation = next_generation();
        }
        if let Some((rect, mask)) = &mut edit.mask {
            swap_mask(&mut self.mask, *rect, mask);
            changed.mask = Some(*rect);
            self.has_mask = any_set(&self.mask, whole);
        }
        if edit.inverted {
            invert(&mut self.mask);
            changed.mask = Some(whole);
            self.has_mask = any_set(&self.mask, whole);
        }
        to.push(edit);
        changed
    }

    fn whole(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn push(
        &mut self,
        label: &str,
        detail: &str,
        pixels: Option<(Rect, Rgb16)>,
        mask: Option<(Rect, Mask)>,
    ) {
        self.record(Edit {
            id: 0,
            label: label.to_owned(),
            detail: detail.to_owned(),
            pixels,
            mask,
            inverted: false,
        });
    }

    /// Adds `edit` to the history under a new id; what was undone can't be redone after it.
    fn record(&mut self, edit: Edit) {
        self.undo.push(Edit {
            id: self.next_id,
            ..edit
        });
        self.next_id += 1;
        self.redo.clear();
    }

    /// Takes the pixels out (to hand them to the worker); [`Self::restore_pixels`] puts them back.
    pub fn take_pixels(&mut self) -> Rgb16 {
        std::mem::take(&mut self.picture.pixels)
    }

    pub fn restore_pixels(&mut self, pixels: Rgb16) {
        self.picture.pixels = pixels;
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Whether any pixel of `rect` is set.
fn any_set(mask: &Mask, rect: Rect) -> bool {
    let width = mask.width() as usize;
    (rect.y..rect.bottom()).any(|y| {
        let start = y as usize * width + rect.x as usize;
        mask.as_raw()[start..start + rect.width as usize]
            .iter()
            .any(|&v| v != 0)
    })
}

/// The bounds of the pixels where `a` and `b` (the same size) differ, or `None` if none do.
fn changed_bounds(a: &Mask, b: &Mask) -> Option<Rect> {
    let width = a.width() as usize;
    if width == 0 {
        return None;
    }
    let (mut x0, mut x1, mut rows) = (width, 0, None::<(usize, usize)>);
    let pairs = a
        .as_raw()
        .chunks_exact(width)
        .zip(b.as_raw().chunks_exact(width));
    for (y, (ra, rb)) in pairs.enumerate() {
        if ra == rb {
            continue;
        }
        let first = ra.iter().zip(rb).position(|(p, q)| p != q).unwrap_or(0);
        let last = ra
            .iter()
            .zip(rb)
            .rposition(|(p, q)| p != q)
            .unwrap_or(first);
        x0 = x0.min(first);
        x1 = x1.max(last + 1);
        rows = Some((rows.map_or(y, |(top, _)| top), y + 1));
    }
    let (top, bottom) = rows?;
    Some(Rect::new(
        x0 as u32,
        top as u32,
        (x1 - x0) as u32,
        (bottom - top) as u32,
    ))
}

/// Inverts every mask value (255 - v), which inverting again undoes exactly.
fn invert(mask: &mut Mask) {
    for v in mask.iter_mut() {
        *v = 255 - *v;
    }
}

fn image_crop(mask: &Mask, rect: Rect) -> Mask {
    gumka_core::image::imageops::crop_imm(mask, rect.x, rect.y, rect.width, rect.height).to_image()
}

fn fill_mask(mask: &mut Mask, rect: Rect, value: u8) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            mask.put_pixel(x, y, gumka_core::image::Luma([value]));
        }
    }
}

fn swap_mask(mask: &mut Mask, rect: Rect, patch: &mut Mask) {
    let width = mask.width() as usize;
    let row = rect.width as usize;
    let mask = mask.as_mut();
    let patch = patch.as_mut();
    for y in 0..rect.height as usize {
        let start = (rect.y as usize + y) * width + rect.x as usize;
        mask[start..start + row].swap_with_slice(&mut patch[y * row..(y + 1) * row]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gumka_core::image::Luma;

    fn document(width: u32, height: u32) -> Document {
        let picture = Picture::from_pixels(Rgb16::new(width, height), 16);
        Document::new(PathBuf::from("test.png"), picture)
    }

    /// Paints one mask pixel the way the canvas does, then records the stroke.
    fn stroke(doc: &mut Document, x: u32, y: u32, removed: bool) {
        let before = doc.mask.clone();
        doc.mask
            .put_pixel(x, y, Luma([if removed { 0 } else { 255 }]));
        doc.push_stroke(&before, Rect::new(x, y, 1, 1), removed);
    }

    #[test]
    fn generations_differ_between_documents_and_change_with_the_pixels() {
        let (a, mut b) = (document(4, 4), document(4, 4));
        assert_ne!(a.generation(), b.generation());
        let opened = b.generation();
        b.push_erase(Rect::new(0, 0, 1, 1), Rgb16::new(1, 1), String::new());
        let erased = b.generation();
        assert_ne!(erased, opened);
        b.undo();
        assert!(![opened, erased, a.generation()].contains(&b.generation()));
    }

    #[test]
    fn has_mask_follows_every_change_of_the_mask() {
        let mut doc = document(6, 5);
        assert!(!doc.has_mask());
        stroke(&mut doc, 2, 3, false);
        assert!(doc.has_mask());
        stroke(&mut doc, 4, 1, false);
        stroke(&mut doc, 2, 3, true);
        assert!(doc.has_mask(), "one pixel is left");
        stroke(&mut doc, 4, 1, true);
        assert!(!doc.has_mask());
        doc.undo();
        assert!(doc.has_mask());
        doc.redo();
        assert!(!doc.has_mask());
        doc.invert_mask();
        assert!(doc.has_mask());
        doc.clear_mask();
        assert!(!doc.has_mask());
        let mut selected = Mask::new(6, 5);
        selected.put_pixel(5, 4, Luma([255]));
        doc.replace_mask(selected, "Select object", "");
        assert!(doc.has_mask());
        doc.push_erase(Rect::new(5, 4, 1, 1), Rgb16::new(1, 1), String::new());
        assert!(!doc.has_mask());
        doc.undo();
        assert!(doc.has_mask(), "undoing the erase brings its mask back");
    }

    #[test]
    fn undoing_a_selection_or_an_invert_updates_has_mask() {
        let mut doc = document(6, 5);
        let mut selected = Mask::new(6, 5);
        selected.put_pixel(1, 1, Luma([255]));
        assert!(doc.replace_mask(selected, "Select object", "").is_some());
        assert!(doc.has_mask());
        doc.undo();
        assert!(!doc.has_mask(), "the selection is undone");
        doc.redo();
        assert!(doc.has_mask());
        doc.clear_mask();
        doc.invert_mask();
        assert!(doc.has_mask(), "everything is selected");
        doc.invert_mask();
        assert!(!doc.has_mask(), "and then nothing");
        doc.undo();
        assert!(doc.has_mask());
    }

    #[test]
    fn a_stroke_that_removes_elsewhere_keeps_has_mask() {
        let mut doc = document(6, 5);
        stroke(&mut doc, 0, 0, false);
        stroke(&mut doc, 5, 4, true);
        assert!(doc.has_mask());
        // A stroke that adds nothing new (painting over set pixels) keeps it too.
        stroke(&mut doc, 0, 0, false);
        assert!(doc.has_mask());
    }

    /// A selection stores and redraws only what it changes (C8); one that changes nothing
    /// leaves no step.
    #[test]
    fn a_selection_stores_only_what_it_changes() {
        let mut doc = document(40, 30);
        stroke(&mut doc, 1, 1, false);
        let mut selected = doc.mask.clone();
        for (x, y) in [(10, 5), (20, 12), (15, 8)] {
            selected.put_pixel(x, y, Luma([255]));
        }
        let before = doc.mask.clone();
        let rect = doc
            .replace_mask(selected.clone(), "Select object", "")
            .unwrap();
        assert_eq!(rect, Rect::new(10, 5, 11, 8));
        let stored = &doc.undo.last().unwrap().mask.as_ref().unwrap().1;
        assert_eq!(
            stored.dimensions(),
            (11, 8),
            "only the changed region is kept"
        );
        assert_eq!(doc.mask, selected);

        let steps = doc.history().len();
        let same = doc
            .replace_mask(selected.clone(), "Select object", "")
            .unwrap();
        assert!(same.is_empty(), "nothing changed");
        assert_eq!(doc.history().len(), steps, "and no step for it");

        assert_eq!(doc.undo().mask, Some(rect));
        assert_eq!(doc.mask, before);
        assert!(doc.has_mask());
        doc.redo();
        assert_eq!(doc.mask, selected);
    }

    /// Inverting is stored as a flag and undone by inverting again, exactly, whatever the
    /// values (review-C #8).
    #[test]
    fn an_invert_is_undone_exactly() {
        let mut doc = document(4, 3);
        for (i, v) in doc.mask.iter_mut().enumerate() {
            *v = [0, 255, 7, 128][i % 4];
        }
        let before = doc.mask.clone();
        assert_eq!(doc.invert_mask(), Rect::new(0, 0, 4, 3));
        assert!(doc.undo.last().unwrap().mask.is_none(), "nothing stored");
        assert_eq!(doc.mask.get_pixel(0, 0).0[0], 255);
        assert_eq!(doc.mask.get_pixel(2, 0).0[0], 248);
        assert_eq!(doc.undo().mask, Some(Rect::new(0, 0, 4, 3)));
        assert_eq!(doc.mask, before);
        doc.redo();
        doc.redo(); // nothing left to redo
        assert_eq!(doc.mask.get_pixel(1, 0).0[0], 0);
        doc.undo();
        assert_eq!(doc.mask, before);
    }

    #[test]
    fn a_mask_of_another_size_is_refused() {
        let mut doc = document(6, 5);
        let mut other = Mask::new(5, 6);
        other.put_pixel(0, 0, Luma([255]));
        assert_eq!(doc.replace_mask(other, "Select object", ""), None);
        assert!(!doc.has_mask());
        assert!(!doc.can_undo());
    }
}
