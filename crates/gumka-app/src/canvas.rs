// SPDX-License-Identifier: GPL-3.0-or-later

//! The picture on screen: view transform, preview textures and brush painting.
//!
//! The picture and the mask overlay are each shown from a pyramid of textures (full size,
//! 1/2, 1/4, ...), because egui's renderer has no mipmaps: drawing the level closest to the
//! zoom keeps zoomed-out views from shimmering. Every level is cut into tiles, so a picture
//! larger than the GPU's texture limit still shows every pixel at 100%. Only the coarsest
//! level is made up front; a finer tile is made when it first comes into view, a few each
//! frame, and until then the nearest coarser level that is made stands in for it. Edits update
//! only the changed region of the tiles made so far.
//!
//! Colors are converted from the picture's profile into the screen's (see display.rs).

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, Frame, Margin, Pos2, Rect as ScreenRect,
    RichText, Stroke as LineStroke, TextureFilter, TextureHandle, TextureOptions, TextureWrapMode,
    Vec2, pos2, vec2,
};
use gumka_core::color::DisplayTransform;
use gumka_core::picture::{oriented_size, stored_to_viewed, viewed_to_stored};
use gumka_core::{Mask, Rect, Rgb16};

use gumka_core::select::Click;

use crate::theme::{BG_VIEWER, MASK_OPACITY, MASK_RGB, OK, TEXT, TEXT_STRONG};
use crate::widgets;
/// Stop adding pyramid levels below this size.
const SMALLEST_LEVEL: u32 = 256;

/// The largest texture a tile is made in, even where the GPU takes larger ones: smaller tiles
/// are made in smaller steps as they come into view.
const LARGEST_TEXTURE: u32 = 2048;

/// Time spent making tiles each frame, so that panning and zooming stay smooth. Not a cap: the
/// first tile of a frame is made however long it takes.
const TILE_BUDGET: Duration = Duration::from_millis(25);

