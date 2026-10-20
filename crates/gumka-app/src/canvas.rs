// SPDX-License-Identifier: GPL-3.0-or-later

//! The picture on screen: view transform, preview textures and brush painting.
//!
//! The picture and the mask overlay are each shown from a small pyramid of textures (full
//! size, 1/2, 1/4, ...), because egui's renderer has no mipmaps: drawing the level closest
//! to the zoom keeps zoomed-out views from shimmering. Edits update only the changed region
//! of each level.
//!
//! Colors are converted from the picture's profile into the screen's (see display.rs).

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
/// Regions with fewer pixels than this are converted on one thread.
const PARALLEL_FROM: u32 = 1 << 16;

/// Stop adding pyramid levels below this size.
const SMALLEST_LEVEL: u32 = 256;

const TEXTURE_OPTIONS: TextureOptions = TextureOptions {
    magnification: TextureFilter::Nearest,
    minification: TextureFilter::Linear,
    wrap_mode: TextureWrapMode::ClampToEdge,
    mipmap_mode: None,
};

struct Level {
    factor: u32,
    texture: TextureHandle,
}

struct Pyramid {
    levels: Vec<Level>,
}

impl Pyramid {
    fn new(
        ctx: &egui::Context,
        name: &str,
        size: (u32, u32),
        render: impl Fn(u32, Rect) -> ColorImage,
    ) -> Self {
        Self::build(ctx, name, size, render, false).0
    }

    /// Two pyramids with the same content, rendered once: one to edit, one to keep.
    fn pair(
        ctx: &egui::Context,
        name: &str,
        size: (u32, u32),
        render: impl Fn(u32, Rect) -> ColorImage,
    ) -> (Self, Self) {
        let (a, b) = Self::build(ctx, name, size, render, true);
        (a, b.unwrap_or(Self { levels: Vec::new() }))
    }

    fn build(
        ctx: &egui::Context,
        name: &str,
        (w, h): (u32, u32),
        render: impl Fn(u32, Rect) -> ColorImage,
        twin: bool,
    ) -> (Self, Option<Self>) {
        let max_side = ctx.input(|i| i.max_texture_side).max(1) as u32;
        let mut factor = w.max(h).div_ceil(max_side).next_power_of_two();
        let (mut levels, mut twins) = (Vec::new(), Vec::new());
        loop {
            let size = level_size((w, h), factor);
            let image = render(factor, Rect::new(0, 0, size.0, size.1));
            if twin {
                let texture = ctx.load_texture(
                    format!("{name}-original-{factor}"),
                    image.clone(),
                    TEXTURE_OPTIONS,
                );
                twins.push(Level { factor, texture });
            }
            let texture = ctx.load_texture(format!("{name}-{factor}"), image, TEXTURE_OPTIONS);
            levels.push(Level { factor, texture });
            if w.max(h) / factor <= SMALLEST_LEVEL {
                break;
            }
            factor *= 2;
        }
        (Self { levels }, twin.then_some(Self { levels: twins }))
    }

    /// Re-renders `rect` (in picture pixels) in every level.
    fn update(&mut self, (w, h): (u32, u32), rect: Rect, render: impl Fn(u32, Rect) -> ColorImage) {
        for level in &mut self.levels {
            let f = level.factor;
            let (lw, lh) = level_size((w, h), f);
            let (x0, y0) = (rect.x / f, rect.y / f);
            let (x1, y1) = (
                rect.right().div_ceil(f).min(lw),
                rect.bottom().div_ceil(f).min(lh),
            );
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let image = render(f, Rect::new(x0, y0, x1 - x0, y1 - y0));
            level
                .texture
                .set_partial([x0 as usize, y0 as usize], image, TEXTURE_OPTIONS);
        }
    }

    /// The coarsest level that still has at least one texel per screen pixel.
    fn pick(&self, pixels_per_image_pixel: f32) -> &Level {
        let wanted = (1.0 / pixels_per_image_pixel).max(1.0);
        self.levels
            .iter()
            .rev()
            .find(|l| l.factor as f32 <= wanted)
            .unwrap_or(&self.levels[0])
    }
}

fn level_size((w, h): (u32, u32), factor: u32) -> (u32, u32) {
    (w.div_ceil(factor), h.div_ceil(factor))
}

/// Averages `factor`-sized blocks of the picture into `rect` of a level and converts them for
/// the screen. Large regions are split over the cores by rows.
fn render_pixels(
    pixels: &Rgb16,
    display: &DisplayTransform,
    factor: u32,
    rect: Rect,
) -> ColorImage {
    let width = rect.width as usize;
    let mut out = vec![Color32::BLACK; width * rect.height as usize];
    let threads = if rect.width * rect.height < PARALLEL_FROM {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    let rows_each = (rect.height as usize).div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (i, chunk) in out.chunks_mut(rows_each * width).enumerate() {
            scope.spawn(move || {
                let mut stored = vec![[0u16; 3]; width];
                let mut shown = vec![[0u8; 3]; width];
                for (r, row) in chunk.chunks_mut(width).enumerate() {
                    let ly = rect.y + (i * rows_each + r) as u32;
                    average_row(pixels, factor, ly, rect, &mut stored);
                    display.convert(&stored, &mut shown);
                    for (out, &[r, g, b]) in row.iter_mut().zip(&shown) {
                        *out = Color32::from_rgb(r, g, b);
                    }
                }
            });
        }
    });
    ColorImage::new([width, rect.height as usize], out)
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
    let mut out = Vec::with_capacity((rect.width * rect.height) as usize);
    for ly in rect.y..rect.bottom() {
        let (y0, y1) = (ly * factor, ((ly + 1) * factor).min(h));
        for lx in rect.x..rect.right() {
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
            out.push(Color32::from_rgba_unmultiplied(r, g, b, alpha));
        }
    }
    ColorImage::new([rect.width as usize, rect.height as usize], out)
}