const TEXTURE_OPTIONS: TextureOptions = TextureOptions {
    magnification: TextureFilter::Nearest,
    minification: TextureFilter::Linear,
    wrap_mode: TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

struct Level {
    factor: u32,
    /// Size in texels: the picture's, divided by `factor` and rounded up.
    size: (u32, u32),
    /// The tiles row by row; `None` until made.
    tiles: Vec<Option<TextureHandle>>,
}

struct Pyramid {
    name: &'static str,
    /// The picture size it shows, in stored pixels.
    size: (u32, u32),
    /// A tile's side in texels. Its texture has a texel more on every side within the level
    /// (see [`Self::texture_rect`]).
    tile: u32,
    /// Finest first. The coarsest is always made in full.
    levels: Vec<Level>,
}

impl Pyramid {
    /// Levels from full size down, in tiles of `tile` texels. Only the coarsest is made now,
    /// from `coarse`, rendered at [`coarsest_factor`].
    fn lazy(
        ctx: &egui::Context,
        name: &'static str,
        size: (u32, u32),
        tile: u32,
        coarse: ColorImage,
    ) -> Self {
        let mut pyramid = Self::unmade(name, size, tile, 1, coarsest_factor(size));
        let last = pyramid.levels.len() - 1;
        pyramid.fill(ctx, last, coarse);
        pyramid
    }

    /// Levels made in full now, one texture each, from the finest a texture of `max_side`
    /// holds: for a picture that never changes.
    fn eager(
        ctx: &egui::Context,
        name: &'static str,
        size: (u32, u32),
        max_side: u32,
        render: impl Fn(u32, Rect) -> ColorImage,
    ) -> Self {
        let first = size.0.max(size.1).div_ceil(max_side).next_power_of_two();
        let last = coarsest_factor(size).max(first);
        let mut pyramid = Self::unmade(name, size, max_side, first, last);
        for i in 0..pyramid.levels.len() {
            let Level { factor, size, .. } = pyramid.levels[i];
            pyramid.fill(ctx, i, render(factor, Rect::new(0, 0, size.0, size.1)));
        }
        pyramid
    }

    /// Levels from `first` to `last` (powers of two), none of their tiles made.
    fn unmade(name: &'static str, size: (u32, u32), tile: u32, first: u32, last: u32) -> Self {
        let levels = std::iter::successors(Some(first), |&f| (f < last).then_some(f * 2))
            .map(|factor| {
                let (lw, lh) = level_size(size, factor);
                let count = lw.div_ceil(tile) * lh.div_ceil(tile);
                Level {
                    factor,
                    size: (lw, lh),
                    tiles: (0..count).map(|_| None).collect(),
                }
            })
            .collect();
        Self {
            name,
            size,
            tile,
            levels,
        }
    }

    /// Makes every tile of `level` from `image`, the whole level.
    fn fill(&mut self, ctx: &egui::Context, level: usize, image: ColorImage) {
        let count = self.levels[level].tiles.len();
        if count == 1 {
            self.set_tile(ctx, level, 0, image);
            return;
        }
        for index in 0..count {
            let part = crop(&image, self.texture_rect(level, index));
            self.set_tile(ctx, level, index, part);
        }
    }

    fn set_tile(&mut self, ctx: &egui::Context, level: usize, index: usize, image: ColorImage) {
        let rect = self.tile_rect(level, index);
        let factor = self.levels[level].factor;
        let (tx, ty) = (rect.x / self.tile, rect.y / self.tile);
        let name = format!("{}-{factor}-{tx}-{ty}", self.name);
        self.levels[level].tiles[index] = Some(ctx.load_texture(name, image, TEXTURE_OPTIONS));
    }

    /// Tile `index` of `level`, in the level's texels.
    fn tile_rect(&self, level: usize, index: usize) -> Rect {
        let (lw, lh) = self.levels[level].size;
        let columns = lw.div_ceil(self.tile).max(1);
        let (x, y) = (
            index as u32 % columns * self.tile,
            index as u32 / columns * self.tile,
        );
        Rect::new(x, y, self.tile.min(lw - x), self.tile.min(lh - y))
    }

    /// What tile `index`'s texture holds, in the level's texels: the tile and an apron of one
    /// texel from its neighbours, within the level. Filtered across the tile's edge, it then
    /// gives what one texture of the whole level would, not its own edge again, which would
    /// show as faint lines between tiles when zoomed out.
    fn texture_rect(&self, level: usize, index: usize) -> Rect {
        let tile = self.tile_rect(level, index);
        let (lw, lh) = self.levels[level].size;
        let (x0, y0) = (tile.x.saturating_sub(1), tile.y.saturating_sub(1));
        let (x1, y1) = ((tile.right() + 1).min(lw), (tile.bottom() + 1).min(lh));
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// What tile `index` of `level` shows, in stored pixels (left, top, right, bottom): its
    /// texels' blocks, cut at the picture's edges.
    fn area(&self, level: usize, index: usize) -> [f32; 4] {
        let r = self.tile_rect(level, index);
        let f = self.levels[level].factor;
        let (w, h) = self.size;
        [
            r.x * f,
            r.y * f,
            (r.right() * f).min(w),
            (r.bottom() * f).min(h),
        ]
        .map(|v| v as f32)
    }

    /// Re-renders `rect` (in picture pixels) in the tiles made so far, aprons included. The
    /// others are made from the picture as it is when they come into view.
    fn update(&mut self, rect: Rect, render: impl Fn(u32, Rect) -> ColorImage) {
        for level in 0..self.levels.len() {
            let f = self.levels[level].factor;
            let (lw, lh) = self.levels[level].size;
            let (x0, y0) = (rect.x / f, rect.y / f);
            let (x1, y1) = (
                rect.right().div_ceil(f).min(lw),
                rect.bottom().div_ceil(f).min(lh),
            );
            for index in 0..self.levels[level].tiles.len() {
                let held = self.texture_rect(level, index);
                let (ix0, iy0) = (x0.max(held.x), y0.max(held.y));
                let (ix1, iy1) = (x1.min(held.right()), y1.min(held.bottom()));
                if ix1 <= ix0 || iy1 <= iy0 {
                    continue;
                }
                if let Some(texture) = &mut self.levels[level].tiles[index] {
                    let image = render(f, Rect::new(ix0, iy0, ix1 - ix0, iy1 - iy0));
                    let at = [(ix0 - held.x) as usize, (iy0 - held.y) as usize];
                    texture.set_partial(at, image, TEXTURE_OPTIONS);
                }
            }
        }
    }

    /// The coarsest level that still has at least one texel per screen pixel.
    fn pick(&self, pixels_per_image_pixel: f32) -> usize {
        let wanted = (1.0 / pixels_per_image_pixel).max(1.0);
        self.levels
            .iter()
            .rposition(|l| l.factor as f32 <= wanted)
            .unwrap_or(0)
    }

    /// Makes the tiles of `level` that are on screen and not made yet, while `budget` lasts.
    /// Returns whether any are left.
    fn make_visible(
        &mut self,
        ctx: &egui::Context,
        level: usize,
        place: &Place,
        budget: &mut Budget,
        render: impl Fn(u32, Rect) -> ColorImage,
    ) -> bool {
        for index in 0..self.levels[level].tiles.len() {
            if self.levels[level].tiles[index].is_some() || !place.shows(self.area(level, index)) {
                continue;
            }
            if !budget.take() {
                return true;
            }
            let f = self.levels[level].factor;
            let image = render(f, self.texture_rect(level, index));
            self.set_tile(ctx, level, index, image);
        }
        false
    }

    /// The quads that draw `level` where `place` puts the picture, where it shows: each tile
    /// from its own texture or, until it is made, from the next coarser level's, and so on
    /// down to the coarsest, which is always made. Every part is drawn once: the mask overlay
    /// is translucent.
    fn meshes(&self, level: usize, place: &Place) -> Vec<egui::Mesh> {
        let mut meshes = Vec::new();
        let whole = [0.0, 0.0, self.size.0 as f32, self.size.1 as f32];
        self.cover(level, whole, place, &mut meshes);
        meshes
    }

    /// Adds the quads that draw `area` (stored pixels) from the tiles of `level` that are made,
    /// and from coarser levels where they aren't.
    fn cover(&self, level: usize, area: [f32; 4], place: &Place, meshes: &mut Vec<egui::Mesh>) {
        for (index, tile) in self.levels[level].tiles.iter().enumerate() {
            let t = self.area(level, index);
            let part = [
                area[0].max(t[0]),
                area[1].max(t[1]),
                area[2].min(t[2]),
                area[3].min(t[3]),
            ];
            if part[0] >= part[2] || part[1] >= part[3] || !place.shows(part) {
                continue;
            }
            match tile {
                Some(texture) => meshes.push(self.quad(level, index, texture, part, place)),
                None if level + 1 < self.levels.len() => {
                    self.cover(level + 1, part, place, meshes);
                }
                None => {}
            }
        }
    }

    /// A quad over `area` (stored pixels, within tile `index` of `level`), textured from that
    /// tile, its apron left out. A level covers whole blocks, so at the picture's right and
    /// bottom edges its last texels reach past the picture; the texture coordinates stop at
    /// the edge.
    fn quad(
        &self,
        level: usize,
        index: usize,
        texture: &TextureHandle,
        [x0, y0, x1, y1]: [f32; 4],
        place: &Place,
    ) -> egui::Mesh {
        let f = self.levels[level].factor as f32;
        let held = self.texture_rect(level, index);
        let uv = |x: f32, y: f32| {
            pos2(
                (x / f - held.x as f32) / held.width as f32,
                (y / f - held.y as f32) / held.height as f32,
            )
        };
        let mut mesh = egui::Mesh::with_texture(texture.id());
        for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: place.to_screen(x, y),
                uv: uv(x, y),
                color: Color32::WHITE,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        mesh
    }
}

/// Where a pyramid's stored pixels are on screen: turned by the picture's orientation over
/// the viewed rectangle `shown`, and only drawn within `clip`.
struct Place {
    orientation: u8,
    size: (f32, f32),
    shown: ScreenRect,
    clip: ScreenRect,
}

impl Place {
    fn to_screen(&self, x: f32, y: f32) -> Pos2 {
        let (u, v) = stored_to_viewed(self.orientation, x / self.size.0, y / self.size.1);
        self.shown.min + vec2(u * self.shown.width(), v * self.shown.height())
    }

    /// A rectangle of stored pixels (left, top, right, bottom) on screen.
    fn rect(&self, [x0, y0, x1, y1]: [f32; 4]) -> ScreenRect {
        ScreenRect::from_two_pos(self.to_screen(x0, y0), self.to_screen(x1, y1))
    }

    /// Whether some of a rectangle of stored pixels is within `clip`; one that only touches
    /// its edge isn't.
    fn shows(&self, area: [f32; 4]) -> bool {
        let (r, clip) = (self.rect(area), self.clip);
        r.min.x < clip.max.x && clip.min.x < r.max.x && r.min.y < clip.max.y && clip.min.y < r.max.y
    }
}

/// The time left this frame for making tiles.
struct Budget {
    until: Instant,
    used: bool,
}

impl Budget {
    fn new() -> Self {
        Self {
            until: Instant::now() + TILE_BUDGET,
            used: false,
        }
    }

    /// No deadline in practice, for tests that need every tile in one call however slow the
    /// machine (a busy CI runner can take over 25 ms for a few tiles).
    #[cfg(test)]
    fn unlimited() -> Self {
        Self {
            until: Instant::now() + Duration::from_secs(3600),
            used: false,
        }
    }

    /// Whether another tile can be made now; the first always can.
    fn take(&mut self) -> bool {
        let ok = !self.used || Instant::now() < self.until;
        self.used = true;
        ok
    }
}

fn level_size((w, h): (u32, u32), factor: u32) -> (u32, u32) {
    (w.div_ceil(factor), h.div_ceil(factor))
}

/// The whole of a level, in its texels.
fn level_rect(size: (u32, u32), factor: u32) -> Rect {
    let (lw, lh) = level_size(size, factor);
    Rect::new(0, 0, lw, lh)
}

/// The factor of a pyramid's coarsest level: the smallest power of two that brings the
/// picture's long side down to [`SMALLEST_LEVEL`].
fn coarsest_factor((w, h): (u32, u32)) -> u32 {
    let mut factor = 1;
    while w.max(h) / factor > SMALLEST_LEVEL {
        factor *= 2;
    }
    factor
}

/// `rect` of `image`.
fn crop(image: &ColorImage, rect: Rect) -> ColorImage {
    let width = image.size[0];
    let pixels = (rect.y..rect.bottom())
        .flat_map(|y| {
            let start = y as usize * width + rect.x as usize;
            image.pixels[start..start + rect.width as usize]
                .iter()
                .copied()
        })
        .collect();
    ColorImage::new([rect.width as usize, rect.height as usize], pixels)
}

/// `rect` of a level, `fill` given the first row and the texels of a run of rows. Renders that
/// read many picture pixels (`factor`² a texel) are split over the cores by rows; a brush dab's
/// stays on this thread.
fn render_rows(rect: Rect, factor: u32, fill: impl Fn(u32, &mut [Color32]) + Sync) -> ColorImage {
    let width = rect.width as usize;
    let mut out = vec![Color32::TRANSPARENT; width * rect.height as usize];
    let read = u64::from(rect.width) * u64::from(rect.height) * u64::from(factor).pow(2);
    gumka_core::by_rows(&mut out, width, read, |first, rows| {
        fill(rect.y + first as u32, rows);
    });
    ColorImage::new([width, rect.height as usize], out)
}

/// Averages `factor`-sized blocks of the picture into `rect` of a level and converts them for
/// the screen.
fn render_pixels(
    pixels: &Rgb16,
    display: &DisplayTransform,
    factor: u32,
    rect: Rect,
) -> ColorImage {
    let width = rect.width as usize;
    render_rows(rect, factor, |first, rows| {
        let mut stored = vec![[0u16; 3]; width];
        let mut shown = vec![[0u8; 3]; width];
        for (r, row) in rows.chunks_mut(width).enumerate() {
            average_row(pixels, factor, first + r as u32, rect, &mut stored);
            display.convert(&stored, &mut shown);
            for (out, &[r, g, b]) in row.iter_mut().zip(&shown) {
                *out = Color32::from_rgb(r, g, b);
            }
        }
    })
}

/// Row `ly` of a level, from `rect.x` across `rect.width`: each value the average of a
/// `factor`-sized block of the picture.
fn average_row(pixels: &Rgb16, factor: u32, ly: u32, rect: Rect, out: &mut [[u16; 3]]) {
    let (w, h) = pixels.dimensions();
    let (y0, y1) = (ly * factor, ((ly + 1) * factor).min(h));
    for (lx, out) in (rect.x..rect.right()).zip(out) {
        let (x0, x1) = (lx * factor, ((lx + 1) * factor).min(w));
        *out = if factor == 1 {
            pixels.get_pixel(x0, y0).0
        } else {
            let mut sum = [0u64; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = pixels.get_pixel(x, y).0;
                    (0..3).for_each(|c| sum[c] += u64::from(p[c]));
                }
            }
            let n = u64::from((x1 - x0) * (y1 - y0));
            sum.map(|s| ((s + n / 2) / n) as u16)
        };
    }
}

/// Mask coverage of `factor`-sized blocks as a translucent overlay.
fn render_mask(mask: &Mask, factor: u32, rect: Rect) -> ColorImage {
    let (w, h) = mask.dimensions();
    let width = rect.width as usize;
    render_rows(rect, factor, |first, rows| {
        for (r, row) in rows.chunks_mut(width).enumerate() {
            let ly = first + r as u32;
            let (y0, y1) = (ly * factor, ((ly + 1) * factor).min(h));
            for (out, lx) in row.iter_mut().zip(rect.x..rect.right()) {
                let (x0, x1) = (lx * factor, ((lx + 1) * factor).min(w));
                let mut covered = 0u32;
                for y in y0..y1 {
                    for x in x0..x1 {
                        covered += u32::from(mask.get_pixel(x, y).0[0] != 0);
                    }
                }
                let coverage = covered as f32 / ((x1 - x0) * (y1 - y0)) as f32;
                let alpha = (coverage * MASK_OPACITY * 255.0).round() as u8;
                let [r, g, b] = MASK_RGB;
                *out = Color32::from_rgba_unmultiplied(r, g, b, alpha);
            }
        }
    })
}

/// What a primary drag on the picture does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorTool {
    Brush,
    Remove,
    /// Click an object to select it (SAM).
    Select,
    /// Only moves the view: tools that work on the whole photo have nothing to paint.
    Pan,
}

/// How the canvas behaves this frame.
pub struct ShowOptions<'a> {
    pub tool: CursorTool,
    pub can_paint: bool,
    pub show_mask: bool,
    /// Show the picture as it was opened.
    pub compare: bool,
    /// Clicks of the current selection, in stored pixels, drawn as markers.
    pub clicks: &'a [Click],
}

pub struct Canvas {
    /// For making the textures anew when the picture changes size.
    ctx: egui::Context,
    /// From the picture's color profile to the screen's.
    display: DisplayTransform,
    /// Stored pixel size; textures and the mask use stored coordinates.
    size: (u32, u32),
    /// EXIF orientation (1-8) applied when drawing, and its viewed size.
    orientation: u8,
    viewed: (u32, u32),
    /// A tile's side in texels, for the picture's and the mask's pyramids.
    tile: u32,
    pixels: Pyramid,
    /// The picture as opened, for Before / After; at the opened size even after an upscale.
    /// Made in full when opened, so up to the GPU's texture limit.
    original: Pyramid,
    mask: Pyramid,
    /// Physical screen pixels per picture pixel.
    zoom: f32,
    /// Screen position (points) of the picture's top-left corner, relative to the canvas.
    offset: Vec2,
    /// Fit to the window every frame (so it follows resizes) until the user pans or zooms.
    fitted: bool,
    /// Brush radius in picture pixels.
    pub brush_radius: f32,
    stroke: Option<Stroke>,
    /// What the primary drag under way does, decided when it started.
    drag: Drag,
    /// Where the canvas was last shown, and screen points per physical pixel then.
    viewport: ScreenRect,
    points_per_pixel: f32,
}

/// What a primary drag does. It keeps the role it started with: pressing or releasing Space,
/// or painting becoming possible or impossible, halfway through doesn't change it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    /// Started with Space held, or in a tool that only pans.
    Pan,
    /// Started where the brush could paint.
    Paint,
    /// Started where it could do neither, say while an erase was running.
    Neither,
}

/// A brush stroke in progress.
struct Stroke {
    before: Mask,
    last: Pos2,
    /// The view `last` was taken in. Points are joined only within one view: a resize (which
    /// refits the picture) mid-stroke moves the picture under the pointer, and a line across
    /// that would be one nobody brushed.
    view: View,
    erase: bool,
    rect: Rect,
}

/// Where the picture is on screen: zoom, offset, the canvas's corner and its pixel density.
#[derive(Clone, Copy, PartialEq)]
struct View(f32, Vec2, Pos2, f32);

/// A finished brush stroke: the mask before it, the region it touched, and whether it
/// removed from the mask.
pub struct StrokeDone {
    pub before: Mask,
    pub rect: Rect,
    pub removed: bool,
}

/// What the canvas did this frame.
#[derive(Default)]
pub struct CanvasEvents {
    pub stroke_done: Option<StrokeDone>,
    /// A click with the Select tool, in stored pixels; Option/Alt excludes.
    pub click: Option<Click>,
}

impl Canvas {
    /// `icc` is the picture's color profile; without one it is shown as sRGB.
    pub fn new(
        ctx: &egui::Context,
        pixels: &Rgb16,
        mask: &Mask,
        orientation: u8,
        icc: Option<&[u8]>,
    ) -> Self {
        // A tile's texture has an apron of a texel on every side, and must fit the GPU.
        let max_side = ctx.input(|i| i.max_texture_side).max(1) as u32;
        let tile = max_side.min(LARGEST_TEXTURE).saturating_sub(2).max(1);
        Self::with_tiles(ctx, pixels, mask, orientation, icc, tile)
    }

    /// [`Self::new`], with tiles of `tile` texels.
    fn with_tiles(
        ctx: &egui::Context,
        pixels: &Rgb16,
        mask: &Mask,
        orientation: u8,
        icc: Option<&[u8]>,
        tile: u32,
    ) -> Self {
        let size = pixels.dimensions();
        let brush_radius = default_brush(size);
        let display = DisplayTransform::new(icc, crate::display::SCREEN);
        let max_side = ctx.input(|i| i.max_texture_side).max(1) as u32;
        let coarse = coarsest_factor(size);
        let first = render_pixels(pixels, &display, coarse, level_rect(size, coarse));
        // Before / After shares the picture's coarsest level.
        let original = Pyramid::eager(ctx, "original", size, max_side, |f, r| {
            if f == coarse {
                first.clone()
            } else {
                render_pixels(pixels, &display, f, r)
            }
        });
        let mask = render_mask(mask, coarse, level_rect(size, coarse));
        Self {
            ctx: ctx.clone(),
            display,
            size,
            orientation,
            viewed: oriented_size(size, orientation),
            tile,
            pixels: Pyramid::lazy(ctx, "picture", size, tile, first),
            original,
            mask: Pyramid::lazy(ctx, "mask", size, tile, mask),
            zoom: 1.0,
            offset: Vec2::ZERO,
            fitted: true,
            brush_radius,
            stroke: None,
            drag: Drag::Neither,
            viewport: ScreenRect::NOTHING,
            points_per_pixel: 1.0,
        }
    }

    /// Whether the picture is shown from its own color profile, rather than as sRGB.
    pub fn uses_own_profile(&self) -> bool {
        self.display.uses_own_profile()
    }

    /// A brush stroke is being painted.
    pub fn stroking(&self) -> bool {
        self.stroke.is_some()
    }