/// What a primary drag on the picture does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorTool {
    Brush,
    Remove,
    /// Click an object to select it (SAM).
    Select,
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
    /// From the picture's color profile to the screen's.
    display: DisplayTransform,
    /// Stored pixel size; textures and the mask use stored coordinates.
    size: (u32, u32),
    /// EXIF orientation (1-8) applied when drawing, and its viewed size.
    orientation: u8,
    viewed: (u32, u32),
    pixels: Pyramid,
    /// The picture as opened, for Before / After.
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
    /// Started with Space held or the Pan tool on.
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
        let size = pixels.dimensions();
        let brush_radius = default_brush(size);
        let display = DisplayTransform::new(icc, crate::display::SCREEN);
        let (picture, original) = Pyramid::pair(ctx, "picture", size, |f, r| {
            render_pixels(pixels, &display, f, r)
        });
        Self {
            display,
            size,
            orientation,
            viewed: oriented_size(size, orientation),
            pixels: picture,
            original,
            mask: Pyramid::new(ctx, "mask", size, |f, r| render_mask(mask, f, r)),
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
        self.pixels.update(self.size, rect, |f, r| {
            render_pixels(pixels, &self.display, f, r)
        });
    }

    pub fn update_mask(&mut self, mask: &Mask, rect: Rect) {
        self.mask
            .update(self.size, rect, |f, r| render_mask(mask, f, r));
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

    /// Shows the canvas filling `ui` and handles pan, zoom and painting.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
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
        let picture = if options.compare {
            &self.original
        } else {
            &self.pixels
        };
        let overlay = (options.show_mask && !options.compare).then_some(&self.mask);
        for pyramid in std::iter::once(picture).chain(overlay) {
            let level = pyramid.pick(self.zoom);
            let (lw, lh) = level_size(self.size, level.factor);
            // A level covers whole blocks, so it can overhang the picture by under one block.
            let scale = vec2(
                self.size.0 as f32 / (lw * level.factor) as f32,
                self.size.1 as f32 / (lh * level.factor) as f32,
            );
            // One quad over the viewed rectangle; its texture coordinates turn the stored
            // pixels by the picture's orientation.
            let mut mesh = egui::Mesh::with_texture(level.texture.id());
            let corners = [
                (shown.left_top(), (0.0, 0.0)),
                (shown.right_top(), (1.0, 0.0)),
                (shown.right_bottom(), (1.0, 1.0)),
                (shown.left_bottom(), (0.0, 1.0)),
            ];
            for (pos, (u, v)) in corners {
                let (s, t) = viewed_to_stored(self.orientation, u, v);
                mesh.vertices.push(egui::epaint::Vertex {
                    pos,
                    uv: pos2(s * scale.x, t * scale.y),
                    color: Color32::WHITE,
                });
            }
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            painter.add(egui::Shape::mesh(mesh));
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
        CursorTool::Pan => ("Drag to pan".into(), &[("B", "brush")]),
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

    /// The finest preview level: 1 shows every picture pixel.
    pub(crate) fn finest_factor(canvas: &Canvas) -> u32 {
        canvas.pixels.levels[0].factor
    }

    /// A 200 x 200 picture on a 400 x 400 canvas: shown at 100%, its top-left corner at
    /// (100, 100) on screen.
    struct Viewer {
        ctx: egui::Context,
        canvas: Canvas,
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
            let ctx = egui::Context::default();
            let mask = Mask::new(200, 200);
            let canvas = Canvas::new(&ctx, &Rgb16::new(200, 200), &mask, 1, None);
            Self {
                ctx,
                canvas,
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
            let (canvas, mask, tool, strokes) = (
                &mut self.canvas,
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
                    strokes.extend(canvas.show(ui, mask, &options).stroke_done);
                })
                .drop_without_applying_deltas();
        }

        fn painted(&self) -> usize {
            self.mask.pixels().filter(|p| p.0[0] != 0).count()
        }
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
    /// textures of at most 2048 pixels, and the preview would be a quarter of a wide picture.
    #[test]
    fn previews_use_the_texture_limit_the_frame_reports() {
        let ctx = egui::Context::default();
        let (pixels, mask) = (Rgb16::new(5000, 8), Mask::new(5000, 8));
        let early = Canvas::new(&ctx, &pixels, &mask, 1, None);
        assert_eq!(finest_factor(&early), 4);
        let mut canvas = None;
        let input = RawInput {
            max_texture_side: Some(16384),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            canvas = Some(Canvas::new(ui.ctx(), &pixels, &mask, 1, None));
        })
        .drop_without_applying_deltas();
        assert_eq!(finest_factor(&canvas.unwrap()), 1);
    }
}