    /// Drops input that would cut a stroke short: egui ends every drag on Escape. Call it on
    /// the frame's raw input, before egui sees it.
    pub fn filter_input(&self, raw: &mut egui::RawInput) {
        if self.stroking() {
            raw.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        ..
                    }
                )
            });
        }
    }

    pub fn reset_brush(&mut self) {
        self.brush_radius = default_brush(self.size);
    }

    pub fn update_pixels(&mut self, pixels: &Rgb16, rect: Rect) {
        self.pixels
            .update(rect, |f, r| render_pixels(pixels, &self.display, f, r));
    }

    pub fn update_mask(&mut self, mask: &Mask, rect: Rect) {
        self.mask.update(rect, |f, r| render_mask(mask, f, r));
    }

    /// Shows `pixels` and `mask` at a new size (an upscale, or its undo). The picture keeps its
    /// place and size on screen, the brush covers the same part of it, and Before / After still
    /// shows the picture as opened. Only the coarsest levels are made now; the rest as they
    /// come into view.
    pub fn resize(&mut self, pixels: &Rgb16, mask: &Mask) {
        let size = pixels.dimensions();
        let ratio = size.0 as f32 / self.size.0.max(1) as f32;
        let coarse = coarsest_factor(size);
        let level = level_rect(size, coarse);
        let image = render_pixels(pixels, &self.display, coarse, level);
        self.pixels = Pyramid::lazy(&self.ctx, "picture", size, self.tile, image);
        let image = render_mask(mask, coarse, level);
        self.mask = Pyramid::lazy(&self.ctx, "mask", size, self.tile, image);
        self.size = size;
        self.viewed = oriented_size(size, self.orientation);
        self.zoom /= ratio;
        self.brush_radius = (self.brush_radius * ratio).clamp(1.0, 2000.0);
        self.stroke = None;
    }

    pub fn zoom_percent(&self) -> f32 {
        self.zoom * 100.0
    }

    pub fn fit(&mut self) {
        self.fitted = true;
    }

    /// 100%: one picture pixel per screen pixel, keeping the center in place.
    pub fn actual_pixels(&mut self) {
        let (canvas, ppp) = (self.viewport, self.points_per_pixel);
        self.zoom_about(canvas.center(), canvas, 1.0, ppp);
    }

    pub fn zoom_by(&mut self, factor: f32) {
        let (canvas, ppp) = (self.viewport, self.points_per_pixel);
        self.zoom_about(canvas.center(), canvas, self.zoom * factor, ppp);
    }

    fn zoom_about(&mut self, anchor: Pos2, canvas: ScreenRect, zoom: f32, points_per_pixel: f32) {
        let zoom = zoom.clamp(0.01, 64.0);
        self.fitted = false;
        let picture = self.to_picture(anchor, canvas, points_per_pixel);
        self.zoom = zoom;
        // Keep the picture point under `anchor` where it was.
        self.offset = (anchor - canvas.min) - picture.to_vec2() * self.scale(points_per_pixel);
    }

    /// Screen points per picture pixel.
    fn scale(&self, points_per_pixel: f32) -> f32 {
        self.zoom * points_per_pixel
    }

    fn to_picture(&self, screen: Pos2, canvas: ScreenRect, points_per_pixel: f32) -> Pos2 {
        ((screen - canvas.min - self.offset) / self.scale(points_per_pixel)).to_pos2()
    }

    /// A point of the viewed picture (from [`Self::to_picture`]) in stored pixel coordinates.
    fn to_stored(&self, viewed: Pos2) -> Pos2 {
        let (vw, vh) = (self.viewed.0 as f32, self.viewed.1 as f32);
        let (s, t) = viewed_to_stored(self.orientation, viewed.x / vw, viewed.y / vh);
        pos2(s * self.size.0 as f32, t * self.size.1 as f32)
    }

    fn picture_rect(&self, canvas: ScreenRect, points_per_pixel: f32) -> ScreenRect {
        let size = vec2(self.viewed.0 as f32, self.viewed.1 as f32) * self.scale(points_per_pixel);
        ScreenRect::from_min_size(canvas.min + self.offset, size)
    }

    /// Where `pyramid` is on screen when the picture is `shown`, drawn within `clip`.
    fn place(&self, pyramid: &Pyramid, shown: ScreenRect, clip: ScreenRect) -> Place {
        Place {
            orientation: self.orientation,
            size: (pyramid.size.0 as f32, pyramid.size.1 as f32),
            shown,
            clip,
        }
    }

    /// Shows the canvas filling `ui` and handles pan, zoom and painting. `pixels` are the
    /// picture's, to make the tiles that come into view; while they are away at the worker
    /// (0 × 0), none are made.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pixels: &Rgb16,
        mask: &mut Mask,
        options: &ShowOptions,
    ) -> CanvasEvents {
        let can_click = options.can_paint && options.tool == CursorTool::Select && !options.compare;
        let can_paint = options.can_paint
            && matches!(options.tool, CursorTool::Brush | CursorTool::Remove)
            && !options.compare;
        let mut events = CanvasEvents::default();
        let (canvas, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let ppp = 1.0 / ui.ctx().pixels_per_point();
        self.viewport = canvas;
        self.points_per_pixel = ppp;

        if self.fitted && canvas.width() > 0.0 {
            let (w, h) = (self.viewed.0 as f32, self.viewed.1 as f32);
            let fit_points = ((canvas.width() - 24.0) / w).min((canvas.height() - 24.0) / h);
            self.zoom = (fit_points / ppp).min(1.0);
            let shown = vec2(w, h) * self.scale(ppp);
            self.offset = (canvas.size() - shown) / 2.0;
        }

        let (pan_key, alt) = ui.input(|i| (i.key_down(egui::Key::Space), i.modifiers.alt));
        let primary = egui::PointerButton::Primary;
        let pan_tool = pan_key || options.tool == CursorTool::Pan;
        if response.drag_started_by(primary) {
            self.drag = if pan_tool {
                Drag::Pan
            } else if can_paint {
                Drag::Paint
            } else {
                Drag::Neither
            };
        }
        let dragging = response.dragged_by(primary);
        let pan_tool = if dragging {
            self.drag == Drag::Pan
        } else {
            pan_tool
        };
        let panning = response.dragged_by(egui::PointerButton::Middle)
            || response.dragged_by(egui::PointerButton::Secondary)
            || (pan_tool && response.dragged_by(primary));

        // The view holds still while a stroke is painted: the picture moving under a held brush
        // would leave a trail nobody brushed.
        if response.hovered() && !self.stroking() {
            let (scroll, zoom, pointer) =
                ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
            if zoom != 1.0 {
                if let Some(pointer) = pointer {
                    self.zoom_about(pointer, canvas, self.zoom * zoom, ppp);
                }
            } else if scroll != Vec2::ZERO {
                self.offset += scroll;
                self.fitted = false;
            }
        }
        if panning && response.dragged() {
            self.offset += response.drag_delta();
            self.fitted = false;
        }

        // Painting: a primary click, or a drag that started as a stroke; Option/Alt removes
        // from the mask. A drag only registers after a few points of movement, so a stroke
        // starts at the press origin.
        let painting = can_paint
            && if dragging {
                self.drag == Drag::Paint
            } else {
                !pan_tool && response.clicked_by(primary)
            };
        let pos = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos())
            .map(|p| self.to_stored(self.to_picture(p, canvas, ppp)));
        let view = View(self.zoom, self.offset, canvas.min, ppp);
        if let Some(pos) = pos {
            if painting {
                let starts = response.drag_started_by(primary) || response.clicked_by(primary);
                let origin = ui
                    .input(|i| i.pointer.press_origin())
                    .filter(|_| starts)
                    .map(|p| self.to_stored(self.to_picture(p, canvas, ppp)));
                let stroke = self.stroke.get_or_insert_with(|| Stroke {
                    before: mask.clone(),
                    last: origin.unwrap_or(pos),
                    view,
                    erase: (options.tool == CursorTool::Remove) != alt,
                    rect: Rect::new(0, 0, 0, 0),
                });
                if stroke.view != view {
                    (stroke.last, stroke.view) = (pos, view);
                }
                let value = if stroke.erase { 0 } else { 255 };
                let dirty = paint_line(mask, stroke.last, pos, self.brush_radius, value);
                stroke.last = pos;
                stroke.rect = stroke.rect.union(&dirty);
                if !dirty.is_empty() {
                    self.update_mask(mask, dirty);
                }
            } else if let Some(stroke) = &mut self.stroke {
                // Paused, say while Before / After is held: it picks up again from wherever
                // the pointer is then, not with a line from where it stopped.
                (stroke.last, stroke.view) = (pos, view);
            }
        }
        if can_click
            && !pan_tool
            && response.clicked_by(egui::PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let at = self.to_stored(self.to_picture(pos, canvas, ppp));
            let (w, h) = (self.size.0 as f32, self.size.1 as f32);
            if (0.0..w).contains(&at.x) && (0.0..h).contains(&at.y) {
                events.click = Some(Click {
                    x: at.x,
                    y: at.y,
                    include: !alt,
                });
            }
        }
        if !response.dragged()
            && let Some(stroke) = self.stroke.take()
            && !stroke.rect.is_empty()
        {
            events.stroke_done = Some(StrokeDone {
                before: stroke.before,
                rect: stroke.rect,
                removed: stroke.erase,
            });
        }

        // Draw.
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 0.0, BG_VIEWER);
        let shown = self.picture_rect(canvas, ppp);
        let shadow = egui::epaint::Shadow {
            offset: [0, 10],
            blur: 34,
            spread: 0,
            color: Color32::from_black_alpha(115),
        };
        painter.add(shadow.as_shape(shown, CornerRadius::ZERO));
        // Tiles coming into view are made from the picture as it is now, so not while its
        // pixels are away at the worker (0 × 0).
        let show_mask = options.show_mask && !options.compare;
        if !options.compare {
            let mut budget = Budget::new();
            let mut left = false;
            if pixels.dimensions() == self.pixels.size {
                let (level, place) = (
                    self.pixels.pick(self.zoom),
                    self.place(&self.pixels, shown, canvas),
                );
                let display = &self.display;
                left |= self
                    .pixels
                    .make_visible(&self.ctx, level, &place, &mut budget, |f, r| {
                        render_pixels(pixels, display, f, r)
                    });
            }
            if show_mask && mask.dimensions() == self.mask.size {
                let (level, place) = (
                    self.mask.pick(self.zoom),
                    self.place(&self.mask, shown, canvas),
                );
                left |= self
                    .mask
                    .make_visible(&self.ctx, level, &place, &mut budget, |f, r| {
                        render_mask(mask, f, r)
                    });
            }
            if left {
                ui.ctx().request_repaint();
            }
        }
        let picture = if options.compare {
            &self.original
        } else {
            &self.pixels
        };
        let overlay = show_mask.then_some(&self.mask);
        for pyramid in std::iter::once(picture).chain(overlay) {
            // The original can be smaller than the picture now (after an upscale): its pixels
            // are larger on screen, by as much.
            let zoom = self.zoom * self.size.0 as f32 / pyramid.size.0.max(1) as f32;
            let place = self.place(pyramid, shown, canvas);
            for mesh in pyramid.meshes(pyramid.pick(zoom), &place) {
                painter.add(egui::Shape::mesh(mesh));
            }
        }

        // Brush outline instead of the pointer.
        if let Some(pos) = response.hover_pos() {
            if panning {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if pan_tool {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            } else if can_click {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            } else if can_paint {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                let radius = self.brush_radius * self.scale(ppp);
                painter.circle_stroke(
                    pos,
                    radius + 1.0,
                    LineStroke::new(1.5, Color32::from_black_alpha(140)),
                );
                painter.circle_stroke(pos, radius, LineStroke::new(1.0, Color32::WHITE));
            }
        }

        // Selection clicks: green includes, red excludes.
        if !options.compare {
            for click in options.clicks {
                let (u, v) = stored_to_viewed(
                    self.orientation,
                    click.x / self.size.0 as f32,
                    click.y / self.size.1 as f32,
                );
                let at = pos2(
                    shown.left() + u * shown.width(),
                    shown.top() + v * shown.height(),
                );
                let fill = if click.include {
                    OK
                } else {
                    Color32::from_rgb(MASK_RGB[0], MASK_RGB[1], MASK_RGB[2])
                };
                painter.circle_filled(at, 7.0, Color32::from_black_alpha(90));
                painter.circle(at, 6.0, fill, LineStroke::new(2.0, Color32::WHITE));
            }
        }

        if options.compare {
            badge(ui, shown.min + vec2(12.0, 12.0), "BEFORE");
        }
        hint_pill(
            ui,
            pos2(canvas.center().x, canvas.bottom() - 16.0),
            options.tool,
            self.brush_radius,
        );
        events
    }
}

/// A brush about 1/40 of the picture's long side.
fn default_brush((w, h): (u32, u32)) -> f32 {
    (w.max(h) as f32 / 80.0).clamp(4.0, 200.0)
}

/// A small label over the top-left of the picture.
fn badge(ui: &egui::Ui, at: Pos2, text: &str) {
    egui::Area::new(egui::Id::unique("gumka-badge"))
        .order(egui::Order::Middle)
        .fixed_pos(at)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::NONE
                .fill(Color32::from_black_alpha(210))
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).size(11.0).strong().color(TEXT_STRONG));
                });
        });
}

/// The floating hint at the bottom of the viewer: what the cursor does, and the keys.
fn hint_pill(ui: &egui::Ui, bottom_center: Pos2, tool: CursorTool, brush_radius: f32) {
    let (lead, keys): (String, &[(&str, &str)]) = match tool {
        // A tool for the whole photo has nothing to paint: only the view to move.
        CursorTool::Pan => (
            "Works on the whole photo".into(),
            &[("Space", "pan"), ("⌘1", "100%")],
        ),
        CursorTool::Select => (
            "Click an object to select it".into(),
            &[("⌥", "click excludes"), ("B", "brush"), ("Space", "pan")],
        ),
        CursorTool::Remove => (
            format!(
                "Paint to remove from the mask · {:.0} px",
                brush_radius * 2.0
            ),
            &[("⌥", "adds"), ("[ ]", "size"), ("Space", "pan")],
        ),
        CursorTool::Brush => (
            format!("Paint over what to remove · {:.0} px", brush_radius * 2.0),
            &[("⌥", "removes"), ("[ ]", "size"), ("Space", "pan")],
        ),
    };
    egui::Area::new(egui::Id::unique("gumka-hint-pill"))
        .order(egui::Order::Middle)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(bottom_center)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::NONE
                .fill(Color32::from_rgba_unmultiplied(26, 26, 26, 230))
                .stroke(LineStroke::new(1.0, Color32::from_gray(0x3c)))
                .corner_radius(CornerRadius::same(255))
                .inner_margin(Margin::symmetric(14, 5))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(lead).size(12.0).color(TEXT));
                        for (key, text) in keys {
                            ui.add_space(8.0);
                            widgets::key_cap(ui, key);
                            ui.label(
                                RichText::new(*text)
                                    .size(12.0)
                                    .color(Color32::from_gray(0xcf)),
                            );
                        }
                    });
                });
        });
}

/// Paints a round brush from `a` to `b` (picture coordinates). Returns the touched region.
fn paint_line(mask: &mut Mask, a: Pos2, b: Pos2, radius: f32, value: u8) -> Rect {
    let step = (radius * 0.25).max(0.5);
    let steps = ((b - a).length() / step).ceil().max(1.0) as u32;
    let mut dirty = Rect::new(0, 0, 0, 0);
    for i in 0..=steps {
        let p = a + (b - a) * (i as f32 / steps as f32);
        dirty = dirty.union(&paint_dab(mask, p, radius, value));
    }
    dirty
}

fn paint_dab(mask: &mut Mask, center: Pos2, radius: f32, value: u8) -> Rect {
    let (w, h) = (mask.width() as f32, mask.height() as f32);
    let x0 = (center.x - radius).floor().clamp(0.0, w) as u32;
    let x1 = (center.x + radius).ceil().clamp(0.0, w) as u32;
    let y0 = (center.y - radius).floor().clamp(0.0, h) as u32;
    let y1 = (center.y + radius).ceil().clamp(0.0, h) as u32;
    let r2 = radius * radius;
    for y in y0..y1 {
        let dy = y as f32 + 0.5 - center.y;
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - center.x;
            if dx * dx + dy * dy <= r2 {
                mask.put_pixel(x, y, gumka_core::image::Luma([value]));
            }
        }
    }
    if x1 > x0 && y1 > y0 {
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    } else {
        Rect::new(0, 0, 0, 0)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eframe::egui::{Event, Key, Modifiers, PointerButton, RawInput};

    /// The finest level of Before / After, which is made in full when the picture opens: 1
    /// shows every picture pixel.
    pub(crate) fn original_finest_factor(canvas: &Canvas) -> u32 {
        canvas.original.levels[0].factor
    }

    /// The tiles of `level` that are made, by column and row.
    fn made(pyramid: &Pyramid, level: usize) -> Vec<(u32, u32)> {
        (0..pyramid.levels[level].tiles.len())
            .filter(|&i| pyramid.levels[level].tiles[i].is_some())
            .map(|i| {
                let r = pyramid.tile_rect(level, i);
                (r.x / pyramid.tile, r.y / pyramid.tile)
            })
            .collect()
    }

    /// A 200 x 200 picture on a 400 x 400 canvas: shown at 100%, its top-left corner at
    /// (100, 100) on screen.
    struct Viewer {
        ctx: egui::Context,
        canvas: Canvas,
        pixels: Rgb16,
        mask: Mask,
        tool: CursorTool,
        /// As the app sets them: off while an erase runs or a dialog is open; Before / After.
        can_paint: bool,
        compare: bool,
        time: f64,
        strokes: Vec<StrokeDone>,
    }

    impl Viewer {
        fn new(tool: CursorTool) -> Self {
            Self::with(tool, (200, 200), 1, LARGEST_TEXTURE - 2)
        }

        /// A picture of `size` (stored) with an `orientation`, in tiles of `tile` texels.
        fn with(tool: CursorTool, (w, h): (u32, u32), orientation: u8, tile: u32) -> Self {
            let ctx = egui::Context::default();
            let mask = Mask::new(w, h);
            let pixels =
                Rgb16::from_fn(w, h, |x, y| gumka_core::image::Rgb([x as u16, y as u16, 7]));
            let canvas = Canvas::with_tiles(&ctx, &pixels, &mask, orientation, None, tile);
            Self {
                ctx,
                canvas,
                pixels,
                mask,
                tool,
                can_paint: true,
                compare: false,
                time: 0.0,
                strokes: Vec::new(),
            }
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.time += 1.0 / 60.0;
            let mut input = RawInput {
                screen_rect: Some(ScreenRect::from_min_size(Pos2::ZERO, vec2(400.0, 400.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            // As the app does in `raw_input_hook`.
            self.canvas.filter_input(&mut input);
            let (canvas, pixels, mask, tool, strokes) = (
                &mut self.canvas,
                &self.pixels,
                &mut self.mask,
                self.tool,
                &mut self.strokes,
            );
            let (can_paint, compare) = (self.can_paint, self.compare);
            self.ctx
                .run_ui(input, |ui| {
                    let options = ShowOptions {
                        tool,
                        can_paint,
                        show_mask: true,
                        compare,
                        clicks: &[],
                    };
                    strokes.extend(canvas.show(ui, pixels, mask, &options).stroke_done);
                })
                .drop_without_applying_deltas();
        }

        fn painted(&self) -> usize {
            self.mask.pixels().filter(|p| p.0[0] != 0).count()
        }
    }

    /// After an upscale the picture keeps its place and size on screen, and the brush its
    /// share of the picture; Before / After still shows the picture as opened.
    #[test]
    fn a_resized_picture_keeps_its_place_on_screen() {
        let mut v = Viewer::new(CursorTool::Brush);
        v.frame(vec![]);
        v.canvas.actual_pixels();
        v.frame(vec![]);
        let (viewport, ppp) = (v.canvas.viewport, v.canvas.points_per_pixel);
        let shown = v.canvas.picture_rect(viewport, ppp);
        let (zoom, brush) = (v.canvas.zoom_percent(), v.canvas.brush_radius);

        (v.pixels, v.mask) = (Rgb16::new(400, 400), Mask::new(400, 400));
        v.canvas.resize(&v.pixels, &v.mask);
        assert_eq!(v.canvas.zoom_percent(), zoom / 2.0);
        assert_eq!(v.canvas.picture_rect(viewport, ppp), shown);
        assert_eq!(v.canvas.brush_radius, brush * 2.0);
        assert_eq!(
            (v.canvas.pixels.size, v.canvas.original.size),
            ((400, 400), (200, 200))
        );

        v.compare = true;
        v.frame(vec![]);
        v.compare = false;
        v.frame(vec![]);
        assert_eq!(
            v.canvas.picture_rect(viewport, ppp),
            shown,
            "and stays there"
        );
    }

    fn space(pressed: bool) -> Event {
        Event::Key {
            key: Key::Space,
            physical_key: Some(Key::Space),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn button(x: f32, y: f32, pressed: bool) -> Event {
        Event::PointerButton {
            pos: pos2(x, y),
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn moved(x: f32, y: f32) -> Event {
        Event::PointerMoved(pos2(x, y))
    }

    #[test]
    fn a_drag_that_starts_as_a_pan_never_paints() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0), space(true)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        let panned = viewer.canvas.offset;
        assert_eq!(panned, vec2(130.0, 100.0), "the drag pans");
        // Space comes up halfway through the drag.
        viewer.frame(vec![space(false), moved(210.0, 160.0)]);
        viewer.frame(vec![moved(240.0, 170.0)]);
        viewer.frame(vec![button(240.0, 170.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.painted(), 0, "nothing was brushed");
        assert!(viewer.strokes.is_empty());
        assert_eq!(viewer.canvas.offset, vec2(190.0, 120.0), "it kept panning");
    }

    #[test]
    fn a_stroke_keeps_painting_when_space_goes_down() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        viewer.frame(vec![space(true), moved(210.0, 150.0)]);
        viewer.frame(vec![moved(240.0, 150.0)]);
        viewer.frame(vec![button(240.0, 150.0, false)]);
        viewer.frame(vec![space(false)]);
        assert_eq!(viewer.canvas.offset, vec2(100.0, 100.0), "no pan");
        assert_eq!(viewer.strokes.len(), 1, "one stroke");
        // The stroke runs from picture (50, 50) to (140, 50).
        for x in [50, 90, 120, 140] {
            assert_eq!(viewer.mask.get_pixel(x, 50).0[0], 255, "painted at x = {x}");
        }
        assert_eq!(viewer.mask.get_pixel(50, 150).0[0], 0);
    }

    #[test]
    fn a_new_drag_after_a_pan_paints_again() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0), space(true)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        viewer.frame(vec![button(180.0, 150.0, false), space(false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.painted(), 0);
        // Picture point (50, 100) is now at screen (180, 200).
        viewer.frame(vec![moved(180.0, 200.0)]);
        viewer.frame(vec![button(180.0, 200.0, true)]);
        viewer.frame(vec![moved(200.0, 200.0)]);
        viewer.frame(vec![button(200.0, 200.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.strokes.len(), 1);
        assert_eq!(viewer.mask.get_pixel(50, 100).0[0], 255);
        assert_eq!(viewer.mask.get_pixel(70, 100).0[0], 255);
    }

    /// A drag that starts while the brush can't paint (an erase running, say) doesn't start a
    /// stroke when it becomes possible halfway: that would be a line from where it began.
    #[test]
    fn a_drag_that_starts_where_it_cant_paint_never_paints() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.can_paint = false;
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        viewer.can_paint = true;
        viewer.frame(vec![moved(210.0, 160.0)]);
        viewer.frame(vec![moved(240.0, 170.0)]);
        viewer.frame(vec![button(240.0, 170.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.painted(), 0);
        assert!(viewer.strokes.is_empty());
    }

    /// Holding Before / After pauses a stroke; it resumes from where the pointer is then.
    #[test]
    fn a_paused_stroke_resumes_without_a_line() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        viewer.compare = true;
        viewer.frame(vec![moved(180.0, 250.0)]);
        viewer.frame(vec![moved(230.0, 250.0)]);
        viewer.compare = false;
        viewer.frame(vec![moved(240.0, 250.0)]);
        viewer.frame(vec![button(240.0, 250.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.strokes.len(), 1, "still one stroke");
        let at = |x, y| viewer.mask.get_pixel(x, y).0[0];
        assert_eq!((at(50, 50), at(80, 50)), (255, 255), "before the pause");
        assert_eq!(at(135, 150), 255, "after the pause");
        assert_eq!(at(80, 100), 0, "no line down to where it resumed");
        // The line it would draw runs from picture (80, 50) to (140, 150).
        assert_eq!(at(110, 100), 0, "no line from where it paused");
        assert_eq!(
            at(100, 150),
            0,
            "nor along the way the pointer went meanwhile"
        );
    }

    /// ⌘+ (or a scroll, or a resize) mid-stroke moves the picture under the pointer; the
    /// stroke goes on from there rather than draw a line between the two picture points.
    #[test]
    fn zooming_mid_stroke_draws_no_line() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        // The picture doubles about the canvas center: the pointer is over (91, 75) next.
        viewer.canvas.zoom_by(2.0);
        viewer.frame(vec![moved(182.0, 150.0)]);
        viewer.frame(vec![button(182.0, 150.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.strokes.len(), 1, "one stroke");
        let at = |x, y| viewer.mask.get_pixel(x, y).0[0];
        assert_eq!((at(50, 50), at(80, 50)), (255, 255), "before the zoom");
        assert_eq!(at(91, 75), 255, "after it");
        assert_eq!(at(85, 62), 0, "nothing between");
    }

    /// Escape mid-stroke: egui aborts every drag on Escape, so the stroke ends there although
    /// the button is still down, and the rest of the drag paints nothing (then ⌘Z, still
    /// mid-drag, undoes the cut-short stroke).
    #[test]
    fn escape_mid_stroke_doesnt_end_the_stroke() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        viewer.frame(vec![Event::Key {
            key: Key::Escape,
            physical_key: Some(Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]);
        assert!(viewer.canvas.stroking(), "still painting");
        viewer.frame(vec![moved(240.0, 150.0)]);
        viewer.frame(vec![button(240.0, 150.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.strokes.len(), 1);
        assert_eq!(
            viewer.mask.get_pixel(130, 50).0[0],
            255,
            "painted to the end"
        );
    }

    /// The view holds still while a stroke is painted: scrolling would move the picture under
    /// the held brush and leave a trail.
    #[test]
    fn scrolling_mid_stroke_leaves_the_view_alone() {
        let mut viewer = Viewer::new(CursorTool::Brush);
        viewer.frame(vec![moved(150.0, 150.0)]);
        viewer.frame(vec![button(150.0, 150.0, true)]);
        viewer.frame(vec![moved(180.0, 150.0)]);
        let offset = viewer.canvas.offset;
        for _ in 0..10 {
            viewer.frame(vec![Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, 20.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Unknown,
                modifiers: Modifiers::NONE,
            }]);
        }
        assert_eq!(viewer.canvas.offset, offset, "no scroll");
        viewer.frame(vec![button(180.0, 150.0, false)]);
        viewer.frame(vec![]);
        assert_eq!(viewer.strokes.len(), 1);
        assert!(
            (10..46).all(|y| viewer.mask.get_pixel(80, y).0[0] == 0),
            "no trail"
        );
    }

    /// Why the app opens the command-line file on its first frame: before then egui assumes
    /// textures of at most 2048 pixels, and Before / After, made in full when the picture
    /// opens, would show a quarter of a wide picture. The picture itself is in tiles, so it
    /// has every pixel either way.
    #[test]
    fn previews_use_the_texture_limit_the_frame_reports() {
        let ctx = egui::Context::default();
        let (pixels, mask) = (Rgb16::new(5000, 8), Mask::new(5000, 8));
        let early = Canvas::new(&ctx, &pixels, &mask, 1, None);
        assert_eq!(original_finest_factor(&early), 4);
        assert_eq!(early.pixels.levels[0].factor, 1, "beyond the texture limit");
        // 2046 texels and an apron on either side: 2048, the limit.
        assert_eq!((early.tile, early.pixels.levels[0].tiles.len()), (2046, 3));
        let mut canvas = None;
        let input = RawInput {
            max_texture_side: Some(16384),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            canvas = Some(Canvas::new(ui.ctx(), &pixels, &mask, 1, None));
        })
        .drop_without_applying_deltas();
        let canvas = canvas.unwrap();
        assert_eq!(original_finest_factor(&canvas), 1);
        assert_eq!(canvas.tile, LARGEST_TEXTURE - 2);
    }

    /// Runs frames until the canvas has made every tile in view.
    fn settle(v: &mut Viewer) {
        for _ in 0..20 {
            v.frame(vec![]);
        }
    }

    /// A 1000 x 600 picture in tiles of 128 at 100%: the canvas shows picture pixels 300 to
    /// 700 across and 100 to 500 down, and only the tiles there are made. Shown whole first
    /// (at 1/2), so that level is made in full, however many tiles a frame's budget allows.
    fn zoomed_in() -> Viewer {
        let mut v = Viewer::with(CursorTool::Brush, (1000, 600), 1, 128);
        settle(&mut v);
        v.canvas.actual_pixels();
        settle(&mut v);
        v
    }

    #[test]
    fn only_the_tiles_in_view_are_made() {
        let v = zoomed_in();
        let levels = v.canvas.pixels.levels.len();
        assert_eq!(v.canvas.pixels.levels[0].factor, 1);
        let mut expected: Vec<(u32, u32)> =
            (0..=3).flat_map(|y| (2..=5).map(move |x| (x, y))).collect();
        expected.sort_unstable();
        let mut tiles = made(&v.canvas.pixels, 0);
        tiles.sort_unstable();
        assert_eq!(tiles, expected, "of 8 x 5");
        assert_eq!(made(&v.canvas.mask, 0).len(), 16, "the mask's too");
        let coarsest = &v.canvas.pixels.levels[levels - 1];
        assert!(coarsest.tiles.iter().all(Option::is_some));
    }

    /// An edit redraws the tiles made so far where it falls; the others stay unmade, to be made
    /// from the picture as it is then.
    #[test]
    fn an_update_remakes_only_tiles_already_made() {
        let mut v = zoomed_in();
        let rect = Rect::new(250, 0, 200, 50);
        v.ctx.tex_manager().write().take_delta().clear();
        v.canvas.update_pixels(&v.pixels, rect);
        let set = taken_patches(&v.ctx);
        let pyramid = &v.canvas.pixels;
        let mut remade = 0;
        for (l, level) in pyramid.levels.iter().enumerate() {
            let f = level.factor;
            for (i, tile) in level.tiles.iter().enumerate() {
                // The texture, apron included, is what an edit lands in.
                let t = pyramid.texture_rect(l, i);
                let hit = rect.x / f < t.right()
                    && rect.right().div_ceil(f) > t.x
                    && rect.y / f < t.bottom()
                    && rect.bottom().div_ceil(f) > t.y;
                if let Some(texture) = tile {
                    let set = set.get(&texture.id());
                    assert_eq!(set.is_some(), hit, "level {l} tile {i}");
                    assert!(set.is_none_or(|s| s.iter().all(|d| d.pos.is_some())));
                    remade += usize::from(hit);
                }
            }
        }
        // Tile (1, 0) is in the rect, but out of view.
        assert!(pyramid.levels[0].tiles[1].is_none(), "still unmade");
        assert_eq!(remade, 5, "two at full size, two at 1/2 and one at 1/4");
        assert_eq!(set.len(), remade, "and nothing else");
    }

    /// The texture changes made since the last frame, taken out of egui. The delta is emptied
    /// before anything is asserted: one dropped with changes in it panics, and that second
    /// panic, while a failed assertion unwinds, would abort every test in the binary.
    fn taken_patches(
        ctx: &egui::Context,
    ) -> std::collections::HashMap<egui::TextureId, Vec<egui::epaint::ImageDelta>> {
        let mut delta = ctx.tex_manager().write().take_delta();
        let set = delta
            .set
            .drain()
            .map(|(id, deltas)| (id, deltas.into_vec()))
            .collect();
        delta.clear();
        set
    }

    /// An edit across tile borders lands in every made tile it touches, each patch at its own
    /// place in its tile: the tile then shows what a fresh render of the edited picture gives.
    #[test]
    fn an_update_across_tile_borders_lands_in_place() {
        let mut v = zoomed_in();
        let before = v.pixels.clone();
        // Across columns 1 to 3 and rows 0 and 1 at full size.
        let rect = Rect::new(250, 100, 200, 60);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let p = gumka_core::image::Rgb([60000, 20000 + x as u16 * 7, 9000 + y as u16]);
                v.pixels.put_pixel(x, y, p);
            }
        }
        v.ctx.tex_manager().write().take_delta().clear();
        v.canvas.update_pixels(&v.pixels, rect);
        let set = taken_patches(&v.ctx);
        let (pyramid, display) = (&v.canvas.pixels, &v.canvas.display);
        let mut checked = 0;
        for (l, level) in pyramid.levels.iter().enumerate() {
            let f = level.factor;
            for (i, tile) in level.tiles.iter().enumerate() {
                let Some(texture) = tile else { continue };
                let t = pyramid.texture_rect(l, i);
                let Some(patches) = set.get(&texture.id()) else {
                    continue;
                };
                // The tile as it was, with the patches laid over it.
                let mut shown = render_pixels(&before, display, f, t);
                for patch in patches {
                    let [px, py] = patch.pos.expect("a patch, not a whole new texture");
                    let egui::ImageData::Color(image) = &patch.image;
                    let [pw, ph] = image.size;
                    assert!(px + pw <= t.width as usize && py + ph <= t.height as usize);
                    for y in 0..ph {
                        let row = &image.pixels[y * pw..(y + 1) * pw];
                        let at = (py + y) * t.width as usize + px;
                        shown.pixels[at..at + pw].copy_from_slice(row);
                    }
                }
                let fresh = render_pixels(&v.pixels, display, f, t);
                assert!(shown.pixels == fresh.pixels, "level {l} tile {i}");
                checked += 1;
            }
        }
        // Columns 2 and 3 at full size, both in rows 0 and 1; two at 1/2; one at 1/4.
        assert_eq!(checked, 7);
    }

    /// At 100%, with no tile at full size made, checks that `source` draws the 4 x 4 tiles in
    /// view: quads that cover each once, textured from where that part is in `source`'s tile.
    fn drawn_from(v: &Viewer, source: usize) {
        let pyramid = &v.canvas.pixels;
        let (viewport, ppp) = (v.canvas.viewport, v.canvas.points_per_pixel);
        let shown = v.canvas.picture_rect(viewport, ppp);
        assert_eq!(
            (shown.min.round(), v.canvas.zoom, ppp),
            (pos2(-300.0, -100.0), 1.0, 1.0)
        );
        let place = v.canvas.place(pyramid, shown, viewport);
        let f = pyramid.levels[source].factor as f32;
        // In view at full size: columns 2 to 5 and rows 0 to 3, 128 x 128 each.
        let in_view = |[x0, y0, x1, y1]: [f32; 4]| {
            (2..=5).any(|c| {
                (0..=3).any(|r| {
                    let (left, top) = (c as f32 * 128.0, r as f32 * 128.0);
                    x0 >= left && y0 >= top && x1 <= left + 128.0 && y1 <= top + 128.0
                })
            })
        };
        let mut covered = 0.0;
        for mesh in pyramid.meshes(0, &place) {
            let ci = pyramid.levels[source]
                .tiles
                .iter()
                .position(|t| t.as_ref().is_some_and(|t| t.id() == mesh.texture_id))
                .expect("drawn from the source level");
            // The tile's texture, with its apron.
            let c = pyramid.texture_rect(source, ci);
            // Stored pixels are screen points here, from the picture's corner.
            let stored: Vec<Pos2> = mesh
                .vertices
                .iter()
                .map(|p| (p.pos - shown.min.to_vec2()).round())
                .collect();
            for (p, s) in mesh.vertices.iter().zip(&stored) {
                let uv = pos2(
                    (s.x / f - c.x as f32) / c.width as f32,
                    (s.y / f - c.y as f32) / c.height as f32,
                );
                assert!(
                    (p.uv - uv).length() < 1e-5,
                    "{:?} at {s:?}, not {uv:?}",
                    p.uv
                );
            }
            let (min, max) = stored
                .iter()
                .fold((stored[0], stored[0]), |(a, b), &p| (a.min(p), b.max(p)));
            assert!(in_view([min.x, min.y, max.x, max.y]), "{min:?} to {max:?}");
            covered += (max - min).x * (max - min).y;
        }
        assert_eq!(covered, 16.0 * 128.0 * 128.0, "each tile in view once");
    }

    /// Until a tile is made, the same part of the next coarser level that is made stands in
    /// for it: here only the coarsest is, as the pixels were away from the start.
    #[test]
    fn an_unmade_tile_is_drawn_from_the_coarsest_level_in_its_place() {
        let mut v = Viewer::with(CursorTool::Brush, (1000, 600), 1, 128);
        let _pixels = std::mem::take(&mut v.pixels);
        v.frame(vec![]);
        v.canvas.actual_pixels();
        settle(&mut v);
        let pyramid = &v.canvas.pixels;
        let coarsest = pyramid.levels.len() - 1;
        assert_eq!((pyramid.levels[coarsest].factor, coarsest), (4, 2));
        assert!(made(pyramid, 1).is_empty());
        drawn_from(&v, coarsest);
    }

    /// A made level between the one drawn and the coarsest stands in before the coarsest: the
    /// picture shown whole made 1/2, then the pixels went to the worker (an upscale, say) and
    /// it was zoomed to 100%, where no tile can be made (review round 2: it was 1/4 then).
    #[test]
    fn an_unmade_tile_is_drawn_from_a_made_middle_level() {
        let mut v = Viewer::with(CursorTool::Brush, (1000, 600), 1, 128);
        settle(&mut v);
        assert_eq!(made(&v.canvas.pixels, 1).len(), 12, "all of 1/2");
        let _pixels = std::mem::take(&mut v.pixels);
        v.canvas.actual_pixels();
        settle(&mut v);
        assert!(made(&v.canvas.pixels, 0).is_empty());
        drawn_from(&v, 1);
    }

    /// The level drawn has at least one texel per screen pixel at any zoom, and only its tiles
    /// in view are made: at 45% that is 1/2, and panned to the right, its last two columns.
    #[test]
    fn the_level_follows_the_zoom_and_only_its_tiles_in_view_are_made() {
        let mut v = Viewer::with(CursorTool::Brush, (1000, 600), 1, 128);
        let factor = |zoom: f32| {
            let pyramid = &v.canvas.pixels;
            pyramid.levels[pyramid.pick(zoom)].factor
        };
        let picked: Vec<u32> = [8.0, 1.0, 0.51, 0.5, 0.3, 0.26, 0.25, 0.1, 0.01]
            .into_iter()
            .map(factor)
            .collect();
        assert_eq!(picked, [1, 1, 1, 2, 2, 2, 4, 4, 4]);

        v.canvas.fitted = false;
        v.canvas.zoom = 0.45;
        // The picture's corner 300 points left of the canvas and 100 above: it shows stored
        // pixels from 667 across and from 222 down.
        v.canvas.offset = vec2(-300.0, -100.0);
        settle(&mut v);
        let pyramid = &v.canvas.pixels;
        assert_eq!(pyramid.levels[1].factor, 2);
        assert!(made(pyramid, 0).is_empty(), "nothing at full size");
        let mut tiles = made(pyramid, 1);
        tiles.sort_unstable();
        assert_eq!(tiles, [(2, 0), (2, 1), (2, 2), (3, 0), (3, 1), (3, 2)]);
    }

    /// Under every EXIF orientation, the tiles fill exactly the rectangle the viewed picture
    /// takes, with the stored picture's top corners where viewers show them.
    #[test]
    fn every_orientation_draws_the_tiles_in_place() {
        let (tl, tr, br, bl) = (0, 1, 2, 3);
        // Where the stored top-left and top-right corners are viewed.
        for (orientation, origin, right) in [
            (1, tl, tr),
            (2, tr, tl),
            (3, br, bl),
            (4, bl, br),
            (5, tl, bl),
            (6, tr, br),
            (7, br, tr),
            (8, bl, tl),
        ] {
            let mut v = Viewer::with(CursorTool::Brush, (300, 200), orientation, 128);
            settle(&mut v);
            let canvas = &v.canvas;
            let shown = canvas.picture_rect(canvas.viewport, canvas.points_per_pixel);
            let turned = orientation >= 5;
            let size = if turned {
                vec2(200.0, 300.0)
            } else {
                vec2(300.0, 200.0)
            };
            assert_eq!(shown.size(), size, "{orientation}");
            let corners = [
                shown.left_top(),
                shown.right_top(),
                shown.right_bottom(),
                shown.left_bottom(),
            ];
            let pyramid = &canvas.pixels;
            let place = canvas.place(pyramid, shown, canvas.viewport);
            let meshes = pyramid.meshes(0, &place);
            assert_eq!(meshes.len(), 6, "{orientation}: 3 x 2 tiles, all made");
            let mut area = 0.0;
            let mut bounds = ScreenRect::NOTHING;
            for mesh in &meshes {
                let r = ScreenRect::from_points(
                    &mesh.vertices.iter().map(|p| p.pos).collect::<Vec<_>>(),
                );
                area += r.area();
                bounds = bounds.union(r);
            }
            assert_eq!(
                (bounds, area),
                (shown, shown.area()),
                "{orientation}: no gap, no overlap"
            );
            let corner = |index: usize, uv: Pos2| {
                let id = pyramid.levels[0].tiles[index].as_ref().unwrap().id();
                let mesh = meshes.iter().find(|m| m.texture_id == id).unwrap();
                mesh.vertices.iter().find(|p| p.uv == uv).unwrap().pos
            };
            assert_eq!(
                corner(0, pos2(0.0, 0.0)),
                corners[origin],
                "{orientation}: top left"
            );
            assert_eq!(
                corner(2, pos2(1.0, 0.0)),
                corners[right],
                "{orientation}: top right"
            );
        }
    }

    /// Whatever the tile side and picture size, every level's tiles cover the picture once,
    /// the last row and column cut at its edges.
    #[test]
    fn the_tiles_of_every_level_cover_the_picture_once() {
        for ((w, h), tile) in [
            ((1001, 601), 128),
            ((128, 128), 128),
            ((129, 1), 128),
            ((5000, 8), 2048),
            ((3, 700), 96),
        ] {
            let pyramid = Pyramid::unmade("test", (w, h), tile, 1, coarsest_factor((w, h)));
            for (l, level) in pyramid.levels.iter().enumerate() {
                let mut seen = vec![0u8; (w * h) as usize];
                for i in 0..level.tiles.len() {
                    let t = pyramid.tile_rect(l, i);
                    assert!(t.width > 0 && t.height > 0 && t.width <= tile && t.height <= tile);
                    let [x0, y0, x1, y1] = pyramid.area(l, i).map(|v| v as u32);
                    for y in y0..y1 {
                        for x in x0..x1 {
                            seen[(y * w + x) as usize] += 1;
                        }
                    }
                }
                assert!(
                    seen.iter().all(|&n| n == 1),
                    "{w} x {h} in {tile}, level {l}"
                );
            }
        }
    }

    /// An upscale (or its undo) makes only the coarsest levels at once, so the UI doesn't wait
    /// on a whole large picture; the rest follow as they come into view.
    #[test]
    fn a_resize_makes_only_the_coarsest_levels() {
        let mut v = zoomed_in();
        assert!(!made(&v.canvas.pixels, 0).is_empty());
        (v.pixels, v.mask) = (Rgb16::new(2500, 1500), Mask::new(2500, 1500));
        v.canvas.resize(&v.pixels, &v.mask);
        for pyramid in [&v.canvas.pixels, &v.canvas.mask] {
            let last = pyramid.levels.len() - 1;
            assert_eq!(pyramid.levels[0].factor, 1);
            for (l, level) in pyramid.levels.iter().enumerate() {
                let all = level.tiles.iter().all(Option::is_some);
                let none = level.tiles.iter().all(Option::is_none);
                assert!(if l == last { all } else { none }, "level {l}");
            }
        }
        settle(&mut v);
        let level = v.canvas.pixels.pick(v.canvas.zoom);
        assert_eq!(v.canvas.pixels.levels[level].factor, 2, "zoomed to 40%");
        assert!(
            !made(&v.canvas.pixels, level).is_empty(),
            "made as they come into view"
        );
    }

    /// While the worker has the pixels (an erase or an upscale), no tile is made from the empty
    /// buffer left behind; coarser levels stand in.
    #[test]
    fn no_tile_is_made_while_the_pixels_are_away() {
        let mut v = Viewer::with(CursorTool::Brush, (1000, 600), 1, 128);
        v.frame(vec![]);
        let pixels = std::mem::take(&mut v.pixels);
        v.canvas.actual_pixels();
        settle(&mut v);
        assert!(made(&v.canvas.pixels, 0).is_empty());
        assert!(
            !made(&v.canvas.mask, 0).is_empty(),
            "the mask is still here"
        );
        v.pixels = pixels;
        settle(&mut v);
        assert_eq!(made(&v.canvas.pixels, 0).len(), 16);
    }

    /// Turned 90° clockwise (EXIF 6), a 300 x 200 picture is viewed 200 x 300: at 100% on the
    /// 400 x 400 canvas it is at (100, 50), and its stored top-left tile is at the top right.
    #[test]
    fn tiles_are_drawn_turned_with_the_picture() {
        let mut v = Viewer::with(CursorTool::Brush, (300, 200), 6, 128);
        settle(&mut v);
        assert_eq!(v.canvas.zoom, 1.0);
        let shown = v
            .canvas
            .picture_rect(v.canvas.viewport, v.canvas.points_per_pixel);
        assert_eq!(
            shown,
            ScreenRect::from_min_size(pos2(100.0, 50.0), vec2(200.0, 300.0))
        );
        let pyramid = &v.canvas.pixels;
        assert_eq!(made(pyramid, 0).len(), 6, "3 x 2 tiles, all in view");
        let place = v.canvas.place(pyramid, shown, v.canvas.viewport);
        let meshes = pyramid.meshes(0, &place);
        assert_eq!(meshes.len(), 6, "one quad each");
        let first = pyramid.levels[0].tiles[0].as_ref().unwrap().id();
        let quad = meshes.iter().find(|m| m.texture_id == first).unwrap();
        // Where a point of the texture is on screen.
        let at = |quad: &egui::Mesh, u: f32, v: f32| {
            let p = quad
                .vertices
                .iter()
                .find(|p| p.uv == pos2(u, v))
                .unwrap()
                .pos;
            (p.x.round(), p.y.round())
        };
        // Stored (0, 0) is viewed at the top right; stored (128, 128), 128 texels into the
        // texture of 129 (its apron on the right and bottom), 72 px from the left and 128 px
        // down.
        let inner = 128.0 / 129.0;
        assert_eq!(at(quad, 0.0, 0.0), (300.0, 50.0));
        assert_eq!(at(quad, inner, inner), (172.0, 178.0));
        // The last column's tile is 44 texels wide, after an apron of one, and ends at the
        // picture's stored right edge, viewed at the bottom.
        let last = pyramid.levels[0].tiles[2].as_ref().unwrap().id();
        let quad = meshes.iter().find(|m| m.texture_id == last).unwrap();
        assert_eq!(at(quad, 1.0, inner), (172.0, 350.0));
        assert_eq!(at(quad, 1.0 / 45.0, 0.0), (300.0, 306.0));
    }

    /// Each tile's texture holds its neighbours' edge texels too (an apron), so that filtering
    /// across a tile border reads what one texture of the whole level would, not the tile's
    /// own edge twice (review round 2: faint lines between tiles when zoomed out). The quads
    /// leave the apron out.
    #[test]
    fn tile_textures_hold_their_neighbours_edges() {
        let mut v = Viewer::with(CursorTool::Brush, (300, 200), 1, 128);
        let pixels = Rgb16::from_fn(300, 200, |x, y| {
            gumka_core::image::Rgb([x as u16 * 200, y as u16 * 300, 40000])
        });
        let place = Place {
            orientation: 1,
            size: (300.0, 200.0),
            shown: ScreenRect::from_min_size(Pos2::ZERO, vec2(300.0, 200.0)),
            clip: ScreenRect::EVERYTHING,
        };
        v.ctx.tex_manager().write().take_delta().clear();
        let Canvas {
            pixels: pyramid,
            display,
            ctx,
            ..
        } = &mut v.canvas;
        let left = pyramid.make_visible(ctx, 0, &place, &mut Budget::unlimited(), |f, r| {
            render_pixels(&pixels, display, f, r)
        });
        assert!(!left && made(pyramid, 0).len() == 6, "3 x 2 tiles");
        let patches = taken_patches(ctx);
        let image = |index: usize| {
            let id = pyramid.levels[0].tiles[index].as_ref().unwrap().id();
            let [patch] = &patches[&id][..] else {
                panic!("one image for tile {index}")
            };
            assert_eq!(patch.pos, None, "a whole texture");
            let egui::ImageData::Color(image) = &patch.image;
            image.clone()
        };
        // Tile (0, 0) holds texels 0 to 128 across and down; tile (1, 0), 127 to 256 across.
        assert_eq!(
            [pyramid.texture_rect(0, 0), pyramid.texture_rect(0, 1)],
            [Rect::new(0, 0, 129, 129), Rect::new(127, 0, 130, 129)]
        );
        let (first, second) = (image(0), image(1));
        for (index, texture) in [(0, &first), (1, &second)] {
            let fresh = render_pixels(&pixels, display, 1, pyramid.texture_rect(0, index));
            assert!(
                texture.pixels == fresh.pixels,
                "tile {index} with its apron"
            );
        }
        for y in 0..129 {
            // Either side of the border at 128, each texture has what the other's tile holds.
            assert_eq!(second.pixels[y * 130], first.pixels[y * 129 + 127]);
            assert_eq!(first.pixels[y * 129 + 128], second.pixels[y * 130 + 1]);
        }
        let quad = pyramid.quad(
            0,
            1,
            pyramid.levels[0].tiles[1].as_ref().unwrap(),
            pyramid.area(0, 1),
            &place,
        );
        let mut across: Vec<f32> = quad.vertices.iter().map(|p| p.uv.x).collect();
        across.dedup();
        across.sort_by(f32::total_cmp);
        across.dedup();
        assert_eq!(
            across,
            [1.0 / 130.0, 129.0 / 130.0],
            "a texel in from either side"
        );
    }

    /// A render large enough to be split over the cores gives what rendering it a row at a time
    /// on this thread gives, for the picture and the mask, at full size and averaged.
    #[test]
    fn renders_split_over_threads_match_one_row_at_a_time() {
        let (w, h) = (301u32, 257u32);
        let pixels = Rgb16::from_fn(w, h, |x, y| {
            gumka_core::image::Rgb([(x * 211) as u16, (y * 251) as u16, ((x ^ y) * 97) as u16])
        });
        let mask = Mask::from_fn(w, h, |x, y| gumka_core::image::Luma([((x * y) % 3) as u8]));
        let display = DisplayTransform::new(None, crate::display::SCREEN);
        for factor in [1, 2] {
            let rect = level_rect((w, h), factor);
            let read = u64::from(rect.width * rect.height * factor * factor);
            assert!(read >= gumka_core::PARALLEL_FROM, "split at {factor}");
            let rows = |render: &dyn Fn(Rect) -> ColorImage| -> Vec<Color32> {
                (0..rect.height)
                    .flat_map(|y| render(Rect::new(0, y, rect.width, 1)).pixels)
                    .collect()
            };
            let picture = |r| render_pixels(&pixels, &display, factor, r);
            assert!(
                picture(rect).pixels == rows(&picture),
                "picture at {factor}"
            );
            let overlay = |r| render_mask(&mask, factor, r);
            assert!(overlay(rect).pixels == rows(&overlay), "mask at {factor}");
        }
    }

    /// At a level whose blocks overhang the picture (301 pixels, 2 to a texel), the texture
    /// coordinates stop at the picture's edge, half a texel short of the texture's.
    #[test]
    fn a_level_that_overhangs_stops_at_the_picture_edge() {
        let v = Viewer::with(CursorTool::Brush, (301, 10), 1, 512);
        let pyramid = &v.canvas.pixels;
        assert_eq!(
            (pyramid.levels[1].factor, pyramid.levels[1].size),
            (2, (151, 5))
        );
        let place = Place {
            orientation: 1,
            size: (301.0, 10.0),
            shown: ScreenRect::from_min_size(Pos2::ZERO, vec2(301.0, 10.0)),
            clip: ScreenRect::EVERYTHING,
        };
        let meshes = pyramid.meshes(1, &place);
        assert_eq!(meshes.len(), 1);
        let right = meshes[0]
            .vertices
            .iter()
            .map(|p| p.uv.x)
            .fold(0.0, f32::max);
        assert_eq!(right, 150.5 / 151.0);
    }
}
