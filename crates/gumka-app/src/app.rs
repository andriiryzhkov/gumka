// SPDX-License-Identifier: GPL-3.0-or-later

//! The app window, laid out as in the design: toolbar, tool rail, viewer, inspector and
//! status bar, plus the start screen and dialogs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Key, KeyboardShortcut, Layout,
    Margin, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2, vec2,
};
use gumka_core::erase::EraseSettings;
use gumka_core::picture::FileType;
use gumka_core::upscale::{Factor, largest_factor, result_size};

use crate::canvas::{Canvas, CursorTool, ShowOptions};
use crate::display::ColorSpaceTag;
use crate::document::{Changed, Document};
use crate::icons::{self, Icon};
use crate::theme::*;
use crate::widgets;
use crate::worker::{
    BACKEND, FOUND, Reply, Status, THIS_COMPUTER, UPSCALERS, Which, Worker, upscaler_name,
    upscaler_slot,
};
use gumka_core::select::Click;

const OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SAVE_AS: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);
const UNDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
const REDO: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
const RUN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Enter);
const CLEAR_MASK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Escape);
const FIT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0);
const ACTUAL_PIXELS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num1);
const ZOOM_IN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals);
/// ⌘+ zooms in too: Shift-= on US keyboards, a key of its own on others.
const ZOOM_IN_PLUS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus);
const ZOOM_OUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Minus);
const SMALLER_BRUSH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::OpenBracket);
const BIGGER_BRUSH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::CloseBracket);
const BRUSH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::B);
const REMOVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::E);
const SELECT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::S);
const ERASE_TOOL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Num1);
const UPSCALE_TOOL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Num2);

/// Height of the History list before it scrolls: about five rows.
const HISTORY_LIST_HEIGHT: f32 = 135.0;

/// A tool in the rail. Extend slots in here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Erase,
    Upscale,
}

/// What a tool works on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    /// What the mask marks: the mask tools are on.
    Mask,
    /// The whole photo: the mask is hidden and can't be painted.
    WholeImage,
}

impl Input {
    fn label(self) -> &'static str {
        match self {
            Input::Mask => "Mask",
            Input::WholeImage => "Whole image",
        }
    }
}

struct ToolInfo {
    name: &'static str,
    input: Input,
    blurb: &'static str,
    action: &'static str,
    footnote: &'static str,
    icon: Icon,
    shortcut: KeyboardShortcut,
}

impl Tool {
    const ALL: [Tool; 2] = [Tool::Erase, Tool::Upscale];

    fn info(self) -> ToolInfo {
        match self {
            Tool::Erase => ToolInfo {
                name: "Erase",
                input: Input::Mask,
                blurb: "Remove objects and rebuild the area from its surroundings.",
                action: "Erase",
                footnote: "Pixels beyond the mask’s soft edge stay untouched.",
                icon: icons::ERASE,
                shortcut: ERASE_TOOL,
            },
            Tool::Upscale => ToolInfo {
                name: "Upscale",
                input: Input::WholeImage,
                blurb: "Make the photo larger, with detail rebuilt by a model.",
                action: "Upscale",
                footnote: "Undo returns the photo to its original size.",
                icon: icons::UPSCALE,
                shortcut: UPSCALE_TOOL,
            },
        }
    }

    fn input(self) -> Input {
        self.info().input
    }
}

/// An upscale under way.
struct UpscaleRun {
    factor: Factor,
    /// Set to stop it after the tile it is on.
    cancel: Arc<AtomicBool>,
    /// Tiles done, of how many (0 of 0 until the worker says).
    done: usize,
    total: usize,
}

/// Said when not even the smallest scale fits the photo.
const AS_LARGE_AS_IT_GETS: &str = "This photo is already as large as Upscale can make it.";

/// The most memory a photo upscaled from `from` to `to` pixels can take while it is open, in
/// bytes: its pixels (6 bytes each, 2 more with alpha) and mask (1); the preview's and the mask
/// overlay's pyramids, once every tile has been shown at 100% (4 bytes a texel, and a third
/// more for the smaller levels); and the photo as it was, kept for undo, with the Before / After
/// pyramid.
fn memory_needed(from: u64, to: u64, alpha: bool) -> f64 {
    let stored = if alpha { 9.0 } else { 7.0 };
    let pyramid = 4.0 * 4.0 / 3.0;
    to as f64 * (stored + 2.0 * pyramid) + from as f64 * (stored + pyramid)
}

/// A click-to-select session: its clicks, and the mask from before it, which every result
/// is added to.
struct Selection {
    clicks: Vec<Click>,
    /// How many of the clicks the mask shows: those of the last result applied.
    shown: usize,
    base: gumka_core::Mask,
}

/// What fills the window between the rail and the status bar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Editor,
    Config,
}

/// Which inspector sections are open.
struct Sections {
    selection: bool,
    model: bool,
    edges: bool,
    advanced: bool,
    scale: bool,
}

/// An action waiting for the answer to "save changes?".
enum Pending {
    OpenDialog,
    Open(PathBuf),
    Quit,
}

pub struct GumkaApp {
    doc: Option<Document>,
    canvas: Option<Canvas>,
    color_space: ColorSpaceTag,
    worker: Worker,
    model: Status,
    /// The selection model (SAM), and the most clicks it takes per selection.
    sam: Status,
    max_clicks: usize,
    selection: Option<Selection>,
    /// The pixel generation the selection model last encoded. Generations are unique across
    /// documents, so this never matches another picture.
    encoded: Option<u64>,
    /// The pixels are with the worker for encoding (`busy` is set too).
    encoding: bool,
    /// Id of the newest selection request, and whether one is running. Ending a selection
    /// moves the id on, so that replies still on their way are ignored.
    select_id: u64,
    selecting: bool,
    /// The pixels are with the worker: for an erase, an upscale (`upscaling`), or for encoding
    /// (`encoding`).
    busy: bool,
    /// The upscalers, in the order of [`UPSCALERS`].
    upscalers: [Status; UPSCALERS.len()],
    /// The scale chosen for Upscale, changed only by its slider. What it makes is
    /// [`Self::effective_factor`].
    factor: Factor,
    upscaling: Option<UpscaleRun>,
    /// The last tool run ("erase", "upscale") and its seconds, for the status bar.
    last_run: Option<(&'static str, f32)>,
    /// Shown in a dialog until dismissed.
    message: Option<String>,
    pending: Option<Pending>,
    /// Asked for while the worker had the pixels, and done once they are back: the save first,
    /// then the request (the newest one wins). The request goes through [`Self::request`]
    /// then, so it asks about the picture as it is by then.
    waiting_save: Option<PathBuf>,
    waiting: Option<Pending>,
    allow_close: bool,
    title: String,
    tool: Tool,
    cursor: CursorTool,
    show_mask: bool,
    /// Before / After toggled on (holding \ also shows it).
    compare: bool,
    settings: EraseSettings,
    sections: Sections,
    page: Page,
    /// Filled when the Config page opens.
    model_rows: Vec<crate::config_page::ModelRow>,
    /// The file from the command line, opened on the first frame: only then does egui know
    /// how large a texture the GPU takes, which the canvas needs.
    startup: Option<PathBuf>,
    /// A file another app sent, until it is opened: that document offers Save & Return.
    sent: Option<PathBuf>,
    /// How many of the files hovering over the window were given up on: AppKit can end a drag
    /// without telling a window that was busy, and then the hover never ends.
    stale_hover: usize,
    /// The main window's zoom (fullscreen) button is off, while a dialog has a window.
    zoom_off: bool,
}

impl GumkaApp {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        // ⌘+, ⌘- and ⌘0 zoom the photo, not the whole UI.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        crate::quit::install(&cc.egui_ctx);
        crate::open_files::attach(&cc.egui_ctx);
        Self {
            doc: None,
            canvas: None,
            color_space: ColorSpaceTag::new(),
            worker: Worker::start(cc.egui_ctx.clone()),
            model: Status::Loading,
            sam: Status::Loading,
            // Until the model says.
            max_clicks: usize::MAX,
            selection: None,
            encoded: None,
            encoding: false,
            select_id: 0,
            selecting: false,
            busy: false,
            upscalers: std::array::from_fn(|_| Status::Loading),
            factor: Factor::X2,
            upscaling: None,
            last_run: None,
            message: None,
            pending: None,
            waiting_save: None,
            waiting: None,
            allow_close: false,
            title: String::new(),
            tool: Tool::Erase,
            cursor: CursorTool::Brush,
            show_mask: true,
            compare: false,
            settings: EraseSettings::default(),
            page: Page::Editor,
            model_rows: Vec::new(),
            sections: Sections {
                selection: true,
                model: true,
                edges: false,
                advanced: false,
                scale: true,
            },
            startup: path,
            sent: None,
            stale_hover: 0,
            zoom_off: false,
        }
    }

    fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(Document::is_dirty)
    }

    /// Unsaved changes, counting those on their way: an erase still running, or a save
    /// waiting for one.
    fn has_unsaved_work(&self) -> bool {
        self.is_dirty() || (self.busy && !self.encoding) || self.waiting_save.is_some()
    }

    fn model_ready(&self) -> bool {
        matches!(self.model, Status::Ready { .. })
    }

    fn dialog_open(&self) -> bool {
        self.pending.is_some() || self.message.is_some()
    }

    // ---- File actions ----

    /// Runs `action`, asking first if there are unsaved changes. While the worker has the
    /// pixels, or a save waits for them, it waits too, and asks about the changes as they are
    /// then.
    fn request(&mut self, ctx: &egui::Context, action: Pending) {
        if self.busy || self.waiting_save.is_some() {
            self.waiting = Some(action);
        } else if self.is_dirty() {
            self.pending = Some(action);
        } else {
            self.run_pending(ctx, action);
        }
    }

    fn run_pending(&mut self, ctx: &egui::Context, action: Pending) {
        match action {
            Pending::OpenDialog => {
                let mut dialog = rfd::FileDialog::new().add_filter("Images", &FileType::EXTENSIONS);
                if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.parent()) {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(path) = panels::pick_file(dialog) {
                    self.open(ctx, path);
                }
            }
            Pending::Open(path) => self.open(ctx, path),
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn show_config(&mut self) {
        self.model_rows = crate::config_page::load_rows();
        self.page = Page::Config;
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.busy {
            self.waiting = Some(Pending::Open(path));
            return;
        }
        let sent = self.sent.take().is_some_and(|sent| sent == path);
        match Document::open(path.clone()) {
            Ok(mut doc) => {
                doc.sent = sent;
                self.end_selection();
                self.canvas = Some(Canvas::new(
                    ctx,
                    &doc.picture.pixels,
                    &doc.mask,
                    doc.picture.orientation(),
                    doc.picture.icc_profile(),
                ));
                self.doc = Some(doc);
                self.page = Page::Editor;
                self.last_run = None;
                self.compare = false;
            }
            Err(e) => self.message = Some(format!("Couldn't open {}:\n{e}", path.display())),
        }
    }

    /// Saves in place. Returns whether it worked. A save already waiting for the pixels (a
    /// Save As, say) stays the one that happens.
    fn save(&mut self) -> bool {
        if self.waiting_save.is_some() {
            return false;
        }
        match &self.doc {
            Some(doc) => self.save_to(doc.path.clone()),
            None => false,
        }
    }

    /// Save & Return, for a photo another app sent: saves it in place (once the worker gives the
    /// pixels back, if it has them) and quits, so that macOS brings back the app it came from.
    /// A failed save says why and stays. Does nothing while a save waits (Save As, say): that
    /// one happens first.
    fn save_and_return(&mut self, ctx: &egui::Context) {
        if self.waiting_save.is_some() {
            return;
        }
        if !self.has_unsaved_work() || self.save() {
            self.run_pending(ctx, Pending::Quit);
        } else if self.waiting_save.is_some() {
            self.waiting = Some(Pending::Quit);
        }
    }

    fn save_as(&mut self) -> bool {
        let Some(doc) = &self.doc else { return false };
        let mut dialog = rfd::FileDialog::new()
            .add_filter("TIFF", &["tif", "tiff"])
            .add_filter("PNG", &["png"])
            .add_filter("JPEG", &["jpg", "jpeg"])
            .set_file_name(doc.name());
        if let Some(dir) = doc.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = panels::save_file(dialog) else {
            return false;
        };
        if path.extension().is_none() {
            path.set_extension(doc.path.extension().unwrap_or_default());
        }
        self.save_to(path)
    }

    /// Saves to `path`. Returns whether it worked; while the worker has the pixels, the save
    /// waits for them and this returns false.
    fn save_to(&mut self, path: PathBuf) -> bool {
        if self.busy {
            self.waiting_save = Some(path);
            return false;
        }
        let Some(doc) = &mut self.doc else {
            return false;
        };
        match doc.picture.save(&path) {
            Ok(()) => {
                // Saved as another file, it's no longer the one the sending app waits for.
                doc.sent &= path == doc.path;
                doc.path = path;
                doc.mark_saved();
                true
            }
            Err(e) => {
                self.message = Some(format!("Couldn't save {}:\n{e}", path.display()));
                false
            }
        }
    }

    // ---- Edit actions ----

    fn run_tool(&mut self) {
        match self.tool {
            Tool::Erase => self.run_erase(),
            Tool::Upscale => self.run_upscale(),
        }
    }

    fn run_erase(&mut self) {
        if self.busy {
            return;
        }
        let Some(doc) = &mut self.doc else { return };
        match &self.model {
            Status::Ready { .. } => {}
            Status::Loading | Status::NotLoaded => {
                self.message = Some("The model is still loading.".into());
                return;
            }
            Status::Unavailable(reason) => {
                self.message = Some(format!("Erase is unavailable:\n{reason}"));
                return;
            }
        }
        if !doc.has_mask() {
            self.message = Some("Paint over what you want to remove first.".into());
            return;
        }
        self.busy = true;
        self.compare = false;
        let profile = doc.picture.icc_profile().map(<[u8]>::to_vec);
        self.worker
            .erase(doc.take_pixels(), doc.mask.clone(), self.settings, profile);
        self.end_selection();
    }

    /// The scale Upscale makes: the one chosen, or the largest that fits the open photo when
    /// that is smaller, so that the slider, its label, the summary and the action agree. The
    /// choice stays for the next photo, and for this one after an undo.
    fn effective_factor(&self) -> Factor {
        let largest = self.doc.as_ref().and_then(|d| {
            let (w, h) = d.viewed_size();
            largest_factor(w, h)
        });
        match largest {
            Some(largest) if largest.quarters() < self.factor.quarters() => largest,
            _ => self.factor,
        }
    }

    /// Why Upscale can't run now, said for the user, or `None` if it can.
    fn upscale_refused(&self) -> Option<String> {
        let doc = self.doc.as_ref()?;
        let factor = self.effective_factor();
        let slot = upscaler_slot(factor.model_scale());
        if let Status::Unavailable(reason) = &self.upscalers[slot] {
            return Some(format!("Upscale is unavailable:\n{reason}"));
        }
        // The effective factor fits whenever any does.
        let (w, h) = doc.viewed_size();
        result_size(w, h, factor)
            .is_err()
            .then(|| AS_LARGE_AS_IT_GETS.into())
    }

    fn run_upscale(&mut self) {
        if self.busy || self.doc.is_none() {
            return;
        }
        if let Some(why) = self.upscale_refused() {
            self.message = Some(why);
            return;
        }
        let factor = self.effective_factor();
        let Some(doc) = &mut self.doc else { return };
        self.busy = true;
        self.compare = false;
        let profile = doc.picture.icc_profile().map(<[u8]>::to_vec);
        let alpha = doc.picture.alpha().map(<[u16]>::to_vec);
        let premultiplied = doc.picture.associated_alpha();
        let cancel = self
            .worker
            .upscale(doc.take_pixels(), alpha, premultiplied, factor, profile);
        self.upscaling = Some(UpscaleRun {
            factor,
            cancel,
            done: 0,
            total: 0,
        });
        self.end_selection();
    }

    /// Stops the running upscale after its current tile; the photo stays as it was.
    fn cancel_upscale(&mut self) {
        if let Some(run) = &self.upscaling {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Adds a click to the selection, starting one if needed, and asks for a new mask.
    fn add_click(&mut self, click: Click) {
        let Some(doc) = &self.doc else { return };
        if !self.sam.is_ready() {
            return;
        }
        let selection = self.selection.get_or_insert_with(|| Selection {
            clicks: Vec::new(),
            shown: 0,
            base: doc.mask.clone(),
        });
        if selection.clicks.len() >= self.max_clicks {
            self.message = Some(format!(
                "A selection takes up to {} clicks.\nTo select more, start a new one: switch to \
                 another tool and back.",
                self.max_clicks
            ));
            return;
        }
        selection.clicks.push(click);
        self.request_selection();
    }

    /// Ends the selection, if there is one; replies to its requests are then ignored.
    fn end_selection(&mut self) {
        if self.selection.take().is_some() {
            self.select_id += 1;
            self.selecting = false;
        }
    }

    /// A selection request failed: drops the clicks the mask doesn't show yet, and says why.
    fn selection_failed(&mut self, error: &str) {
        let none_left = self.selection.as_mut().is_some_and(|s| {
            s.clicks.truncate(s.shown);
            s.clicks.is_empty()
        });
        if none_left {
            self.end_selection();
        }
        self.message = Some(format!("Selection failed:\n{error}"));
    }

    /// Sends the selection's clicks to the model, encoding the picture first if it changed.
    fn request_selection(&mut self) {
        let (Some(doc), Some(selection)) = (&mut self.doc, &self.selection) else {
            return;
        };
        if selection.clicks.is_empty() || self.busy {
            return;
        }
        if self.encoded != Some(doc.generation()) {
            self.busy = true;
            self.encoding = true;
            let generation = doc.generation();
            self.worker.encode(doc.take_pixels(), generation);
            return; // asks again when the encoding is back
        }
        self.select_id += 1;
        self.selecting = true;
        self.worker
            .select(self.select_id, selection.clicks.clone(), doc.size());
    }

    /// Shows a selection result: the mask from before the selection plus the selected area.
    fn apply_selection(&mut self, mut selected: gumka_core::Mask, seconds: f32) {
        let (Some(doc), Some(selection)) = (&mut self.doc, &mut self.selection) else {
            return;
        };
        for (out, base) in selected.iter_mut().zip(selection.base.iter()) {
            *out |= *base;
        }
        let model = match &self.sam {
            Status::Ready { model, .. } => model.as_str(),
            _ => "",
        };
        let detail = format!("{model} · {seconds:.2} s");
        let Some(rect) = doc.replace_mask(selected, "Select object", &detail) else {
            self.selection_failed("the mask doesn't match the picture's size");
            return;
        };
        // A click that changes nothing still counts as shown (it leaves no history step).
        selection.shown = selection.clicks.len();
        self.redraw(Changed {
            mask: (!rect.is_empty()).then_some(rect),
            ..Changed::default()
        });
    }

    fn undo_redo(&mut self, undo: bool) {
        if self.busy {
            return;
        }
        self.end_selection();
        if let Some(doc) = &mut self.doc {
            let changed = if undo { doc.undo() } else { doc.redo() };
            self.redraw(changed);
        }
    }

    fn clear_mask(&mut self) {
        if self.busy {
            return;
        }
        self.end_selection();
        if let Some(doc) = &mut self.doc {
            let rect = doc.clear_mask();
            self.redraw(Changed {
                mask: rect,
                ..Changed::default()
            });
        }
    }

    fn invert_mask(&mut self) {
        if self.busy {
            return;
        }
        self.end_selection();
        if let Some(doc) = &mut self.doc {
            let rect = doc.invert_mask();
            self.redraw(Changed {
                mask: Some(rect),
                ..Changed::default()
            });
        }
    }

    fn redraw(&mut self, changed: Changed) {
        let (Some(doc), Some(canvas)) = (&self.doc, &mut self.canvas) else {
            return;
        };
        if changed.resized {
            canvas.resize(&doc.picture.pixels, &doc.mask);
            return;
        }
        if let Some(rect) = changed.pixels {
            canvas.update_pixels(&doc.picture.pixels, rect);
        }
        if let Some(rect) = changed.mask {
            canvas.update_mask(&doc.mask, rect);
        }
    }

    // ---- Per-frame handling ----

    fn poll_worker(&mut self) {
        while let Some(reply) = self.worker.try_recv() {
            match reply {
                Reply::Status(Which::Inpainter, status) => self.model = status,
                Reply::Status(Which::Segmenter, status) => self.sam = status,
                Reply::Status(Which::Upscaler(scale), status) => {
                    self.upscalers[upscaler_slot(scale)] = status;
                }
                Reply::Progress { done, total } => {
                    if let Some(run) = &mut self.upscaling {
                        (run.done, run.total) = (done, total);
                    }
                }
                Reply::Upscaled { pixels, result } => {
                    self.busy = false;
                    self.upscaling = None;
                    let Some(doc) = &mut self.doc else { continue };
                    doc.restore_pixels(pixels);
                    match result {
                        Ok(Some(up)) => {
                            let label = format!("Upscale {}", up.factor);
                            let detail = format!("{} · {:.1} s", up.model, up.seconds);
                            match doc.push_resize(up.pixels, up.alpha, &label, &detail) {
                                Ok(changed) => {
                                    self.last_run = Some(("upscale", up.seconds));
                                    self.redraw(changed);
                                }
                                Err(e) => self.message = Some(format!("Upscale failed:\n{e}")),
                            }
                        }
                        Ok(None) => {} // cancelled: nothing changes
                        Err(e) => self.message = Some(format!("Upscale failed:\n{e}")),
                    }
                }
                Reply::MaxClicks(n) => self.max_clicks = n,
                Reply::Encoded {
                    pixels,
                    generation,
                    result,
                } => {
                    self.busy = false;
                    self.encoding = false;
                    let Some(doc) = &mut self.doc else { continue };
                    doc.restore_pixels(pixels);
                    match result {
                        Ok(_) => {
                            self.encoded = Some(generation);
                            self.request_selection();
                        }
                        Err(e) => {
                            self.end_selection();
                            self.message = Some(format!("Couldn't prepare the selection:\n{e}"));
                        }
                    }
                }
                Reply::Selected { id, result } => {
                    if id != self.select_id {
                        continue; // a newer click is on its way, or the selection ended
                    }
                    self.selecting = false;
                    match result {
                        Ok((selected, seconds)) => self.apply_selection(selected, seconds),
                        Err(e) => self.selection_failed(&e),
                    }
                }
                Reply::Erased { pixels, result } => {
                    self.busy = false;
                    let model = match &self.model {
                        Status::Ready { model, .. } => model.clone(),
                        _ => String::new(),
                    };
                    let Some(doc) = &mut self.doc else { continue };
                    doc.restore_pixels(pixels);
                    match result {
                        Ok(Some(erased)) => {
                            let detail = format!("{model} · {:.1} s", erased.seconds);
                            let changed = doc.push_erase(erased.rect, erased.old, detail);
                            self.last_run = Some(("erase", erased.seconds));
                            self.redraw(changed);
                        }
                        Ok(None) => {}
                        Err(e) => self.message = Some(format!("Erase failed:\n{e}")),
                    }
                }
                Reply::Stopped(reason) => {
                    let reason = format!("the model thread stopped: {reason}");
                    self.model = Status::Unavailable(reason.clone());
                    self.sam = Status::Unavailable(reason.clone());
                    self.upscalers = std::array::from_fn(|_| Status::Unavailable(reason.clone()));
                    self.upscaling = None;
                    self.end_selection();
                    self.encoding = false;
                    if std::mem::take(&mut self.busy) {
                        // The pixels were with the thread and are gone: close the photo so
                        // that it can't be saved without them. A waiting open or quit still
                        // runs, once the message is read.
                        self.doc = None;
                        self.canvas = None;
                        self.pending = None;
                        self.waiting_save = None;
                        self.last_run = None;
                        self.compare = false;
                        self.message = Some(format!(
                            "Gumka lost the photo it was working on ({reason}).\nOpen it again; \
                             changes since the last save are lost."
                        ));
                    }
                }
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        if self.dialog_open() {
            return; // files dropped on a dialog are ignored too
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.request(ctx, Pending::Open(path));
            return;
        }
        // More specific shortcuts first: Cmd+Shift+Z also matches Cmd+Z.
        let pressed = |s: &KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(s));
        if pressed(&OPEN) {
            self.request(ctx, Pending::OpenDialog);
        }
        if pressed(&SAVE_AS) {
            self.save_as();
        }
        if pressed(&SAVE) && self.has_unsaved_work() {
            self.save();
        }
        // Keys that change the mask or the pixels wait for a brush stroke to end: the stroke
        // is recorded against the mask from before it.
        let stroking = self.canvas.as_ref().is_some_and(Canvas::stroking);
        if pressed(&REDO) && !stroking {
            self.undo_redo(false);
        }
        if pressed(&UNDO) && !stroking {
            self.undo_redo(true);
        }
        if self.page == Page::Config {
            if pressed(&CLEAR_MASK) {
                self.page = Page::Editor;
            }
            return;
        }
        if self.doc.is_none() {
            return;
        }
        if pressed(&RUN) && !stroking {
            self.run_tool();
        }
        // Escape stops an upscale; otherwise it clears the mask, where the tool shows one.
        if pressed(&CLEAR_MASK) && !stroking {
            if self.upscaling.is_some() {
                self.cancel_upscale();
            } else if self.tool.input() == Input::Mask {
                self.clear_mask();
            }
        }
        if pressed(&ERASE_TOOL) {
            self.tool = Tool::Erase;
        }
        if pressed(&UPSCALE_TOOL) {
            self.tool = Tool::Upscale;
        }
        // The mask tools' keys belong to tools that take a mask; elsewhere they do nothing.
        let masking = self.tool.input() == Input::Mask;
        if masking && pressed(&BRUSH) {
            self.cursor = CursorTool::Brush;
        }
        if masking && pressed(&REMOVE) {
            self.cursor = CursorTool::Remove;
        }
        if masking && pressed(&SELECT) {
            if self.sam.is_ready() {
                self.cursor = CursorTool::Select;
            } else if let Status::Unavailable(reason) = &self.sam {
                self.message = Some(format!("Select object is unavailable:\n{reason}"));
            }
        }
        if let Some(canvas) = &mut self.canvas {
            // The view holds still while a stroke is painted (see `Canvas::show`).
            if pressed(&FIT) && !stroking {
                canvas.fit();
            }
            if pressed(&ACTUAL_PIXELS) && !stroking {
                canvas.actual_pixels();
            }
            if (pressed(&ZOOM_IN) || pressed(&ZOOM_IN_PLUS)) && !stroking {
                canvas.zoom_by(2.0);
            }
            if pressed(&ZOOM_OUT) && !stroking {
                canvas.zoom_by(0.5);
            }
            if masking && pressed(&SMALLER_BRUSH) {
                canvas.brush_radius = (canvas.brush_radius / 1.25).max(1.0);
            }
            if masking && pressed(&BIGGER_BRUSH) {
                canvas.brush_radius = (canvas.brush_radius * 1.25).min(2000.0);
            }
        }
    }

    /// Runs what waited for the pixels, once the worker has given them back. Holds back while
    /// a message is shown, so that it reads as being about the work it followed.
    fn run_waiting(&mut self, ctx: &egui::Context) {
        if self.busy || self.message.is_some() {
            return;
        }
        if let Some(path) = self.waiting_save.take() {
            // A Save asked for during an erase that then failed has nothing to save.
            let needed = self
                .doc
                .as_ref()
                .is_some_and(|d| d.is_dirty() || d.path != path);
            if needed && !self.save_to(path) {
                return; // the message says why; the request waits for it to be read
            }
        }
        // The open panel waits for the window to be on screen.
        let hidden = ctx.input(|i| i.viewport().visible() == Some(false));
        if hidden && matches!(self.waiting, Some(Pending::OpenDialog)) {
            return;
        }
        if let Some(action) = self.waiting.take() {
            self.request(ctx, action);
        }
    }

    /// Closing the window (or quitting, see quit.rs) with unsaved changes asks first, at once
    /// even while an erase runs: Don't Save mustn't wait for work that may never come back.
    /// Only a save already asked for is waited for, the first time.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.has_unsaved_work()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.waiting_save.is_some() && !matches!(self.waiting, Some(Pending::Quit)) {
                // The save asked for comes first, then the quit, without asking. Closing
                // again asks, so a save that never comes can't keep the window open.
                self.waiting = Some(Pending::Quit);
            } else {
                // The question replaces a quit that was waiting: Cancel there means don't quit.
                if matches!(self.waiting, Some(Pending::Quit)) {
                    self.waiting = None;
                }
                self.pending = Some(Pending::Quit);
            }
        }
    }

    /// The answer to "Save changes?". A save that waits for the pixels takes the action along;
    /// what was waiting before gives way. (Cancel leaves it waiting, but for a quit, which the
    /// question replaced.)
    fn answer(&mut self, ctx: &egui::Context, save: bool) {
        let Some(action) = self.pending.take() else {
            return;
        };
        self.waiting = None;
        if !save || self.save() {
            self.run_pending(ctx, action);
        } else if self.waiting_save.is_some() {
            self.waiting = Some(action);
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.doc {
            Some(doc) => format!(
                "{}{} — Gumka",
                doc.name(),
                if doc.is_dirty() { " •" } else { "" }
            ),
            None => "Gumka".into(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    // ---- Toolbar ----

    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let has_doc = self.doc.is_some();
        let dirty = self.is_dirty();
        let idle = !self.busy;
        // The mask tools are for tools that take a mask. Panning is Space and a drag, anywhere.
        let masking = has_doc && self.tool.input() == Input::Mask;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let tip =
                |name: &str, s: &KeyboardShortcut| format!("{name} ({})", ctx.format_shortcut(s));
            let size = vec2(32.0, 32.0);
            if widgets::icon_button(ui, icons::OPEN, &tip("Open", &OPEN), false, idle, size)
                .clicked()
            {
                self.request(&ctx, Pending::OpenDialog);
            }
            if widgets::icon_button(
                ui,
                icons::SAVE,
                &tip("Save", &SAVE),
                false,
                dirty && idle,
                size,
            )
            .clicked()
            {
                self.save();
            }
            let more = widgets::icon_button(
                ui,
                icons::CHEVRON_DOWN,
                "More ways to save",
                false,
                has_doc && idle,
                vec2(18.0, 32.0),
            );
            egui::Popup::menu(&more).show(|ui| {
                if menu_item(ui, "Save As…", &ctx.format_shortcut(&SAVE_AS)) {
                    self.save_as();
                }
            });
            toolbar_separator(ui);

            Frame::NONE
                .fill(Color32::from_gray(0x1b))
                .stroke(Stroke::new(1.0, Color32::from_gray(0x33)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(2))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let cursors = [
                            (CursorTool::Brush, icons::BRUSH, "Brush (B)"),
                            (CursorTool::Remove, icons::REMOVE, "Remove from mask (E)"),
                        ];
                        for (tool, icon, tooltip) in cursors {
                            let selected = masking && self.cursor == tool;
                            if widgets::icon_button(
                                ui,
                                icon,
                                tooltip,
                                selected,
                                masking,
                                vec2(34.0, 30.0),
                            )
                            .clicked()
                            {
                                self.cursor = tool;
                            }
                        }
                        let tooltip = match &self.sam {
                            Status::Ready { .. } => "Select object (S)".to_owned(),
                            Status::Loading | Status::NotLoaded => {
                                "Select object (S) · loading SAM…".to_owned()
                            }
                            Status::Unavailable(reason) => {
                                format!("Select object (S) · unavailable: {reason}")
                            }
                        };
                        let selected = masking && self.cursor == CursorTool::Select;
                        if widgets::icon_button(
                            ui,
                            icons::SELECT,
                            &tooltip,
                            selected,
                            masking && self.sam.is_ready(),
                            vec2(34.0, 30.0),
                        )
                        .clicked()
                        {
                            self.cursor = CursorTool::Select;
                        }
                    });
                });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                // Save & Return, for a photo another app sent; off while a save waits.
                if self.doc.as_ref().is_some_and(|d| d.sent) {
                    let enabled = self.waiting_save.is_none();
                    let button = widgets::accent_button(ui, "Save & Return", enabled)
                        .on_hover_text("Save the photo and quit, back to the app that sent it");
                    if button.clicked() {
                        self.save_and_return(&ctx);
                    }
                    ui.add_space(6.0);
                }
                // Zoom menu.
                let zoom = self
                    .canvas
                    .as_ref()
                    .map_or(String::from("—"), |c| format!("{:.0}%", c.zoom_percent()));
                let zoom_button = ui.add_enabled(
                    has_doc,
                    egui::Button::new(RichText::new(zoom).font(mono(12.0)))
                        .min_size(vec2(64.0, 30.0)),
                );
                egui::Popup::menu(&zoom_button).show(|ui| {
                    let Some(canvas) = &mut self.canvas else {
                        return;
                    };
                    if menu_item(ui, "Fit in Window", &ctx.format_shortcut(&FIT)) {
                        canvas.fit();
                    }
                    if menu_item(ui, "Actual Pixels", &ctx.format_shortcut(&ACTUAL_PIXELS)) {
                        canvas.actual_pixels();
                    }
                    if menu_item(ui, "Zoom In", &ctx.format_shortcut(&ZOOM_IN_PLUS)) {
                        canvas.zoom_by(2.0);
                    }
                    if menu_item(ui, "Zoom Out", &ctx.format_shortcut(&ZOOM_OUT)) {
                        canvas.zoom_by(0.5);
                    }
                });
                let compare = egui::Button::image_and_text(
                    icons::COMPARE
                        .image()
                        .tint(ICON)
                        .fit_to_exact_size(vec2(16.0, 16.0)),
                    RichText::new("Before / After").size(12.0),
                )
                .selected(self.compare)
                .min_size(vec2(0.0, 30.0));
                if ui
                    .add_enabled(has_doc, compare)
                    .on_hover_text("Show the photo as opened (hold \\ to peek)")
                    .clicked()
                {
                    self.compare = !self.compare;
                }
                toolbar_separator(ui);
                let (can_undo, can_redo) = self
                    .doc
                    .as_ref()
                    .map_or((false, false), |d| (d.can_undo(), d.can_redo()));
                if widgets::icon_button(
                    ui,
                    icons::REDO,
                    &tip("Redo", &REDO),
                    false,
                    can_redo && idle,
                    size,
                )
                .clicked()
                {
                    self.undo_redo(false);
                }
                if widgets::icon_button(
                    ui,
                    icons::UNDO,
                    &tip("Undo", &UNDO),
                    false,
                    can_undo && idle,
                    size,
                )
                .clicked()
                {
                    self.undo_redo(true);
                }
            });
        });
    }

    // ---- Tool rail ----

    fn rail(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for tool in Tool::ALL {
                let info = tool.info();
                let tooltip = format!("{} ({})", info.name, ctx.format_shortcut(&info.shortcut));
                let selected = self.page == Page::Editor && self.tool == tool;
                if widgets::rail_button(ui, info.icon, info.name, &tooltip, selected).clicked() {
                    self.tool = tool;
                    self.page = Page::Editor;
                }
            }
        });
        // Config sits at the bottom of the rail.
        let rail = ui.max_rect();
        let spot = Rect::from_center_size(
            pos2(rail.center().x, rail.bottom() - 29.0),
            vec2(64.0, 58.0),
        );
        let selected = self.page == Page::Config;
        let clicked = ui
            .scope_builder(egui::UiBuilder::new().max_rect(spot), |ui| {
                widgets::rail_button(ui, icons::CONFIG, "Config", "Config", selected).clicked()
            })
            .inner;
        if clicked {
            if selected {
                self.page = Page::Editor;
            } else {
                self.show_config();
            }
        }
    }

    // ---- Inspector ----

    fn inspector(&mut self, ui: &mut Ui) {
        let info = self.tool.info();
        egui::Panel::top("inspector-header")
            .resizable(false)
            .frame(Frame::NONE.inner_margin(Margin {
                left: 16,
                right: 16,
                top: 14,
                bottom: 12,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(info.name)
                            .font(FontId::new(15.0, semibold()))
                            .color(TEXT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::chip(ui, &format!("Input · {}", info.input.label()));
                    });
                });
                ui.label(RichText::new(info.blurb).size(12.0).color(TEXT_WEAK));
            });

        egui::Panel::bottom("inspector-action")
            .resizable(false)
            .frame(Frame::NONE.inner_margin(Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 14,
            }))
            .show(ui, |ui| {
                match self.tool {
                    Tool::Erase => self.erase_action(ui, &info),
                    Tool::Upscale => self.upscale_action(ui, &info),
                }
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(info.footnote).size(11.5).color(TEXT_WEAK));
                });
            });

        // Sized from its rows: a panel sized by its content lags a frame behind.
        let rows = self.doc.as_ref().map_or(1, |d| d.history().len()) as f32;
        egui::Panel::bottom("inspector-history")
            .resizable(false)
            .exact_size(30.0 + (rows * 25.0 + 10.0).min(HISTORY_LIST_HEIGHT))
            .frame(Frame::NONE.fill(BG_HISTORY))
            .show(ui, |ui| self.history(ui));

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match self.tool {
                            Tool::Erase => self.erase_sections(ui),
                            Tool::Upscale => self.upscale_sections(ui),
                        }
                    });
            });
    }

    fn erase_action(&mut self, ui: &mut Ui, info: &ToolInfo) {
        let has_mask = self.doc.as_ref().is_some_and(Document::has_mask);
        let enabled = has_mask && self.model_ready() && !self.busy;
        let erasing = self.busy && !self.encoding && self.upscaling.is_none();
        let label = if erasing { "Erasing…" } else { info.action };
        let response = widgets::primary_button(ui, label, "↩", enabled, erasing);
        let response = if has_mask {
            response
        } else {
            response.on_hover_text("Paint over what you want to remove first")
        };
        if response.clicked() {
            self.run_tool();
        }
    }

    fn upscale_action(&mut self, ui: &mut Ui, info: &ToolInfo) {
        // While it runs, the button itself stops it, so nothing appears or moves. The worker
        // stops after the tile it is on (seconds on a CPU), so a stop shows at once.
        if let Some(run) = &self.upscaling {
            if run.cancel.load(Ordering::Relaxed) {
                widgets::primary_button(ui, "Stopping…", "", false, true)
                    .on_hover_text("Stops after the part it is on; the photo stays as it was");
                return;
            }
            let percent = (run.done * 100).checked_div(run.total).unwrap_or(0);
            let label = format!("Upscaling {}… {percent}%", run.factor);
            if widgets::running_button(ui, &label, "Stop upscaling", "Esc")
                .on_hover_text("The photo stays as it was")
                .clicked()
            {
                self.cancel_upscale();
            }
            return;
        }
        let refused = self.upscale_refused();
        let enabled = self.doc.is_some() && refused.is_none() && !self.busy;
        let label = format!("{} {}", info.action, self.effective_factor());
        let response = widgets::primary_button(ui, &label, "↩", enabled, false);
        let response = match &refused {
            Some(why) => response.on_hover_text(why),
            None => response,
        };
        if response.clicked() {
            self.run_tool();
        }
    }

    fn upscale_sections(&mut self, ui: &mut Ui) {
        let size = self.doc.as_ref().map(Document::viewed_size);
        let may_clip = self.doc.as_ref().is_some_and(Document::may_clip_beyond_p3);
        let alpha = self
            .doc
            .as_ref()
            .is_some_and(|d| d.picture.alpha().is_some());
        let largest = size.map(|(w, h)| largest_factor(w, h));
        let factor = self.effective_factor();
        let result = size.map(|(w, h)| result_size(w, h, factor));
        let summary = match &result {
            Some(Ok((w, h))) => format!("{factor} · {}", megapixels(*w, *h)),
            _ => factor.to_string(),
        };
        let idle = !self.busy;
        widgets::section(
            ui,
            "Scale",
            &mut self.sections.scale,
            Some(summary),
            false,
            |ui| {
                let top = largest.flatten().unwrap_or(Factor::MIN).quarters();
                let mut quarters = factor.quarters();
                ui.add_enabled_ui(idle && largest != Some(None), |ui| {
                    let slider = widgets::step_slider_row(
                        ui,
                        "Scale",
                        &factor.to_string(),
                        &mut quarters,
                        Factor::MIN.quarters()..=top.max(Factor::MIN.quarters()),
                    );
                    if slider.changed()
                        && let Some(chosen) = Factor::from_quarters(quarters)
                    {
                        self.factor = chosen;
                    }
                });
                let made = if factor.is_exact() {
                    "The model's own output.".to_owned()
                } else {
                    format!(
                        "Made with the {}× model, then scaled down to {factor}.",
                        factor.model_scale(),
                    )
                };
                widgets::hint(ui, &made);
                match &result {
                    Some(Ok((w, h))) => {
                        widgets::info_row(
                            ui,
                            "Result",
                            &format!("{w} × {h} px · {}", megapixels(*w, *h)),
                        );
                        let from = size.map_or(0, |(w, h)| u64::from(w) * u64::from(h));
                        let bytes = memory_needed(from, u64::from(*w) * u64::from(*h), alpha);
                        if bytes > 1.5e9 {
                            widgets::hint(
                                ui,
                                &format!(
                                    "A photo this large can need up to about {:.1} GB of \
                                     memory while it is open.",
                                    bytes / 1e9
                                ),
                            );
                        }
                    }
                    Some(Err(_)) => {
                        ui.label(RichText::new(AS_LARGE_AS_IT_GETS).size(12.0).color(WARN));
                    }
                    None => {}
                }
                if may_clip {
                    widgets::hint(
                        ui,
                        "The model sees this photo in Display P3, so colors beyond P3 may be \
                         clipped across the photo.",
                    );
                }
            },
        );

        let scale = factor.model_scale();
        let name = upscaler_name(scale);
        let status = &self.upscalers[upscaler_slot(scale)];
        let summary = match status {
            Status::Ready { .. } | Status::NotLoaded => name.clone(),
            Status::Loading => "Loading…".into(),
            Status::Unavailable(_) => "Unavailable".into(),
        };
        widgets::section(
            ui,
            "Model",
            &mut self.sections.model,
            Some(summary),
            false,
            |ui| match status {
                Status::Ready { runs_on, .. } => {
                    ui.label(
                        RichText::new(&name)
                            .font(FontId::new(14.0, medium()))
                            .color(TEXT),
                    );
                    widgets::info_row(ui, "Runs on", &format!("{THIS_COMPUTER} · {runs_on}"));
                    widgets::hint(
                        ui,
                        "Sharpens what is in the photo without inventing detail.",
                    );
                }
                Status::NotLoaded => {
                    ui.label(
                        RichText::new(&name)
                            .font(FontId::new(14.0, medium()))
                            .color(TEXT),
                    );
                    widgets::info_row(ui, "Runs on", &format!("{THIS_COMPUTER} · {BACKEND}"));
                    widgets::hint(
                        ui,
                        if cfg!(target_os = "macos") {
                            "Sharpens what is in the photo without inventing detail. Included \
                             with Gumka; it loads the first time you upscale."
                        } else {
                            "Sharpens what is in the photo without inventing detail. It loads \
                             the first time you upscale."
                        },
                    );
                }
                Status::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new(format!("Loading {name}…"))
                                .size(12.0)
                                .color(TEXT_LABEL),
                        );
                    });
                }
                Status::Unavailable(reason) => {
                    ui.label(
                        RichText::new("Upscale is unavailable")
                            .size(12.0)
                            .color(WARN),
                    );
                    widgets::hint(ui, reason);
                }
            },
        );
    }

    fn erase_sections(&mut self, ui: &mut Ui) {
        let idle = !self.busy;
        let has_mask = self.doc.as_ref().is_some_and(Document::has_mask);
        let brush = self.canvas.as_ref().map_or(0.0, |c| c.brush_radius * 2.0);
        let (mut invert, mut clear) = (false, false);

        let mode = if self.cursor == CursorTool::Remove {
            "Removes"
        } else {
            "Brush"
        };
        let reset = widgets::section(
            ui,
            "Selection",
            &mut self.sections.selection,
            Some(format!("{mode} · {brush:.0} px")),
            true,
            |ui| {
                if let Some(canvas) = &mut self.canvas {
                    let mut diameter = canvas.brush_radius * 2.0;
                    let slider = widgets::slider_row(
                        ui,
                        "Brush size",
                        &mut diameter,
                        2.0..=4000.0,
                        0.0,
                        " px",
                        0,
                        true,
                    );
                    if slider.on_hover_text("[ and ] change the size").changed() {
                        canvas.brush_radius = diameter / 2.0;
                    }
                }
                let removes = usize::from(self.cursor == CursorTool::Remove);
                if let Some(i) =
                    widgets::segmented(ui, Some("Brush"), &["Adds", "Removes"], removes)
                {
                    self.cursor = if i == 0 {
                        CursorTool::Brush
                    } else {
                        CursorTool::Remove
                    };
                }
                widgets::toggle_row(ui, "Show mask", &mut self.show_mask);
                select_hint(ui, &self.sam, self.cursor, self.encoding || self.selecting);
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 8.0) / 2.0;
                    invert = widgets::button(ui, "Invert", idle, Some(width)).clicked();
                    clear =
                        widgets::button(ui, "Clear mask", idle && has_mask, Some(width)).clicked();
                });
            },
        );
        if reset {
            self.cursor = CursorTool::Brush;
            self.show_mask = true;
            if let Some(canvas) = &mut self.canvas {
                canvas.reset_brush();
            }
        }
        if invert {
            self.invert_mask();
        }
        if clear {
            self.clear_mask();
        }

        let summary = match &self.model {
            Status::Ready { model, .. } => model.clone(),
            Status::Loading | Status::NotLoaded => "Loading…".into(),
            Status::Unavailable(_) => "Unavailable".into(),
        };
        widgets::section(
            ui,
            "Model",
            &mut self.sections.model,
            Some(summary),
            false,
            |ui| match &self.model {
                Status::Ready { model, runs_on } => {
                    ui.label(
                        RichText::new(model)
                            .font(FontId::new(14.0, medium()))
                            .color(TEXT),
                    );
                    widgets::info_row(ui, "Runs on", &format!("{THIS_COMPUTER} · {runs_on}"));
                    // Only macOS's models come with the app.
                    if cfg!(target_os = "macos") {
                        widgets::hint(ui, "Included with Gumka.");
                    }
                }
                Status::Loading | Status::NotLoaded => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading LaMa…").size(12.0).color(TEXT_LABEL));
                    });
                }
                Status::Unavailable(reason) => {
                    ui.label(RichText::new("Erase is unavailable").size(12.0).color(WARN));
                    widgets::hint(ui, reason);
                }
            },
        );

        let defaults = EraseSettings::default();
        let s = &mut self.settings;
        let summary = format!("Grow {:.0} px · Feather {:.0} px", s.grow, s.feather);
        if widgets::section(
            ui,
            "Edges",
            &mut self.sections.edges,
            Some(summary),
            true,
            |ui| {
                widgets::slider_row(
                    ui,
                    "Grow mask",
                    &mut s.grow,
                    0.0..=20.0,
                    1.0,
                    " px",
                    0,
                    false,
                );
                widgets::slider_row(
                    ui,
                    "Feather",
                    &mut s.feather,
                    0.0..=20.0,
                    1.0,
                    " px",
                    0,
                    false,
                );
                widgets::hint(
                    ui,
                    "Grow covers the object’s soft outline. Feather fades the fill into the photo.",
                );
            },
        ) {
            s.grow = defaults.grow;
            s.feather = defaults.feather;
        }

        let summary = format!("Context {:.1}×", s.context);
        if widgets::section(
            ui,
            "Advanced",
            &mut self.sections.advanced,
            Some(summary),
            true,
            |ui| {
                widgets::slider_row(ui, "Context", &mut s.context, 1.5..=4.0, 0.5, "×", 1, false);
                widgets::hint(ui, "How much of the photo around the mask the model sees.");
            },
        ) {
            s.context = defaults.context;
        }
    }

    fn history(&mut self, ui: &mut Ui) {
        let Some(doc) = &self.doc else { return };
        let rows = doc.history();
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, BG_SECTION);
        painter.text(
            pos2(rect.left() + 16.0, rect.center().y),
            Align2::LEFT_CENTER,
            "History",
            FontId::new(12.5, medium()),
            Color32::from_gray(0xe4),
        );
        let steps = rows.len() - 1;
        painter.text(
            pos2(rect.right() - 16.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{steps} {}", if steps == 1 { "step" } else { "steps" }),
            FontId::proportional(11.5),
            TEXT_WEAK,
        );
        egui::ScrollArea::vertical()
            .max_height(HISTORY_LIST_HEIGHT)
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(4.0);
                for row in &rows {
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 25.0), Sense::hover());
                    let painter = ui.painter();
                    if row.current {
                        painter.rect_filled(rect, 0.0, BG_CURRENT_ROW);
                    }
                    let color = if row.current {
                        TEXT_STRONG
                    } else if row.undone {
                        TEXT_DISABLED
                    } else {
                        TEXT_LABEL
                    };
                    painter.text(
                        pos2(rect.left() + 16.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        &row.label,
                        FontId::proportional(12.0),
                        color,
                    );
                    painter.text(
                        pos2(rect.right() - 16.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        &row.detail,
                        mono(11.5),
                        if row.undone { TEXT_DISABLED } else { TEXT_WEAK },
                    );
                }
                ui.add_space(6.0);
            });
    }

    // ---- Status bar ----

    fn status_bar(&self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let small = |text: String| RichText::new(text).size(11.5).color(TEXT_WEAK);
            match &self.doc {
                Some(doc) => {
                    let (w, h) = doc.viewed_size();
                    let shown_as_srgb = self.canvas.as_ref().is_some_and(|c| !c.uses_own_profile());
                    let profile = profile_label(
                        doc.picture.has_icc_profile(),
                        doc.picture.profile_name(),
                        shown_as_srgb,
                    );
                    ui.label(small(format!(
                        "{w} × {h} · {}-bit · {profile}",
                        doc.picture.bit_depth()
                    )));
                }
                None => {
                    ui.label(small("No photo open".into()));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(canvas) = &self.canvas {
                    ui.label(
                        RichText::new(format!("{:.0}%", canvas.zoom_percent()))
                            .font(mono(11.5))
                            .color(TEXT_WEAK),
                    );
                }
                if let Some(run) = &self.upscaling {
                    let percent = (run.done * 100).checked_div(run.total).unwrap_or(0);
                    if run.cancel.load(Ordering::Relaxed) {
                        ui.label(small("Stopping upscale…".into()));
                    } else {
                        ui.label(small(format!("Upscaling {percent}%")));
                    }
                } else if let Some((what, seconds)) = self.last_run {
                    ui.label(small(format!("Last {what} {seconds:.1} s")));
                }
                // The model of the tool in use.
                let (status, name) = match self.tool {
                    Tool::Erase => (&self.model, "Erase".to_owned()),
                    Tool::Upscale => {
                        let scale = self.effective_factor().model_scale();
                        (&self.upscalers[upscaler_slot(scale)], upscaler_name(scale))
                    }
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match status {
                        Status::Ready { model, runs_on } => {
                            ui.label(small(format!("{model} ready · {runs_on}")));
                            widgets::dot(ui, OK);
                        }
                        Status::NotLoaded => {
                            ui.label(small(format!("{name} · loads when used")));
                            widgets::dot(ui, OK);
                        }
                        Status::Loading => {
                            ui.label(small("Loading model…".into()));
                            ui.spinner();
                        }
                        Status::Unavailable(reason) => {
                            let tool = self.tool.info().name;
                            ui.label(
                                RichText::new(format!("{tool} unavailable"))
                                    .size(11.5)
                                    .color(WARN),
                            )
                            .on_hover_text(reason);
                        }
                    }
                });
            });
        });
    }

    // ---- Start screen ----

    fn welcome(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().min(1040.0);
                let margin = ((ui.available_width() - width) / 2.0).max(24.0);
                Frame::NONE
                    .inner_margin(Margin {
                        left: margin.min(127.0) as i8,
                        right: margin.min(127.0) as i8,
                        top: 64,
                        bottom: 40,
                    })
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 40.0;
                            let side = 320.0;
                            let main = (ui.available_width().min(1040.0) - side - 40.0).max(360.0);
                            ui.vertical(|ui| {
                                ui.set_width(main);
                                self.drop_zone(ui, &ctx);
                            });
                            ui.vertical(|ui| {
                                ui.set_width(side);
                                ui.spacing_mut().item_spacing = vec2(8.0, 16.0);
                                self.photo_editor_card(ui);
                                self.models_card(ui);
                            });
                        });
                    });
            });
    }

    fn drop_zone(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 300.0), Sense::click());
        let hovered = response.hovered();
        ui.painter().rect_filled(
            rect,
            2.0,
            if hovered {
                Color32::from_gray(0x2a)
            } else {
                BG_PANEL
            },
        );
        widgets::dashed_rect(
            ui,
            rect.shrink(1.0),
            Color32::from_gray(if hovered { 0x77 } else { 0x55 }),
        );
        let center = rect.center();
        icons::PHOTO.paint(
            ui,
            Rect::from_center_size(pos2(center.x, rect.top() + 74.0), vec2(40.0, 40.0)),
            TEXT_WEAK,
        );
        let painter = ui.painter();
        painter.text(
            pos2(center.x, rect.top() + 122.0),
            Align2::CENTER_CENTER,
            "Open a photo to retouch",
            FontId::new(20.0, semibold()),
            TEXT,
        );
        painter.text(
            pos2(center.x, rect.top() + 154.0),
            Align2::CENTER_CENTER,
            "Drop a TIFF, PNG or JPEG here. 16-bit files stay 16-bit,",
            FontId::proportional(13.0),
            Color32::from_gray(0xb5),
        );
        painter.text(
            pos2(center.x, rect.top() + 173.0),
            Align2::CENTER_CENTER,
            "and the color profile and metadata are kept when you save.",
            FontId::proportional(13.0),
            Color32::from_gray(0xb5),
        );
        painter.text(
            pos2(center.x, rect.top() + 192.0),
            Align2::CENTER_CENTER,
            "JPEGs are re-encoded when saved.",
            FontId::proportional(13.0),
            Color32::from_gray(0xb5),
        );
        let button = Rect::from_center_size(pos2(center.x, rect.top() + 234.0), vec2(150.0, 36.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(button));
        let open = widgets::primary_button(
            &mut child,
            "Open…",
            &ctx.format_shortcut(&OPEN),
            true,
            false,
        );
        if open.clicked() || response.clicked() {
            self.request(ctx, Pending::OpenDialog);
        }
    }

    /// Using Gumka as the external editor of a RAW editor. No product is named: any editor
    /// that sends a file and picks it up when it is saved in place works the same way.
    fn photo_editor_card(&self, ui: &mut Ui) {
        widgets::card(ui, |ui| {
            ui.label(
                RichText::new("Use Gumka from your photo editor")
                    .font(FontId::new(14.0, semibold()))
                    .color(TEXT),
            );
            for (i, step) in [
                "In your RAW editor, send the photo to Gumka.",
                "Use a 16-bit TIFF in Adobe RGB or Display P3.",
                "Retouch and save; your editor picks it up.",
            ]
            .iter()
            .enumerate()
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{}.", i + 1))
                            .size(12.5)
                            .color(TEXT_WEAK),
                    );
                    ui.label(
                        RichText::new(*step)
                            .size(12.5)
                            .color(Color32::from_gray(0xcf)),
                    );
                });
            }
        });
    }

    fn models_card(&mut self, ui: &mut Ui) {
        let mut manage = false;
        widgets::card(ui, |ui| {
            ui.label(
                RichText::new("Models")
                    .font(FontId::new(14.0, semibold()))
                    .color(TEXT),
            );
            model_status_row(ui, "LaMa · erase", &self.model);
            model_status_row(ui, "SAM 2.1 · select object", &self.sam);
            // The two upscalers as one row: the worse of their states.
            let rank = |s: &Status| match s {
                Status::Unavailable(_) => 0,
                Status::Loading => 1,
                Status::NotLoaded | Status::Ready { .. } => 2,
            };
            let upscaler = self
                .upscalers
                .iter()
                .min_by_key(|s| rank(s))
                .unwrap_or(&self.upscalers[0]);
            model_status_row(ui, "RealPLKSR · upscale", upscaler);
            if ui
                .add(egui::Link::new(
                    RichText::new("Manage models…").size(12.0).underline(),
                ))
                .clicked()
            {
                manage = true;
            }
        });
        if manage {
            self.show_config();
        }
    }

    // ---- Overlays and dialogs ----

    fn drop_hint(&mut self, ctx: &egui::Context) {
        let (hovering, pointer) = ctx.input(|i| {
            let pointer = i.raw.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::PointerMoved(_) | egui::Event::PointerButton { .. }
                )
            });
            (i.raw.hovered_files.len(), pointer)
        });
        // During a drag the window gets no pointer events: one means the files hovering now
        // were given up on. A new drag adds its files to them.
        if pointer || hovering < self.stale_hover {
            self.stale_hover = hovering;
        }
        if hovering <= self.stale_hover || self.dialog_open() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("drop-hint"),
        ));
        let screen = ctx.content_rect();
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
        painter.text(
            screen.center(),
            Align2::CENTER_CENTER,
            "Drop to open",
            FontId::new(20.0, semibold()),
            TEXT_STRONG,
        );
    }

    /// The message, else "Save changes?", each in a window of its own (see [`panels`]). Once
    /// answered, the window closes, and the next frame runs what waited for the answer.
    fn dialogs(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if let Some(message) = &self.message {
            if panels::message(ui, message) {
                self.message = None;
                ctx.request_repaint();
            }
            return;
        }
        if self.pending.is_none() {
            return;
        }
        let name = self.doc.as_ref().map(Document::name).unwrap_or_default();
        match panels::save_changes(ui, &name) {
            Some(panels::Answer::Save) => self.answer(&ctx, true),
            Some(panels::Answer::DontSave) => self.answer(&ctx, false),
            Some(panels::Answer::Cancel) => self.pending = None,
            None => return,
        }
        ctx.request_repaint();
    }

    /// Turns the main window's zoom button off while a dialog is up, on macOS: it would take the
    /// main window to a fullscreen Space without the dialog, or out of one with the dialog
    /// behind it. Elsewhere maximizing leaves the dialog alone, so the button stays. Only on a
    /// change: a command repaints the window.
    fn zoom_button(&mut self, ctx: &egui::Context) {
        let off = cfg!(target_os = "macos") && self.dialog_open();
        if off != self.zoom_off {
            self.zoom_off = off;
            ctx.send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::EnableButtons {
                    close: true,
                    minimized: true,
                    maximize: !off,
                },
            );
        }
    }
}

fn menu_item(ui: &mut Ui, label: &str, shortcut: &str) -> bool {
    let clicked = ui
        .add(
            egui::Button::new(label)
                .shortcut_text(shortcut)
                .min_size(vec2(180.0, 0.0)),
        )
        .clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn toolbar_separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(13.0, 22.0), Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        Stroke::new(1.0, STROKE_FIELD),
    );
}

impl eframe::App for GumkaApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(canvas) = &self.canvas {
            canvas.filter_input(raw_input);
        }
    }

    /// Here rather than in `ui`, which doesn't run while the window is hidden or in the Dock:
    /// replies still come in then, and a Quit can still close the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A selection lasts while the Select tool is on. Ended before the replies are read,
        // so that one arriving after a tool switch isn't applied.
        if self.cursor != CursorTool::Select {
            self.end_selection();
        }
        self.poll_worker();
        self.run_waiting(ctx);
        // A file another app sent (Finder, a RAW editor's Edit With) opens like one picked in
        // the open panel. It waits while a dialog is up, as dropped files are ignored then.
        if !self.dialog_open()
            && let Some(path) = crate::open_files::take()
        {
            self.sent = Some(path.clone());
            self.request(ctx, Pending::Open(path));
        }
        crate::quit::set_unsaved(self.has_unsaved_work() && !self.allow_close);
        // After the replies, so that a close sees an erase that came back this frame.
        self.handle_close(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.color_space.keep(frame);
        if let Some(path) = self.startup.take() {
            // On Windows and Linux, another app's Edit With hands the file over this way.
            self.sent = Some(path.clone());
            self.open(&ctx, path);
        }
        self.handle_input(&ctx);

        egui::Panel::top("toolbar")
            .exact_size(46.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(BG_TOOLBAR)
                    .inner_margin(Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(BG_STATUS)
                    .inner_margin(Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("rail")
            .exact_size(76.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(BG_RAIL)
                    .inner_margin(Margin::symmetric(6, 8)),
            )
            .show(ui, |ui| self.rail(ui));
        if self.doc.is_some() && self.page == Page::Editor {
            egui::Panel::right("inspector")
                .exact_size(320.0)
                .resizable(false)
                .frame(Frame::NONE.fill(BG_PANEL))
                .show(ui, |ui| self.inspector(ui));
        }
        if self.page == Page::Config {
            egui::CentralPanel::default()
                .frame(Frame::NONE.fill(BG_PANEL))
                .show(ui, |ui| {
                    crate::config_page::show(ui, &self.model_rows, &|info| match info.id.as_str() {
                        gumka_infer::models::LAMA => Some(&self.model),
                        gumka_infer::models::SAM => Some(&self.sam),
                        id => UPSCALERS
                            .iter()
                            .position(|&s| gumka_infer::models::upscaler_id(s) == Ok(id))
                            .map(|slot| &self.upscalers[slot]),
                    })
                });
        } else {
            egui::CentralPanel::default()
                .frame(Frame::NONE.fill(BG_VIEWER))
                .show(ui, |ui| {
                    let no_clicks: &[Click] = &[];
                    // A tool for the whole photo hides the mask, and only pans.
                    let masking = self.tool.input() == Input::Mask;
                    let options = ShowOptions {
                        tool: if masking {
                            self.cursor
                        } else {
                            CursorTool::Pan
                        },
                        can_paint: masking && !self.busy && !self.dialog_open(),
                        show_mask: masking && self.show_mask,
                        compare: self.compare || ui.input(|i| i.key_down(Key::Backslash)),
                        clicks: match &self.selection {
                            Some(s) if masking => &s.clicks,
                            _ => no_clicks,
                        },
                    };
                    let mut click = None;
                    match (&mut self.doc, &mut self.canvas) {
                        (Some(doc), Some(canvas)) => {
                            let events =
                                canvas.show(ui, &doc.picture.pixels, &mut doc.mask, &options);
                            if let Some(stroke) = events.stroke_done {
                                doc.push_stroke(&stroke.before, stroke.rect, stroke.removed);
                            }
                            click = events.click;
                        }
                        _ => self.welcome(ui),
                    }
                    if let Some(click) = click {
                        self.add_click(click);
                    }
                });
        }

        self.drop_hint(&ctx);
        self.dialogs(ui);
        self.zoom_button(&ctx);
        self.update_title(&ctx);
        // Again after anything this frame that saved or changed the picture.
        crate::quit::set_unsaved(self.has_unsaved_work() && !self.allow_close);
    }
}

/// The Selection section's line about clicking objects.
fn select_hint(ui: &mut Ui, sam: &Status, cursor: CursorTool, working: bool) {
    match (sam, cursor) {
        (Status::Ready { model, .. }, CursorTool::Select) => {
            if working {
                ui.horizontal(|ui| {
                    ui.spinner();
                    widgets::hint(ui, "Selecting…");
                });
            }
            widgets::hint(
                ui,
                &format!("{model}: click to include, Option-click to leave out."),
            );
        }
        (Status::Ready { .. }, _) => {
            widgets::hint(ui, "Or press S and click an object to select it.");
        }
        (Status::Loading | Status::NotLoaded, _) => widgets::hint(ui, "Select object is loading…"),
        (Status::Unavailable(_), _) => {}
    }
}

/// A model's name and state, for the start screen.
fn model_status_row(ui: &mut Ui, name: &str, status: &Status) {
    ui.horizontal(|ui| {
        match status {
            Status::Ready { .. } | Status::NotLoaded => widgets::dot(ui, OK),
            Status::Loading => {
                ui.spinner();
            }
            Status::Unavailable(_) => widgets::dot(ui, WARN),
        }
        ui.label(RichText::new(name).size(12.5).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let state = match status {
                Status::Ready { .. } if cfg!(target_os = "macos") => "Ready on this Mac",
                Status::Ready { .. } => "Ready on this computer",
                Status::NotLoaded => FOUND,
                Status::Loading => "Loading…",
                Status::Unavailable(_) => "Unavailable",
            };
            let label = ui.label(RichText::new(state).size(12.0).color(TEXT_WEAK));
            if let Status::Unavailable(reason) = status {
                label.on_hover_text(reason);
            }
        });
    });
}

/// A size in megapixels, as "96 MP" (one decimal below 10).
fn megapixels(w: u32, h: u32) -> String {
    let mp = f64::from(w) * f64::from(h) / 1e6;
    if mp < 10.0 {
        format!("{mp:.1} MP")
    } else {
        format!("{mp:.0} MP")
    }
}

/// The native open and save panels, and the dialogs' windows. Tests never show one: they count
/// the panels instead, and leave the dialogs unanswered unless they give an answer or press a
/// key.
mod panels {
    use std::path::PathBuf;

    use eframe::egui::{self, Key, Ui};

    /// The answer to "Save changes?".
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Answer {
        Save,
        DontSave,
        Cancel,
    }

    /// What Enter and Escape answer in a dialog. Escape always answers, as the window's close
    /// button does.
    #[derive(Clone, Copy)]
    struct Keys<T> {
        enter: Option<T>,
        escape: T,
    }

    impl<T: Copy> Keys<T> {
        fn answer(&self, input: &egui::InputState) -> Option<T> {
            if input.key_pressed(Key::Escape) {
                Some(self.escape)
            } else if input.key_pressed(Key::Enter) {
                self.enter
            } else {
                None
            }
        }
    }

    #[cfg(not(test))]
    pub fn pick_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
        dialog.pick_file()
    }

    #[cfg(not(test))]
    pub fn save_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
        dialog.save_file()
    }

    // Keys pressed in the main window answer a dialog too, as they do in its own window: the
    // main window can have the keyboard meanwhile (the user clicked it, or the system kept the
    // focus there). Read here, outside the dialogs, so that tests cover them.

    /// Shows `message` until it is dismissed. Returns whether it was, this frame.
    pub fn message(ui: &mut Ui, message: &str) -> bool {
        let keys = Keys {
            enter: Some(()),
            escape: (),
        };
        let pressed = ui.input(|i| keys.answer(i));
        pressed
            .or_else(|| message_dialog(ui, message, keys))
            .is_some()
    }

    /// Asks "Save changes to `name`?". Returns the answer in the frame it is given.
    pub fn save_changes(ui: &mut Ui, name: &str) -> Option<Answer> {
        let keys = Keys {
            enter: None,
            escape: Answer::Cancel,
        };
        let pressed = ui.input(|i| keys.answer(i));
        pressed.or_else(|| save_changes_dialog(ui, name, keys))
    }

    #[cfg(not(test))]
    fn message_dialog(ui: &mut Ui, message: &str, keys: Keys<()>) -> Option<()> {
        let id = egui::Id::unique(("message", message));
        let message = message.to_owned();
        dialog(ui, id, keys, move |ui| {
            ui.label(&message);
            ui.add_space(8.0);
            ui.button("OK").clicked().then_some(())
        })
    }

    #[cfg(not(test))]
    fn save_changes_dialog(ui: &mut Ui, name: &str, keys: Keys<Answer>) -> Option<Answer> {
        let question = format!("Save changes to {name}?");
        dialog(ui, egui::Id::unique(&question), keys, move |ui| {
            ui.heading(&question);
            ui.label("Your changes will be lost if you don't save them.");
            ui.add_space(8.0);
            let mut answer = None;
            ui.horizontal(|ui| {
                for (label, button) in [
                    ("Save", Answer::Save),
                    ("Don't Save", Answer::DontSave),
                    ("Cancel", Answer::Cancel),
                ] {
                    if ui.button(label).clicked() {
                        answer = Some(button);
                    }
                }
            });
            answer
        })
    }

    /// The answer given in a dialog's window, until the main window's pass takes it.
    type Answered<T> = std::sync::Arc<egui::mutex::Mutex<Option<T>>>;

    /// Shows `content` in a window of its own, centered on the main window, which stays dimmed
    /// and takes no input meanwhile: clicking or focusing it brings the dialog back to the
    /// front. The answer is a button `content` returns, a key `keys` maps, or the window's
    /// close button (as Escape); the first one given is returned in the main window's next
    /// pass, which leaves the dialog out, and that closes its window.
    ///
    /// `content` only lays the dialog out and returns the button clicked, if any: it runs
    /// whenever the dialog's window repaints, and also unseen (and disabled) in the main
    /// window, to size the dialog. `id` comes from what the dialog says: a window is repainted
    /// only for its own input, so another message is another window.
    fn dialog<T: Copy + Send + Sync + 'static>(
        ui: &mut Ui,
        id: egui::Id,
        keys: Keys<T>,
        content: impl Fn(&mut Ui) -> Option<T> + Send + Sync + 'static,
    ) -> Option<T> {
        const WIDTH: f32 = 420.0;
        const MARGIN: i8 = 16;
        let ctx = ui.ctx().clone();
        let id = egui::ViewportId(id);
        let new = ctx.input(|i| !i.raw.viewports.contains_key(&id));
        let answered = ctx.data_mut(|d| {
            if new {
                // An answer an earlier window gave after it was left out isn't for this one.
                d.remove::<Answered<T>>(id.0);
            }
            d.get_temp_mut_or_default::<Answered<T>>(id.0).clone()
        });
        let closed = ctx.input(|i| {
            i.raw
                .viewports
                .get(&id)
                .is_some_and(egui::ViewportInfo::close_requested)
        });
        let answer = answered.lock().take().or(closed.then_some(keys.escape));
        if answer.is_some() {
            ctx.data_mut(|d| d.remove::<Answered<T>>(id.0));
            return answer;
        }
        if new {
            // Its window is made at the start of the next pass.
            ctx.request_repaint();
        }
        // The window fits its content, laid out unseen at most `WIDTH` wide: rounded up, and a
        // point wider, so that its text wraps in the window as it did here even once the
        // window's size is rounded to whole pixels. Where the window's scale lays the text out
        // wider still, the window grows (below).
        let mut sizing = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(WIDTH, 4096.0),
                ))
                .invisible(),
        );
        content(&mut sizing);
        let size = sizing.min_rect().size().ceil()
            + egui::vec2(1.0, 0.0)
            + egui::Vec2::splat(2.0 * f32::from(MARGIN));
        let mut builder = egui::ViewportBuilder::default()
            .with_title("Gumka")
            .with_app_id(crate::APP_ID)
            .with_inner_size(size)
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false)
            .with_taskbar(false)
            // So that X11 window managers draw a dialog's title bar: close only. (On Wayland
            // winit draws it, with the buttons the desktop's layout names: it takes no others.)
            .with_window_type(egui::X11WindowType::Dialog);
        // Centered on the main window, title bars aside (Wayland doesn't say where windows
        // are), until its window has told its scale and title bar; from then on it stays where
        // it is, however it or the main window is moved.
        let placed = id.0.with("placed");
        if new {
            ctx.data_mut(|d| d.remove::<egui::Pos2>(placed));
        }
        let kept = ctx.data(|d| d.get_temp::<egui::Pos2>(placed));
        let position = kept.or_else(|| {
            let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
            let (outer, inner) = (outer?, inner?);
            let title_bar = title_bar(&ctx, id).unwrap_or(inner.min - outer.min);
            let center = inner.center() * scale_to_main(&ctx, id);
            Some(center - size / 2.0 - title_bar)
        });
        if let Some(position) = position {
            builder = builder.with_position(position);
            if kept.is_none() && told(&ctx, id) {
                ctx.data_mut(|d| d.insert_temp(placed, position));
            }
        }
        let backdrop = egui::Modal::new(id.0)
            .frame(egui::Frame::NONE)
            .show(&ctx, |_| {});
        let focused = ctx.input(|i| i.events.contains(&egui::Event::WindowFocused(true)));
        if backdrop.backdrop_response.clicked() || focused {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        ctx.show_viewport_deferred(id, builder, move |ui, _| {
            let frame = egui::Frame::NONE
                .fill(ui.visuals().window_fill)
                .inner_margin(egui::Margin::same(MARGIN));
            let (clicked, used) = egui::CentralPanel::default()
                .frame(frame)
                .show(ui, |ui| {
                    let clicked = content(ui);
                    (clicked, ui.min_rect().size())
                })
                .inner;
            // Where this window's scale lays the text out wider than the main window's did, it
            // wraps onto more lines: the window grows to fit, so nothing is cut off.
            let has = ui.input(|i| i.viewport().inner_rect.map(|r| r.size()));
            let needed = used + egui::Vec2::splat(2.0 * f32::from(MARGIN));
            if let Some(size) = has.and_then(|has| grown(has, needed)) {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
            let closed = ui.input(|i| i.viewport().close_requested());
            let answer = clicked
                .or_else(|| ui.input(|i| keys.answer(i)))
                .or(closed.then_some(keys.escape));
            if let Some(answer) = answer {
                answered.lock().get_or_insert(answer);
                ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
            }
        });
        None
    }

    /// The size a dialog's window that `has` this size grows to, if its content `needed` more.
    fn grown(has: egui::Vec2, needed: egui::Vec2) -> Option<egui::Vec2> {
        let short = needed.x > has.x + 0.5 || needed.y > has.y + 0.5;
        short.then(|| needed.max(has).ceil())
    }

    /// Whether dialog `id`'s window has told its scale and where it is, title bar and all.
    fn told(ctx: &egui::Context, id: egui::ViewportId) -> bool {
        ctx.input(|i| {
            i.raw.viewports.get(&id).is_some_and(|v| {
                v.native_pixels_per_point.is_some()
                    && v.inner_rect.is_some()
                    && v.outer_rect.is_some()
            })
        })
    }

    /// How far a dialog's content is from its window's corner, once a dialog's window has said:
    /// until then, the main window's offset stands in for it (which is none in fullscreen).
    fn title_bar(ctx: &egui::Context, id: egui::ViewportId) -> Option<egui::Vec2> {
        let remembered = egui::Id::unique("dialog title bar");
        let own = ctx.input(|i| {
            let dialog = i.raw.viewports.get(&id)?;
            Some(dialog.inner_rect?.min - dialog.outer_rect?.min)
        });
        ctx.data_mut(|d| match own {
            Some(offset) => {
                d.insert_temp(remembered, offset);
                Some(offset)
            }
            None => d.get_temp(remembered),
        })
    }

    /// What the main window's positions are scaled by to be dialog `id`'s (sizes are the same
    /// on every monitor). Positions are a window's pixels over its own scale, and a new window
    /// takes the scale of the monitor it is first put on (on X11 the one under the pointer):
    /// where that isn't the main window's, the dialog moves once its window tells its scale.
    /// On macOS positions are in points on every monitor.
    fn scale_to_main(ctx: &egui::Context, id: egui::ViewportId) -> f32 {
        if cfg!(target_os = "macos") {
            return 1.0;
        }
        ctx.input(|i| {
            let main = i.viewport().native_pixels_per_point?;
            let dialog = i.raw.viewports.get(&id)?.native_pixels_per_point?;
            Some(main / dialog)
        })
        .unwrap_or(1.0)
    }

    #[cfg(test)]
    thread_local! {
        /// Panels shown on this thread (each test runs on its own).
        pub static SHOWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The next answer to "Save changes?", taken when the question is shown.
        pub static ANSWER: std::cell::Cell<Option<Answer>> = const { std::cell::Cell::new(None) };
        /// Whether the next message shown is dismissed, as with its OK button.
        pub static DISMISS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(test)]
    fn message_dialog(_: &mut Ui, _: &str, _: Keys<()>) -> Option<()> {
        DISMISS.with(std::cell::Cell::take).then_some(())
    }

    #[cfg(test)]
    fn save_changes_dialog(_: &mut Ui, _: &str, _: Keys<Answer>) -> Option<Answer> {
        ANSWER.with(std::cell::Cell::take)
    }

    #[cfg(test)]
    pub fn pick_file(_: rfd::FileDialog) -> Option<PathBuf> {
        SHOWN.with(|n| n.set(n.get() + 1));
        None
    }

    #[cfg(test)]
    pub fn save_file(_: rfd::FileDialog) -> Option<PathBuf> {
        SHOWN.with(|n| n.set(n.get() + 1));
        None
    }

    /// The dialog's window as the main window asks for it, in headless passes: egui records
    /// the window it would open (its builder and its own pass) without opening one.
    #[cfg(test)]
    mod tests {
        use super::*;
        use egui::{Rect, ViewportCommand, ViewportId, ViewportInfo, pos2, vec2};

        fn id() -> egui::Id {
            egui::Id::unique("a test dialog")
        }

        /// A window at `outer`, with a title bar 30 points high, of scale 1.
        fn window(outer: Rect) -> ViewportInfo {
            ViewportInfo {
                native_pixels_per_point: Some(1.0),
                outer_rect: Some(outer),
                inner_rect: Some(outer.with_min_y(outer.min.y + 30.0)),
                ..Default::default()
            }
        }

        /// A main window's pass, the main window at `main` and the dialog's window telling
        /// `opened`, once it has one. Returns what the dialog's window is asked to be.
        fn pass(
            ctx: &egui::Context,
            main: Rect,
            opened: Option<ViewportInfo>,
        ) -> egui::ViewportOutput {
            let mut input = egui::RawInput::default();
            input.viewports.insert(ViewportId::ROOT, window(main));
            if let Some(info) = opened {
                input.viewports.insert(ViewportId(id()), info);
            }
            let keys = Keys {
                enter: Some(()),
                escape: (),
            };
            let mut output = ctx.run_ui(input, |ui| {
                dialog(ui, id(), keys, |ui| {
                    ui.label("Couldn't open the photo.");
                    ui.button("OK").clicked().then_some(())
                });
            });
            let window = output
                .viewport_output
                .remove(&ViewportId(id()))
                .expect("the dialog has a window");
            output.drop_without_applying_deltas();
            window
        }

        /// A dialog opens centered on the main window, and once its window has told where it
        /// is, it stays: neither the main window moving nor the dialog being dragged pulls it
        /// back.
        #[test]
        fn a_dialog_is_placed_once_then_stays() {
            let ctx = egui::Context::default();
            ctx.set_embed_viewports(false);
            let main = Rect::from_min_size(pos2(100.0, 100.0), vec2(800.0, 600.0));
            let first = pass(&ctx, main, None).builder;
            let (at, size) = (first.position.unwrap(), first.inner_size.unwrap());
            let center = main.with_min_y(130.0).center();
            assert_eq!(
                at + size / 2.0 + vec2(0.0, 30.0),
                center,
                "centered, title bars aside"
            );

            let opened = window(Rect::from_min_size(at, size + vec2(0.0, 30.0)));
            assert_eq!(
                pass(&ctx, main, Some(opened.clone())).builder.position,
                Some(at)
            );
            let dragged = window(opened.outer_rect.unwrap().translate(vec2(-50.0, 40.0)));
            let moved = main.translate(vec2(300.0, 0.0));
            assert_eq!(
                pass(&ctx, moved, Some(dragged)).builder.position,
                Some(at),
                "it stays where it is"
            );
        }

        #[test]
        fn a_window_grows_only_for_more_than_rounding() {
            let has = vec2(300.0, 120.0);
            assert_eq!(grown(has, has), None);
            assert_eq!(
                grown(has, vec2(300.3, 119.0)),
                None,
                "less than a pixel's rounding"
            );
            assert_eq!(
                grown(has, vec2(300.0, 138.4)),
                Some(vec2(300.0, 139.0)),
                "a line more"
            );
            assert_eq!(grown(has, vec2(301.2, 100.0)), Some(vec2(302.0, 120.0)));
        }

        /// A dialog's window too small for its content (its scale wrapped the text onto one
        /// more line) asks to grow to fit; one that fits asks nothing.
        #[test]
        fn a_window_too_small_for_its_dialog_grows() {
            let ctx = egui::Context::default();
            ctx.set_embed_viewports(false);
            let main = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
            let asked = pass(&ctx, main, None);
            let size = asked.builder.inner_size.unwrap();
            let ui = asked.viewport_ui_cb.expect("the window's pass");
            let own_pass = |height: f32| {
                let mut input = egui::RawInput {
                    viewport_id: ViewportId(id()),
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(size.x, height))),
                    ..Default::default()
                };
                let inner = Rect::from_min_size(pos2(0.0, 30.0), vec2(size.x, height));
                input.viewports.insert(
                    ViewportId(id()),
                    ViewportInfo {
                        native_pixels_per_point: Some(1.0),
                        inner_rect: Some(inner),
                        outer_rect: Some(inner.with_min_y(0.0)),
                        ..Default::default()
                    },
                );
                let mut output = ctx.run_ui(input, |ui_| ui(ui_));
                let commands = output
                    .viewport_output
                    .remove(&ViewportId(id()))
                    .map(|v| v.commands)
                    .unwrap_or_default();
                output.drop_without_applying_deltas();
                commands.into_iter().find_map(|c| match c {
                    ViewportCommand::InnerSize(size) => Some(size),
                    _ => None,
                })
            };
            assert_eq!(own_pass(size.y), None, "it fits");
            let grown = own_pass(size.y - 20.0).expect("too small: it asks for more");
            assert!(
                grown.y >= size.y && grown.x >= size.x,
                "{grown:?} for {size:?}"
            );
        }
    }
}

/// The status bar's word on the picture's color profile: its name, and whether it is shown as
/// sRGB instead (when lcms2 can't use it).
fn profile_label(has_profile: bool, name: Option<String>, shown_as_srgb: bool) -> String {
    match (has_profile, name) {
        (false, _) => "No color profile (shown as sRGB)".into(),
        (true, Some(name)) if shown_as_srgb => format!("{name} (unreadable, shown as sRGB)"),
        (true, Some(name)) => name,
        (true, None) if shown_as_srgb => "Unreadable color profile (shown as sRGB)".into(),
        (true, None) => "Unnamed color profile".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use eframe::App as _;
    use eframe::egui::{Event, RawInput, ViewportCommand, ViewportEvent, ViewportId, ViewportInfo};
    use gumka_core::image::{Luma, Rgb};
    use gumka_core::picture::Picture;
    use gumka_core::{Mask, Rgb16};

    use super::*;
    use crate::worker::Erased;
    use crate::worker::tests::{Fake, fake};

    /// [`GumkaApp::new`] with a fake worker.
    fn app(ctx: &egui::Context, worker: Worker) -> GumkaApp {
        crate::theme::apply(ctx);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        egui_extras::install_image_loaders(ctx);
        GumkaApp {
            doc: None,
            canvas: None,
            color_space: ColorSpaceTag::new(),
            worker,
            model: Status::Loading,
            sam: Status::Loading,
            max_clicks: usize::MAX,
            selection: None,
            encoded: None,
            encoding: false,
            select_id: 0,
            selecting: false,
            busy: false,
            upscalers: std::array::from_fn(|_| Status::Loading),
            factor: Factor::X2,
            upscaling: None,
            last_run: None,
            message: None,
            pending: None,
            waiting_save: None,
            waiting: None,
            allow_close: false,
            title: String::new(),
            tool: Tool::Erase,
            cursor: CursorTool::Brush,
            show_mask: true,
            compare: false,
            settings: EraseSettings::default(),
            page: Page::Editor,
            model_rows: Vec::new(),
            sections: Sections {
                selection: true,
                model: true,
                edges: false,
                advanced: false,
                scale: true,
            },
            startup: None,
            sent: None,
            stale_hover: 0,
            zoom_off: false,
        }
    }

    /// The app without a window: frames are run by hand, and the test plays the worker.
    struct Harness {
        ctx: egui::Context,
        app: GumkaApp,
        fake: Fake,
        time: f64,
        dir: PathBuf,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gumka-app-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let ctx = egui::Context::default();
            let (worker, fake) = fake();
            let app = app(&ctx, worker);
            Self {
                ctx,
                app,
                fake,
                time: 0.0,
                dir,
            }
        }

        /// Writes a `width` by `height` PNG with a gradient, and returns its path.
        fn picture(&self, name: &str, width: u32, height: u32) -> PathBuf {
            let pixels = Rgb16::from_fn(width, height, |x, y| {
                let v = ((x * 7 + y * 3) % 256) as u16 * 257;
                Rgb([v, v, v])
            });
            let path = self.dir.join(name);
            Picture::from_pixels(pixels, 8).save(&path).unwrap();
            path
        }

        fn open(&mut self, path: &Path) {
            self.app.open(&self.ctx, path.to_path_buf());
            assert_eq!(self.doc().path, path, "opened");
        }

        fn doc(&self) -> &Document {
            self.app.doc.as_ref().expect("a photo is open")
        }

        fn ready(&mut self) {
            let ready = || Status::Ready {
                model: "Test".into(),
                runs_on: "here".into(),
            };
            self.fake.reply(Reply::Status(Which::Inpainter, ready()));
            self.fake.reply(Reply::Status(Which::Segmenter, ready()));
            for scale in [2, 4] {
                self.fake
                    .reply(Reply::Status(Which::Upscaler(scale), Status::NotLoaded));
            }
            self.app.poll_worker();
        }

        /// Runs one frame, as eframe does: `logic`, then `ui`. Returns the window commands.
        fn frame_with(&mut self, input: RawInput) -> Vec<ViewportCommand> {
            self.run(input, |output| {
                output
                    .viewport_output
                    .remove(&ViewportId::ROOT)
                    .map(|v| v.commands)
                    .unwrap_or_default()
            })
        }

        /// Runs one frame as [`Self::frame_with`] does. Returns the painted shapes.
        fn shapes(&mut self, input: RawInput) -> Vec<egui::Shape> {
            self.run(input, |output| {
                output.shapes.iter().map(|s| s.shape.clone()).collect()
            })
        }

        /// Runs one frame in a 1280 x 860 window and returns what `take` takes from its output.
        fn run<T>(
            &mut self,
            mut input: RawInput,
            take: impl FnOnce(&mut egui::FullOutput) -> T,
        ) -> T {
            self.time += 1.0 / 60.0;
            input.time = Some(self.time);
            input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 860.0)));
            input.max_texture_side = Some(8192);
            let mut frame = eframe::Frame::_new_kittest();
            let app = &mut self.app;
            let mut output = self.ctx.run_ui(input, |ui| {
                app.logic(ui.ctx(), &mut frame);
                app.ui(ui, &mut frame);
            });
            let taken = take(&mut output);
            output.drop_without_applying_deltas();
            taken
        }

        fn frame(&mut self, events: Vec<Event>) -> Vec<ViewportCommand> {
            self.frame_with(RawInput {
                events,
                ..Default::default()
            })
        }

        fn drop_file(&mut self, path: &Path) {
            self.frame_with(RawInput {
                dropped_files: vec![std::sync::Arc::new(Dropped(path.to_path_buf()))],
                ..Default::default()
            });
        }

        /// The window's close button (or a Quit, see quit.rs).
        fn close(&mut self) -> Vec<ViewportCommand> {
            let mut input = RawInput::default();
            input.viewports.insert(
                ViewportId::ROOT,
                ViewportInfo {
                    events: vec![ViewportEvent::Close],
                    ..Default::default()
                },
            );
            self.frame_with(input)
        }

        /// Selects with one click and shows the result: the whole picture.
        fn select_all(&mut self, at: Click) {
            self.app.add_click(at);
            if self.app.encoding {
                self.fake.encode();
                self.app.poll_worker();
            }
            let (id, _, size) = self.fake.select();
            self.fake.reply(Reply::Selected {
                id,
                result: Ok((filled(size), 0.1)),
            });
            self.app.poll_worker();
        }

        /// Paints the top-left pixel into the mask and erases it: an unsaved change.
        fn erase(&mut self) {
            let doc = self.app.doc.as_mut().unwrap();
            let before = doc.mask.clone();
            doc.mask.put_pixel(0, 0, Luma([255]));
            doc.push_stroke(&before, gumka_core::Rect::new(0, 0, 1, 1), false);
            self.app.run_tool();
            let (mut pixels, _) = self.fake.erase();
            let rect = gumka_core::Rect::new(0, 0, 1, 1);
            let old = Rgb16::from_pixel(1, 1, *pixels.get_pixel(0, 0));
            pixels.put_pixel(0, 0, Rgb([1, 2, 3]));
            self.fake.reply(Reply::Erased {
                pixels,
                result: Ok(Some(Erased {
                    rect,
                    old,
                    seconds: 0.1,
                })),
            });
            self.app.poll_worker();
            assert!(self.app.is_dirty());
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            // Hand egui the textures of the photo, so that none are left unapplied.
            self.app.doc = None;
            self.app.canvas = None;
            self.ctx
                .run_ui(RawInput::default(), |_| {})
                .drop_without_applying_deltas();
        }
    }

    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &Path {
            &self.0
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    fn click(x: f32, y: f32) -> Click {
        Click {
            x,
            y,
            include: true,
        }
    }

    fn filled((width, height): (u32, u32)) -> Mask {
        Mask::from_pixel(width, height, Luma([255]))
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn modified(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    // ---- A1: a selection never reaches another picture ----

    #[test]
    fn opening_another_picture_leaves_the_old_selection_behind() {
        let mut h = Harness::new("open-during-select");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(10.0, 10.0));
        let first = h.fake.encode();
        h.app.poll_worker();
        let (id, clicks, size) = h.fake.select();
        assert_eq!((clicks.len(), size), (1, (40, 30)));

        // The other picture opens while the selection is on its way.
        h.open(&b);
        assert!(h.app.selection.is_none());
        h.fake.reply(Reply::Selected {
            id,
            result: Ok((filled((40, 30)), 0.1)),
        });
        h.app.poll_worker();
        assert!(!h.doc().has_mask(), "the old picture's mask isn't applied");
        assert!(!h.doc().can_undo());
        assert!(h.app.message.is_none());

        // The first click on it encodes it, and selects with that click alone.
        h.app.add_click(click(5.0, 6.0));
        let second = h.fake.encode();
        assert_ne!(second, first);
        assert_eq!(second, h.doc().generation());
        h.app.poll_worker();
        let (id2, clicks, size) = h.fake.select();
        assert_ne!(id2, id);
        assert_eq!(clicks, vec![click(5.0, 6.0)]);
        assert_eq!(size, (24, 36));
        h.fake.reply(Reply::Selected {
            id: id2,
            result: Ok((filled((24, 36)), 0.1)),
        });
        h.app.poll_worker();
        assert!(h.doc().has_mask());

        // Erase gets the new picture with a mask of its size.
        h.app.run_tool();
        let (pixels, mask) = h.fake.erase();
        assert_eq!(pixels.dimensions(), (24, 36));
        assert_eq!(mask.dimensions(), (24, 36));
    }

    #[test]
    fn a_selection_reply_after_switching_tools_is_ignored() {
        let mut h = Harness::new("switch-tools");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.frame(vec![]);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(10.0, 10.0));
        h.fake.encode();
        h.app.poll_worker();
        let (id, _, size) = h.fake.select();
        // B is pressed, and the reply comes in before the next frame.
        h.app.cursor = CursorTool::Brush;
        h.fake.reply(Reply::Selected {
            id,
            result: Ok((filled(size), 0.1)),
        });
        h.frame(vec![]);
        assert!(!h.doc().has_mask());
        assert!(!h.doc().can_undo());
        assert!(h.app.selection.is_none());
        assert!(!h.app.selecting, "no spinner left behind");
    }

    #[test]
    fn a_selection_reply_from_before_an_undo_is_ignored() {
        let mut h = Harness::new("undo-during-select");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(10.0, 10.0));
        h.fake.encode();
        h.app.poll_worker();
        let (old, _, size) = h.fake.select();

        // Undoing the erase changes the pixels, so the next click encodes them again.
        h.app.undo_redo(true);
        h.app.add_click(click(20.0, 20.0));
        assert!(h.app.encoding);
        let mask = h.doc().mask.clone();
        h.fake.reply(Reply::Selected {
            id: old,
            result: Ok((filled(size), 0.1)),
        });
        h.app.poll_worker();
        assert_eq!(h.doc().mask, mask, "the old reply isn't applied");
        assert!(h.doc().can_redo(), "nothing new in the history");

        h.fake.encode();
        h.app.poll_worker();
        let (id, clicks, _) = h.fake.select();
        assert_ne!(id, old);
        assert_eq!(clicks, vec![click(20.0, 20.0)]);
    }

    #[test]
    fn a_selection_mask_of_another_size_is_refused() {
        let mut h = Harness::new("wrong-size");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(10.0, 10.0));
        h.fake.encode();
        h.app.poll_worker();
        let (id, _, _) = h.fake.select();
        h.fake.reply(Reply::Selected {
            id,
            result: Ok((filled((30, 40)), 0.1)),
        });
        h.app.poll_worker();
        assert!(!h.doc().has_mask());
        assert!(!h.doc().can_undo());
        assert!(h.app.selection.is_none(), "its only click is dropped");
        let message = h.app.message.as_deref().unwrap_or_default();
        assert!(
            message.contains("doesn't match the picture's size"),
            "{message}"
        );
    }

    // ---- A2: closing and quitting ask about unsaved changes ----

    #[test]
    fn closing_with_unsaved_changes_asks_first() {
        let mut h = Harness::new("close");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        let commands = h.close();
        assert!(commands.contains(&ViewportCommand::CancelClose));
        assert!(matches!(h.app.pending, Some(Pending::Quit)));

        // Don't Save: the window closes for good.
        let action = h.app.pending.take().unwrap();
        h.app.run_pending(&h.ctx, action);
        assert!(h.app.allow_close);
        let commands = h.close();
        assert!(!commands.contains(&ViewportCommand::CancelClose));
    }

    #[test]
    fn closing_a_hidden_window_with_unsaved_changes_asks_first() {
        let mut h = Harness::new("close-hidden");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        h.frame(vec![]);
        // While the window is hidden or in the Dock, eframe runs only `logic`.
        let mut input = RawInput::default();
        input.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                events: vec![ViewportEvent::Close],
                minimized: Some(true),
                ..Default::default()
            },
        );
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut h.app;
        let output = h.ctx.run_logic(&input, |ctx| app.logic(ctx, &mut frame));
        let commands = output
            .viewport_commands
            .get(&ViewportId::ROOT)
            .cloned()
            .unwrap_or_default();
        assert!(commands.contains(&ViewportCommand::CancelClose));
        assert!(matches!(h.app.pending, Some(Pending::Quit)));
    }

    #[test]
    fn closing_without_unsaved_changes_just_closes() {
        let mut h = Harness::new("close-clean");
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let commands = h.close();
        assert!(!commands.contains(&ViewportCommand::CancelClose));
        assert!(h.app.pending.is_none());
    }

    /// The answer to "Save changes?" is acted on in the frame it comes, whichever it is.
    #[test]
    fn the_answer_to_save_changes_is_acted_on() {
        let mut h = Harness::new("close-answer");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        h.close();
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "asks until answered"
        );

        panels::ANSWER.with(|answer| answer.set(Some(panels::Answer::Cancel)));
        h.frame(vec![]);
        assert!(
            h.app.pending.is_none() && !h.app.allow_close,
            "Cancel stays"
        );
        assert!(h.doc().is_dirty());

        panels::ANSWER.with(|answer| answer.set(Some(panels::Answer::Save)));
        let commands = h.close();
        assert!(!h.doc().is_dirty(), "saved");
        assert!(h.app.allow_close && commands.contains(&ViewportCommand::Close));

        let mut h = Harness::new("close-answer-dont-save");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        let bytes = std::fs::read(&a).unwrap();
        h.open(&a);
        h.erase();
        panels::ANSWER.with(|answer| answer.set(Some(panels::Answer::DontSave)));
        let commands = h.close();
        assert!(h.app.allow_close && commands.contains(&ViewportCommand::Close));
        assert_eq!(std::fs::read(&a).unwrap(), bytes, "not saved");
    }

    /// Enter or Escape dismisses a message; Escape cancels "Save changes?", which leaves Enter
    /// alone.
    #[test]
    fn enter_and_escape_answer_the_dialogs() {
        let answers = |event: Event| {
            let ctx = egui::Context::default();
            let mut answers = (false, None);
            let input = RawInput {
                events: vec![event],
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                answers = (
                    panels::message(ui, "Hello"),
                    panels::save_changes(ui, "a.png"),
                );
            })
            .drop_without_applying_deltas();
            answers
        };
        assert_eq!(answers(key(Key::Enter, Modifiers::NONE)), (true, None));
        let cancel = Some(panels::Answer::Cancel);
        assert_eq!(answers(key(Key::Escape, Modifiers::NONE)), (true, cancel));
        assert_eq!(answers(key(Key::A, Modifiers::NONE)), (false, None));
    }

    /// The main window can have the keyboard while a dialog's window is up: its keys answer
    /// the dialog.
    #[test]
    fn keys_in_the_main_window_answer_the_dialogs() {
        let mut h = Harness::new("dialog-keys");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        h.close();
        h.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "Enter isn't an answer"
        );
        h.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(
            h.app.pending.is_none() && !h.app.allow_close,
            "Escape cancels"
        );
        assert!(h.doc().is_dirty());

        h.app.message = Some("Hello".into());
        h.frame(vec![]);
        assert!(h.app.message.is_some());
        h.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(h.app.message.is_none(), "Enter dismisses a message");
    }

    /// Dismissing a message lets what waited for it go on: here an open that waited for a save
    /// that failed, which then asks about the unsaved changes.
    #[test]
    fn dismissing_a_message_lets_what_waited_for_it_run() {
        let mut h = Harness::new("dismiss");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        let pixels = start_erase(&mut h);
        assert!(!h.app.save_to(h.dir.join("missing").join("a.png")));
        h.drop_file(&b);
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert!(h.app.message.as_deref().unwrap().contains("Couldn't save"));
        h.frame(vec![]);
        assert!(h.app.message.is_some() && h.app.pending.is_none(), "waits");

        panels::DISMISS.with(|dismiss| dismiss.set(true));
        h.frame(vec![]);
        assert!(h.app.message.is_none(), "dismissed");
        h.frame(vec![]);
        assert!(matches!(h.app.pending, Some(Pending::Open(_))), "asks");
        panels::ANSWER.with(|answer| answer.set(Some(panels::Answer::DontSave)));
        h.frame(vec![]);
        assert_eq!(h.doc().path, b, "then opens");
    }

    /// On macOS the main window's zoom button is off while a dialog is up, however the dialog
    /// goes. Elsewhere maximizing leaves the dialog alone, and the button stays on.
    #[test]
    fn the_zoom_button_is_off_on_macos_while_a_dialog_is_up() {
        let zoom = |on| ViewportCommand::EnableButtons {
            close: true,
            minimized: true,
            maximize: on,
        };
        let buttons = |c: &ViewportCommand| matches!(c, ViewportCommand::EnableButtons { .. });
        let mut h = Harness::new("zoom");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        if !cfg!(target_os = "macos") {
            assert!(!h.close().iter().any(buttons), "not off here");
            h.app.message = Some("Hello".into());
            assert!(!h.frame(vec![]).iter().any(buttons));
            return;
        }
        assert!(h.close().contains(&zoom(false)));
        let commands = h.frame(vec![]);
        assert!(!commands.iter().any(buttons), "only on a change");
        panels::ANSWER.with(|answer| answer.set(Some(panels::Answer::Cancel)));
        assert!(h.frame(vec![]).contains(&zoom(true)), "answered");

        h.app.message = Some("Hello".into());
        assert!(h.frame(vec![]).contains(&zoom(false)));
        h.app.message = None;
        assert!(h.frame(vec![]).contains(&zoom(true)), "gone unanswered");
    }

    // ---- A3: a failing or stopped worker ----

    #[test]
    fn a_failed_erase_gives_the_pixels_back() {
        let mut h = Harness::new("erase-failed");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let doc = h.app.doc.as_mut().unwrap();
        doc.mask.put_pixel(3, 3, Luma([255]));
        doc.push_stroke(&Mask::new(40, 30), gumka_core::Rect::new(3, 3, 1, 1), false);
        h.app.run_tool();
        assert!(h.app.busy);
        let (pixels, _) = h.fake.erase();
        h.fake.reply(Reply::Erased {
            pixels,
            result: Err("internal error: boom".into()),
        });
        h.app.poll_worker();
        assert!(!h.app.busy);
        assert_eq!(h.doc().picture.pixels.dimensions(), (40, 30));
        assert!(!h.app.is_dirty());
        assert!(h.app.message.as_deref().unwrap().contains("boom"));
        // And the photo can still be saved.
        assert!(h.app.save());
    }

    #[test]
    fn a_stopped_worker_closes_the_photo_it_had() {
        let mut h = Harness::new("stopped-busy");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let doc = h.app.doc.as_mut().unwrap();
        doc.mask.put_pixel(3, 3, Luma([255]));
        doc.push_stroke(&Mask::new(40, 30), gumka_core::Rect::new(3, 3, 1, 1), false);
        h.app.run_tool();
        let _ = h.fake.erase();
        h.fake.reply(Reply::Stopped("boom".into()));
        h.app.poll_worker();
        assert!(h.app.doc.is_none() && h.app.canvas.is_none());
        assert!(!h.app.busy, "nothing waits forever");
        assert!(!h.app.model.is_ready() && !h.app.sam.is_ready());
        assert!(h.app.message.as_deref().unwrap().contains("boom"));
        h.frame(vec![]);
    }

    #[test]
    fn a_worker_that_stops_while_idle_keeps_the_photo() {
        let mut h = Harness::new("stopped-idle");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.fake.reply(Reply::Stopped("boom".into()));
        h.app.poll_worker();
        assert!(h.app.doc.is_some());
        assert!(!h.app.sam.is_ready());
        h.app.run_tool();
        assert!(!h.app.busy);
        assert!(h.app.message.as_deref().unwrap().contains("unavailable"));
    }

    // ---- A4: the command-line file ----

    #[test]
    fn the_command_line_file_opens_on_the_first_frame_at_full_resolution() {
        let mut h = Harness::new("startup");
        let wide = h.picture("wide.png", 5000, 8);
        h.app.startup = Some(wide.clone());
        h.frame(vec![]);
        assert_eq!(h.doc().path, wide);
        let canvas = h.app.canvas.as_ref().unwrap();
        assert_eq!(crate::canvas::tests::original_finest_factor(canvas), 1);
        assert!(h.app.startup.is_none());
    }

    // ---- A5: the click limit ----

    #[test]
    fn the_click_after_the_limit_is_refused() {
        let mut h = Harness::new("click-limit");
        h.ready();
        h.fake.reply(Reply::MaxClicks(3));
        h.app.poll_worker();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        for i in 0..3 {
            h.select_all(click(i as f32, 1.0));
            assert!(h.app.message.is_none());
        }
        // The second and third clicks change nothing: they leave no step, but still count.
        assert_eq!(h.doc().history().len(), 2);
        h.app.add_click(click(9.0, 9.0));
        assert!(h.fake.idle(), "the fourth click isn't sent");
        assert_eq!(h.app.selection.as_ref().unwrap().clicks.len(), 3);
        let message = h.app.message.take().unwrap();
        assert!(message.contains("up to 3 clicks"), "{message}");

        // Another tool and back start a new selection.
        h.app.cursor = CursorTool::Brush;
        h.frame(vec![]);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(9.0, 9.0));
        let (_, clicks, _) = h.fake.select();
        assert_eq!(clicks, vec![click(9.0, 9.0)]);
    }

    #[test]
    fn a_failed_click_is_dropped_from_the_selection() {
        let mut h = Harness::new("click-failed");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.select_all(click(1.0, 1.0));
        h.app.add_click(click(2.0, 2.0));
        let (id, _, _) = h.fake.select();
        h.fake.reply(Reply::Selected {
            id,
            result: Err("too many".into()),
        });
        h.app.poll_worker();
        let selection = h.app.selection.as_ref().unwrap();
        assert_eq!(selection.clicks, vec![click(1.0, 1.0)]);
        assert!(h.app.message.take().unwrap().contains("too many"));
        h.app.add_click(click(3.0, 3.0));
        let (_, clicks, _) = h.fake.select();
        assert_eq!(clicks, vec![click(1.0, 1.0), click(3.0, 3.0)]);
    }

    /// A click whose mask is the same as before is shown all the same: a later failure keeps
    /// it (C8).
    #[test]
    fn a_click_that_changes_nothing_still_counts_as_shown() {
        let mut h = Harness::new("click-no-change");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.select_all(click(1.0, 1.0));
        h.select_all(click(2.0, 2.0));
        assert!(h.app.message.is_none());
        h.app.add_click(click(3.0, 3.0));
        let (id, _, _) = h.fake.select();
        h.fake.reply(Reply::Selected {
            id,
            result: Err("no".into()),
        });
        h.app.poll_worker();
        h.app.message = None;
        let selection = h.app.selection.as_ref().unwrap();
        assert_eq!(selection.clicks, vec![click(1.0, 1.0), click(2.0, 2.0)]);
        assert_eq!(h.doc().history().len(), 2, "one step, for the first click");
    }

    #[test]
    fn a_failed_first_click_ends_the_selection() {
        let mut h = Harness::new("first-click-failed");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(1.0, 1.0));
        h.fake.encode();
        h.app.poll_worker();
        let (id, _, _) = h.fake.select();
        h.fake.reply(Reply::Selected {
            id,
            result: Err("no".into()),
        });
        h.app.poll_worker();
        assert!(h.app.selection.is_none());
        assert!(!h.doc().can_undo());
    }

    // ---- A6: files dropped while a dialog is open ----

    #[test]
    fn files_dropped_on_a_dialog_are_ignored() {
        let mut h = Harness::new("drop");
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        h.app.message = Some("Hello".into());
        h.drop_file(&b);
        assert_eq!(h.doc().path, a);
        assert_eq!(h.app.message.as_deref(), Some("Hello"));

        h.app.message = None;
        h.app.pending = Some(Pending::Quit);
        h.drop_file(&b);
        assert_eq!(h.doc().path, a);
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "still quitting"
        );

        h.app.pending = None;
        h.drop_file(&b);
        assert_eq!(h.doc().path, b, "without a dialog the drop opens");
    }

    // ---- A8: zoom keys ----

    #[test]
    fn command_plus_zooms_the_photo() {
        let mut h = Harness::new("zoom");
        // ⌘+ with no photo open does nothing.
        h.frame(vec![key(Key::Plus, Modifiers::COMMAND | Modifiers::SHIFT)]);
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.frame(vec![]);
        let zoom = || h.app.canvas.as_ref().unwrap().zoom_percent();
        let start = zoom();
        // Shift-= on a US keyboard.
        h.frame(vec![Event::Key {
            key: Key::Plus,
            physical_key: Some(Key::Equals),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND | Modifiers::SHIFT,
        }]);
        assert_eq!(h.app.canvas.as_ref().unwrap().zoom_percent(), start * 2.0);
        // Its own key on a German keyboard.
        h.frame(vec![key(Key::Plus, Modifiers::COMMAND)]);
        assert_eq!(h.app.canvas.as_ref().unwrap().zoom_percent(), start * 4.0);
        h.frame(vec![key(Key::Equals, Modifiers::COMMAND)]);
        assert_eq!(h.app.canvas.as_ref().unwrap().zoom_percent(), start * 8.0);
        h.frame(vec![key(Key::Minus, Modifiers::COMMAND)]);
        assert_eq!(h.app.canvas.as_ref().unwrap().zoom_percent(), start * 4.0);
        assert_eq!(h.ctx.zoom_factor(), 1.0, "the UI keeps its size");
    }

    // ---- A13: Save needs unsaved changes ----

    #[test]
    fn save_waits_for_unsaved_changes() {
        let mut h = Harness::new("save");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        assert_eq!(modified(&a), opened, "nothing to save");

        h.erase();
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        let saved = modified(&a);
        assert_ne!(saved, opened, "saved");
        assert!(!h.app.is_dirty());
        std::thread::sleep(Duration::from_millis(20));
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        assert_eq!(modified(&a), saved, "nothing new to save");
    }

    // ---- What waits while the worker has the pixels ----

    /// Paints a mask pixel and starts erasing it; the test answers with [`erased`].
    fn start_erase(h: &mut Harness) -> Rgb16 {
        let doc = h.app.doc.as_mut().unwrap();
        let before = doc.mask.clone();
        doc.mask.put_pixel(1, 1, Luma([255]));
        doc.push_stroke(&before, gumka_core::Rect::new(1, 1, 1, 1), false);
        h.app.run_tool();
        assert!(h.app.busy);
        h.fake.erase().0
    }

    /// The worker's answer to [`start_erase`]: pixel (1, 1) changed.
    fn erased(mut pixels: Rgb16) -> Reply {
        let old = Rgb16::from_pixel(1, 1, *pixels.get_pixel(1, 1));
        pixels.put_pixel(1, 1, Rgb([4, 5, 6]));
        Reply::Erased {
            pixels,
            result: Ok(Some(Erased {
                rect: gumka_core::Rect::new(1, 1, 1, 1),
                old,
                seconds: 0.1,
            })),
        }
    }

    #[test]
    fn opening_while_the_picture_is_encoded_waits_for_it() {
        let mut h = Harness::new("open-while-encoding");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        h.app.add_click(click(10.0, 10.0));
        assert!(h.app.busy);
        h.drop_file(&b);
        assert_eq!(h.doc().path, a, "not while the pixels are away");
        assert!(h.app.message.is_none(), "and without a complaint");
        h.fake.encode();
        h.frame(vec![]);
        assert_eq!(h.doc().path, b, "opened once they are back");
        assert!(h.app.message.is_none());
    }

    #[test]
    fn saving_while_an_erase_runs_saves_its_result() {
        let mut h = Harness::new("save-while-erasing");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        let pixels = start_erase(&mut h);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        assert_eq!(modified(&a), opened, "not while the pixels are away");
        assert!(h.app.message.is_none());
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert_ne!(modified(&a), opened, "saved once they are back");
        assert!(!h.app.is_dirty(), "with both erases");
    }

    /// An erase may never come back (a stuck model): closing asks at once, and Don't Save
    /// closes without waiting for it.
    #[test]
    fn closing_while_an_erase_runs_asks_at_once() {
        let mut h = Harness::new("close-while-erasing");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.erase();
        let _ = start_erase(&mut h);
        let commands = h.close();
        assert!(commands.contains(&ViewportCommand::CancelClose));
        assert!(matches!(h.app.pending, Some(Pending::Quit)), "asks now");
        h.app.answer(&h.ctx, false);
        assert!(h.app.allow_close);
        assert!(!h.close().contains(&ViewportCommand::CancelClose), "closes");
    }

    /// The first erase of a clean photo comes back in the frame the window is closed.
    #[test]
    fn a_close_in_the_frame_an_erase_comes_back_asks_first() {
        let mut h = Harness::new("close-as-erased");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let pixels = start_erase(&mut h);
        h.fake.reply(erased(pixels));
        let commands = h.close();
        assert!(commands.contains(&ViewportCommand::CancelClose));
        assert!(matches!(h.app.pending, Some(Pending::Quit)));
    }

    #[test]
    fn replies_are_read_while_the_window_is_hidden() {
        let mut h = Harness::new("hidden-reply");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.frame(vec![]);
        let pixels = start_erase(&mut h);
        h.fake.reply(erased(pixels));
        let mut input = RawInput::default();
        input.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                minimized: Some(true),
                ..Default::default()
            },
        );
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut h.app;
        let _ = h.ctx.run_logic(&input, |ctx| app.logic(ctx, &mut frame));
        assert!(!h.app.busy);
        assert!(h.app.is_dirty(), "so a Quit now asks first");
    }

    #[test]
    fn a_lost_photo_drops_its_save_but_still_opens_the_dropped_file() {
        let mut h = Harness::new("stopped-waiting");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        let copy = h.dir.join("copy.png");
        h.open(&a);
        let _ = start_erase(&mut h);
        assert!(!h.app.save_to(copy.clone()));
        h.drop_file(&b);
        assert!(h.app.waiting.is_some());
        h.fake.reply(Reply::Stopped("boom".into()));
        h.frame(vec![]);
        assert!(h.app.doc.is_none());
        assert!(h.app.waiting_save.is_none() && h.app.pending.is_none());
        h.frame(vec![]);
        assert!(h.app.doc.is_none(), "the message is read first");
        h.app.message = None;
        h.frame(vec![]);
        assert!(!copy.exists(), "nothing to save without the pixels");
        assert_eq!(h.doc().path, b, "then the dropped file opens");
        assert!(h.app.pending.is_none(), "no question left over");
    }

    #[test]
    fn save_as_while_an_erase_runs_saves_the_copy_with_it() {
        let mut h = Harness::new("save-as-while-erasing");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        let copy = h.dir.join("copy.png");
        h.open(&a);
        h.erase();
        let pixels = start_erase(&mut h);
        // The path picked in the Save As panel.
        assert!(!h.app.save_to(copy.clone()), "waits for the pixels");
        assert!(!copy.exists());
        h.frame(vec![]);
        assert!(!copy.exists(), "not while the pixels are away");
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert_eq!(h.doc().path, copy, "the document continues as the copy");
        assert!(!h.app.is_dirty());
        // Pixel (1, 1) is the second erase's; it was 10 in the 8-bit original.
        let saved = Picture::open(&copy).unwrap();
        assert_eq!(saved.pixels.get_pixel(1, 1).0, [0, 0, 0], "with the erase");
        let original = Picture::open(&a).unwrap();
        assert_eq!(original.pixels.get_pixel(1, 1).0, [10 * 257; 3]);
    }

    #[test]
    fn a_file_dropped_while_an_erase_runs_asks_about_its_result() {
        let mut h = Harness::new("drop-while-erasing");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        h.erase();
        let pixels = start_erase(&mut h);
        h.drop_file(&b);
        assert_eq!(h.doc().path, a);
        assert!(h.app.pending.is_none(), "no question while erasing");
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert_eq!(h.doc().path, a);
        assert!(
            matches!(&h.app.pending, Some(Pending::Open(path)) if *path == b),
            "Save changes? before opening the dropped file"
        );
        assert_eq!(h.doc().history().last().unwrap().label, "Erase");
    }

    /// A 400 x 300 picture shown at 100%: picture point (x, y) is at screen (318 + x, 290 + y).
    fn press(x: f32, y: f32, pressed: bool) -> Event {
        Event::PointerButton {
            pos: pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn a_drag_started_while_erasing_never_paints_even_after_the_erase_lands() {
        let mut h = Harness::new("drag-while-erasing");
        h.ready();
        let a = h.picture("a.png", 400, 300);
        h.open(&a);
        h.frame(vec![]);
        let pixels = start_erase(&mut h);
        h.frame(vec![Event::PointerMoved(pos2(400.0, 400.0))]);
        h.frame(vec![press(400.0, 400.0, true)]);
        h.frame(vec![Event::PointerMoved(pos2(430.0, 400.0))]);
        h.frame(vec![Event::PointerMoved(pos2(460.0, 400.0))]);
        // The erase lands halfway through the drag.
        h.fake.reply(erased(pixels));
        h.frame(vec![Event::PointerMoved(pos2(500.0, 400.0))]);
        assert!(!h.app.busy);
        h.frame(vec![Event::PointerMoved(pos2(560.0, 400.0))]);
        h.frame(vec![Event::PointerMoved(pos2(600.0, 400.0))]);
        h.frame(vec![press(600.0, 400.0, false)]);
        h.frame(vec![]);
        assert!(!h.doc().has_mask(), "nothing painted");
        assert!(h.doc().mask.pixels().all(|p| p.0[0] == 0));
        assert_eq!(h.doc().history().last().unwrap().label, "Erase");

        // The next drag paints.
        h.frame(vec![Event::PointerMoved(pos2(400.0, 450.0))]);
        h.frame(vec![press(400.0, 450.0, true)]);
        h.frame(vec![Event::PointerMoved(pos2(450.0, 450.0))]);
        h.frame(vec![press(450.0, 450.0, false)]);
        h.frame(vec![]);
        assert_eq!(h.doc().history().last().unwrap().label, "Brush stroke");
        assert!(h.doc().has_mask());
    }

    /// Links are told apart by an underline, not by the accent color.
    #[test]
    fn the_manage_models_link_is_underlined_and_neutral() {
        fn texts(shapes: &[egui::Shape], out: &mut Vec<egui::epaint::TextShape>) {
            for shape in shapes {
                match shape {
                    egui::Shape::Text(text) => out.push(text.clone()),
                    egui::Shape::Vec(shapes) => texts(shapes, out),
                    _ => {}
                }
            }
        }
        let mut h = Harness::new("links");
        h.frame(vec![]);
        let mut all = Vec::new();
        texts(&h.shapes(RawInput::default()), &mut all);
        let link = all
            .iter()
            .find(|t| t.galley.job.text.contains("Manage models"))
            .expect("the start screen shows the link");
        for section in &link.galley.job.sections {
            assert!(section.format.underline.width > 0.0, "underlined");
            assert_ne!(section.format.color, ACCENT);
        }
    }

    /// The Config page starts with Gumka's version, then lists the models as before.
    #[test]
    fn the_config_page_shows_the_version() {
        let mut h = Harness::new("config");
        let version = format!("Gumka {}", crate::config_page::VERSION);
        h.frame(vec![]);
        assert!(!shows(&mut h, RawInput::default(), &version));
        h.app.show_config();
        assert!(h.app.page == Page::Config);
        assert!(shows(&mut h, RawInput::default(), &version));
        assert!(shows(&mut h, RawInput::default(), "Where models run"));
    }

    /// With the Config page open, its rail button is lit in the accent, as a tool's is in the
    /// editor (no tool is selected meanwhile).
    #[test]
    fn the_config_button_is_in_the_accent_while_its_page_is_open() {
        /// The colors painted in the rail; an icon is a rectangle filled with its tint.
        fn rail_colors(shapes: &[egui::Shape], out: &mut Vec<Color32>) {
            for shape in shapes {
                match shape {
                    egui::Shape::Rect(rect) if rect.rect.max.x <= 76.0 => out.push(rect.fill),
                    egui::Shape::Vec(shapes) => rail_colors(shapes, out),
                    _ => {}
                }
            }
        }
        let mut h = Harness::new("config-accent");
        h.app.show_config();
        // The icons are drawn once their SVGs have loaded.
        h.frame(vec![]);
        h.frame(vec![]);
        let mut colors = Vec::new();
        rail_colors(&h.shapes(RawInput::default()), &mut colors);
        assert!(colors.contains(&ACCENT));
    }

    /// Bug 1: a Save As picked while the worker has the pixels is dropped by a later request.
    #[test]
    fn a_save_as_is_kept_when_something_else_waits_too() {
        let mut h = Harness::new("save-as-then-drop");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        let copy = h.dir.join("copy.png");
        h.open(&a);
        h.erase();
        let pixels = start_erase(&mut h);
        assert!(!h.app.save_to(copy.clone()));
        h.drop_file(&b);
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(copy.exists(), "the copy picked in Save As is written");
    }

    /// Bug 2: ⌘S during the photo's first erase is ignored, so its result isn't saved.
    #[test]
    fn save_during_the_first_erase_saves_its_result() {
        let mut h = Harness::new("save-first-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        let pixels = start_erase(&mut h);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert_ne!(modified(&a), opened, "saved once the erase is back");
        assert!(!h.app.is_dirty());
    }

    /// Bug 3: closing (or quitting) during the first erase closes at once and loses it.
    #[test]
    fn closing_during_the_first_erase_waits_for_it_and_asks() {
        let mut h = Harness::new("close-first-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let pixels = start_erase(&mut h);
        let commands = h.close();
        assert!(
            commands.contains(&ViewportCommand::CancelClose),
            "doesn't close yet"
        );
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "asks about the erase"
        );
    }

    #[test]
    fn saving_then_closing_during_the_first_erase_saves_and_closes() {
        let mut h = Harness::new("save-close-first-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        let pixels = start_erase(&mut h);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        // The save asked for comes first: the close waits for it, without asking.
        assert!(h.close().contains(&ViewportCommand::CancelClose));
        assert!(h.app.pending.is_none() && !h.app.allow_close);
        h.fake.reply(erased(pixels));
        let commands = h.frame(vec![]);
        assert_ne!(modified(&a), opened, "saved");
        assert!(h.app.pending.is_none(), "without asking again");
        assert!(h.app.allow_close && commands.contains(&ViewportCommand::Close));
    }

    #[test]
    fn what_waits_lets_an_error_be_read_first() {
        let mut h = Harness::new("wait-for-message");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        let pixels = start_erase(&mut h);
        h.drop_file(&b);
        h.fake.reply(Reply::Erased {
            pixels,
            result: Err("boom".into()),
        });
        h.frame(vec![]);
        assert_eq!(h.doc().path, a, "the error comes first");
        assert!(h.app.message.as_deref().unwrap().contains("boom"));
        h.app.message = None;
        h.frame(vec![]);
        assert_eq!(h.doc().path, b);
    }

    #[test]
    fn a_waiting_open_panel_waits_for_the_window_to_be_shown() {
        let mut h = Harness::new("hidden-open-panel");
        h.app.waiting = Some(Pending::OpenDialog);
        let mut input = RawInput::default();
        input.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                minimized: Some(true),
                ..Default::default()
            },
        );
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut h.app;
        let _ = h.ctx.run_logic(&input, |ctx| app.logic(ctx, &mut frame));
        assert!(matches!(h.app.waiting, Some(Pending::OpenDialog)));
        assert_eq!(panels::SHOWN.with(|n| n.get()), 0, "no panel while hidden");
        h.frame(vec![]);
        assert!(h.app.waiting.is_none());
        assert_eq!(panels::SHOWN.with(|n| n.get()), 1, "shown once on screen");
    }

    #[test]
    fn return_and_undo_wait_for_a_stroke_to_end() {
        let mut h = Harness::new("keys-mid-stroke");
        h.ready();
        let a = h.picture("a.png", 400, 300);
        h.open(&a);
        h.frame(vec![]);
        let doc = h.app.doc.as_mut().unwrap();
        let before = doc.mask.clone();
        doc.mask.put_pixel(1, 1, Luma([255]));
        doc.push_stroke(&before, gumka_core::Rect::new(1, 1, 1, 1), false);
        h.frame(vec![Event::PointerMoved(pos2(400.0, 400.0))]);
        h.frame(vec![press(400.0, 400.0, true)]);
        h.frame(vec![Event::PointerMoved(pos2(430.0, 400.0))]);
        assert!(h.app.canvas.as_ref().unwrap().stroking());
        h.frame(vec![
            key(Key::Enter, Modifiers::NONE),
            key(Key::Z, Modifiers::COMMAND),
            Event::PointerMoved(pos2(440.0, 400.0)),
        ]);
        assert!(!h.app.busy && h.fake.idle(), "no erase with half a stroke");
        assert_eq!(h.doc().history().len(), 2, "nothing undone");
        h.frame(vec![press(440.0, 400.0, false)]);
        h.frame(vec![]);
        assert_eq!(h.doc().history().len(), 3);
        assert_eq!(h.doc().history().last().unwrap().label, "Brush stroke");
        h.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(h.app.busy, "Return erases once the stroke is recorded");
        let _ = h.fake.erase();
    }

    /// Whether `text` is drawn in a frame with `input` (a 1280 x 860 window).
    fn shows(h: &mut Harness, input: RawInput, text: &str) -> bool {
        fn has(shape: &egui::Shape, text: &str) -> bool {
            match shape {
                egui::Shape::Text(t) => t.galley.job.text.contains(text),
                egui::Shape::Vec(shapes) => shapes.iter().any(|s| has(s, text)),
                _ => false,
            }
        }
        h.shapes(input).iter().any(|s| has(s, text))
    }

    /// AppKit can give up on a drag while the app is busy without saying so; the overlay then
    /// goes with the first pointer event.
    #[test]
    fn the_drop_overlay_goes_once_the_pointer_moves() {
        let mut h = Harness::new("stale-hover");
        // As egui-winit reports them: hovering files stay listed until a drop or the drag's
        // exit, and a new drag adds its own.
        let hovering = |files: &[&str], events: Vec<Event>| RawInput {
            hovered_files: files
                .iter()
                .map(|f| egui::HoveredFile {
                    path: Some(PathBuf::from(f)),
                    ..Default::default()
                })
                .collect(),
            events,
            ..Default::default()
        };
        assert!(shows(&mut h, hovering(&["b.png"], vec![]), "Drop to open"));
        assert!(shows(&mut h, hovering(&["b.png"], vec![]), "Drop to open"));
        let moved = vec![Event::PointerMoved(pos2(100.0, 100.0))];
        assert!(
            !shows(&mut h, hovering(&["b.png"], moved), "Drop to open"),
            "the drag is over"
        );
        assert!(
            !shows(&mut h, hovering(&["b.png"], vec![]), "Drop to open"),
            "and stays gone"
        );
        assert!(
            shows(
                &mut h,
                hovering(&["b.png", "c.png"], vec![]),
                "Drop to open"
            ),
            "a new drag shows it"
        );
        h.frame(vec![]);
        assert!(
            shows(&mut h, hovering(&["d.png"], vec![]), "Drop to open"),
            "so does one after a drop"
        );
    }

    /// ⌘+, ⌘=, ⌘−, ⌘0 and ⌘1 leave the view alone while a stroke is painted, and work again
    /// once it ends.
    #[test]
    fn zoom_keys_wait_for_a_stroke_to_end() {
        let mut h = Harness::new("zoom-mid-stroke");
        let a = h.picture("a.png", 400, 300);
        h.open(&a);
        h.frame(vec![]);
        let zoom = |h: &Harness| h.app.canvas.as_ref().unwrap().zoom_percent();
        let start = zoom(&h);
        h.frame(vec![Event::PointerMoved(pos2(400.0, 400.0))]);
        h.frame(vec![press(400.0, 400.0, true)]);
        h.frame(vec![Event::PointerMoved(pos2(440.0, 400.0))]);
        assert!(h.app.canvas.as_ref().unwrap().stroking());
        for (k, m) in [
            (Key::Plus, Modifiers::COMMAND | Modifiers::SHIFT),
            (Key::Equals, Modifiers::COMMAND),
            (Key::Minus, Modifiers::COMMAND),
            (Key::Num0, Modifiers::COMMAND),
            (Key::Num1, Modifiers::COMMAND),
        ] {
            h.frame(vec![key(k, m)]);
            assert_eq!(zoom(&h), start, "{k:?} mid-stroke");
        }
        h.frame(vec![Event::PointerMoved(pos2(480.0, 400.0))]);
        h.frame(vec![press(480.0, 400.0, false)]);
        h.frame(vec![]);
        assert_eq!(h.doc().history().last().unwrap().label, "Brush stroke");
        h.frame(vec![key(Key::Equals, Modifiers::COMMAND)]);
        assert_eq!(zoom(&h), start * 2.0, "works again after the stroke");
    }

    /// Closing again while a save waits asks, so an erase that never comes back can't keep
    /// the window open; Save there still waits.
    #[test]
    fn a_second_close_asks_while_a_save_waits() {
        let mut h = Harness::new("close-twice-save-waiting");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let _ = start_erase(&mut h);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        h.close();
        assert!(h.app.pending.is_none());
        h.close();
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "asks the second time"
        );
        h.app.answer(&h.ctx, false);
        assert!(h.app.allow_close, "Don't Save closes now");
    }

    #[test]
    fn cancelling_the_question_keeps_a_waiting_open() {
        let mut h = Harness::new("cancel-keeps-open");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        h.open(&a);
        h.erase();
        let pixels = start_erase(&mut h);
        h.drop_file(&b);
        h.close();
        assert!(matches!(h.app.pending, Some(Pending::Quit)));
        h.app.pending = None; // Cancel
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert!(
            matches!(&h.app.pending, Some(Pending::Open(path)) if *path == b),
            "the open still comes, asking about the erase"
        );
    }

    /// Opens `path` as a file another app sent.
    fn open_sent(h: &mut Harness, path: &Path) {
        h.app.sent = Some(path.to_path_buf());
        h.open(path);
    }

    #[test]
    fn only_a_photo_another_app_sent_offers_save_and_return() {
        let mut h = Harness::new("sent");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        open_sent(&mut h, &a);
        assert!(h.doc().sent);
        h.drop_file(&b);
        assert_eq!(h.doc().path, b);
        assert!(!h.doc().sent, "a dropped file wasn't sent");
        // A sent file that fails to open doesn't make the next one sent.
        let bad = h.dir.join("bad.png");
        std::fs::write(&bad, b"not a picture").unwrap();
        h.app.sent = Some(bad.clone());
        h.app.open(&h.ctx.clone(), bad);
        h.app.message = None;
        h.open(&a);
        assert!(!h.doc().sent);
    }

    #[test]
    fn save_and_return_saves_then_quits() {
        let mut h = Harness::new("save-return");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        h.erase();
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        h.app.save_and_return(&h.ctx.clone());
        assert_ne!(modified(&a), opened, "saved in place");
        assert!(!h.app.is_dirty() && h.app.allow_close, "and quits");
    }

    #[test]
    fn save_and_return_without_changes_just_quits() {
        let mut h = Harness::new("save-return-clean");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        let opened = modified(&a);
        h.app.save_and_return(&h.ctx.clone());
        assert_eq!(modified(&a), opened, "nothing to save");
        assert!(h.app.allow_close);
    }

    #[test]
    fn save_and_return_during_an_erase_waits_for_it() {
        let mut h = Harness::new("save-return-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        let opened = modified(&a);
        std::thread::sleep(Duration::from_millis(20));
        let pixels = start_erase(&mut h);
        h.app.save_and_return(&h.ctx.clone());
        assert!(!h.app.allow_close, "waits for the erase");
        assert_eq!(modified(&a), opened);
        h.fake.reply(erased(pixels));
        let commands = h.frame(vec![]);
        assert_ne!(modified(&a), opened, "saved with the erase");
        assert!(h.app.allow_close && commands.contains(&ViewportCommand::Close));
    }

    #[test]
    fn saving_as_another_file_ends_save_and_return() {
        let mut h = Harness::new("save-return-save-as");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        h.erase();
        assert!(h.app.save_to(a.clone()));
        assert!(h.doc().sent, "saved in place, still sent");
        h.erase();
        assert!(h.app.save_to(h.dir.join("copy.png")));
        assert!(!h.doc().sent);
    }

    #[test]
    fn save_and_return_leaves_a_waiting_save_as_alone() {
        let mut h = Harness::new("save-return-waiting");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        let opened = modified(&a);
        let pixels = start_erase(&mut h);
        let copy = h.dir.join("copy.png");
        assert!(!h.app.save_to(copy.clone()), "waits for the erase");
        h.app.save_and_return(&h.ctx.clone());
        h.fake.reply(erased(pixels));
        let commands = h.frame(vec![]);
        assert!(
            copy.exists() && modified(&a) == opened,
            "only the copy is saved"
        );
        assert!(!h.doc().sent && !h.app.allow_close);
        assert!(!commands.contains(&ViewportCommand::Close));
    }

    #[cfg(unix)]
    #[test]
    fn save_and_return_stays_when_the_save_fails() {
        use std::os::unix::fs::PermissionsExt;
        let mut h = Harness::new("save-return-fails");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        h.erase();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o444)).unwrap();
        h.app.save_and_return(&h.ctx.clone());
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();
        if h.app.message.is_none() {
            return; // running as root, which may write anyway
        }
        assert!(
            h.app.is_dirty() && !h.app.allow_close,
            "stays, with the error shown"
        );
    }

    #[test]
    fn save_after_a_waiting_save_as_keeps_the_copy() {
        let mut h = Harness::new("save-after-save-as");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        let copy = h.dir.join("copy.png");
        h.open(&a);
        h.erase();
        let opened = modified(&a);
        let pixels = start_erase(&mut h);
        assert!(!h.app.save_to(copy.clone()));
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert!(copy.exists(), "the Save As happens");
        assert_eq!(modified(&a), opened, "the original stays as it was");
        assert_eq!(h.doc().path, copy);
    }

    /// A save held back by a message (here: its erase failed) still comes before what was
    /// asked after it.
    #[test]
    fn a_request_waits_for_a_save_held_back_by_a_message() {
        let mut h = Harness::new("request-after-held-save");
        h.ready();
        let (a, b) = (h.picture("a.png", 40, 30), h.picture("b.png", 24, 36));
        let copy = h.dir.join("copy.png");
        h.open(&a);
        let pixels = start_erase(&mut h);
        assert!(!h.app.save_to(copy.clone()));
        h.fake.reply(Reply::Erased {
            pixels,
            result: Err("boom".into()),
        });
        h.frame(vec![]);
        assert!(h.app.message.is_some() && !copy.exists());
        h.app.request(&h.ctx, Pending::Open(b.clone()));
        assert!(h.app.waiting.is_some(), "waits for the save");
        h.app.message = None;
        h.frame(vec![]);
        assert!(copy.exists(), "the save first");
        assert_eq!(h.doc().path, b, "then the request");
    }

    /// Cancel in the quit question means "don't quit", even when an earlier quit was waiting
    /// for a save.
    #[test]
    fn cancelling_a_second_quit_keeps_the_app_open() {
        let mut h = Harness::new("cancel-second-quit");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let pixels = start_erase(&mut h);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        h.close();
        h.close();
        assert!(
            matches!(h.app.pending, Some(Pending::Quit)),
            "the second quit asks"
        );
        h.app.pending = None; // Cancel
        h.fake.reply(erased(pixels));
        let mut commands = h.frame(vec![]);
        commands.extend(h.frame(vec![]));
        assert!(!h.app.is_dirty(), "the save asked for still happens");
        assert!(!h.app.allow_close, "Cancel said: don't quit");
        assert!(!commands.contains(&ViewportCommand::Close));
    }

    /// A profile without a readable name is "Unnamed" when it is used, "Unreadable" when the
    /// picture is shown as sRGB instead (review-C #7).
    #[test]
    fn the_status_bar_says_how_the_profile_is_used() {
        let label = |has, name: Option<&str>, srgb| profile_label(has, name.map(Into::into), srgb);
        assert_eq!(label(false, None, true), "No color profile (shown as sRGB)");
        assert_eq!(label(true, Some("Display P3"), false), "Display P3");
        assert_eq!(
            label(true, Some("Odd"), true),
            "Odd (unreadable, shown as sRGB)"
        );
        assert_eq!(label(true, None, false), "Unnamed color profile");
        assert_eq!(
            label(true, None, true),
            "Unreadable color profile (shown as sRGB)"
        );
    }
    // ---- Tester's C8 checks: selections, inverts and the click limit through the history ----

    /// One click of a selection, answered with `selected` (what SAM found for all its clicks so
    /// far). Returns the clicks the request carried.
    fn select_with(h: &mut Harness, at: Click, selected: Mask) -> Vec<Click> {
        h.app.add_click(at);
        if h.app.encoding {
            h.fake.encode();
            h.app.poll_worker();
        }
        let (id, clicks, _) = h.fake.select();
        h.fake.reply(Reply::Selected {
            id,
            result: Ok((selected, 0.1)),
        });
        h.app.poll_worker();
        clicks
    }

    /// A 40 x 30 mask set over x0..x1, y0..y1.
    fn rect_mask(x0: u32, y0: u32, x1: u32, y1: u32) -> Mask {
        Mask::from_fn(40, 30, |x, y| {
            Luma([if (x0..x1).contains(&x) && (y0..y1).contains(&y) {
                255
            } else {
                0
            }])
        })
    }

    fn union(a: &Mask, b: &Mask) -> Mask {
        Mask::from_fn(a.width(), a.height(), |x, y| {
            Luma([a.get_pixel(x, y).0[0] | b.get_pixel(x, y).0[0]])
        })
    }

    fn inverted(a: &Mask) -> Mask {
        Mask::from_fn(a.width(), a.height(), |x, y| {
            Luma([255 - a.get_pixel(x, y).0[0]])
        })
    }

    /// Selections, a click that changes nothing, inverts and a selection after an invert: every
    /// step undoes and redoes to exactly the mask it had, and has_mask follows.
    #[test]
    fn tester_selections_and_inverts_undo_and_redo_exactly() {
        let mut h = Harness::new("t-c8-walk");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        let mut states = vec![h.doc().mask.clone()];
        select_with(&mut h, click(5.0, 5.0), rect_mask(0, 0, 10, 10));
        states.push(h.doc().mask.clone());
        select_with(&mut h, click(15.0, 5.0), rect_mask(0, 0, 20, 10));
        states.push(h.doc().mask.clone());
        assert_eq!(h.doc().mask, rect_mask(0, 0, 20, 10));
        select_with(&mut h, click(16.0, 5.0), rect_mask(0, 0, 20, 10)); // no change: no step
        h.app.invert_mask();
        assert!(h.app.selection.is_none(), "an invert ends the selection");
        assert_eq!(h.doc().mask, inverted(&rect_mask(0, 0, 20, 10)));
        states.push(h.doc().mask.clone());
        // A new selection adds to the inverted mask; it sends only its own click.
        let sent = select_with(&mut h, click(5.0, 5.0), rect_mask(2, 2, 6, 6));
        assert_eq!(sent, vec![click(5.0, 5.0)]);
        assert_eq!(
            h.doc().mask,
            union(&inverted(&rect_mask(0, 0, 20, 10)), &rect_mask(2, 2, 6, 6))
        );
        states.push(h.doc().mask.clone());
        h.app.invert_mask();
        states.push(h.doc().mask.clone());
        assert_eq!(
            h.doc().history().len(),
            states.len(),
            "Open plus one row per change"
        );

        for i in (0..states.len() - 1).rev() {
            h.app.undo_redo(true);
            assert!(h.doc().mask == states[i], "undo back to state {i}");
            assert_eq!(
                h.doc().has_mask(),
                states[i].iter().any(|&v| v != 0),
                "has_mask at {i}"
            );
        }
        h.app.undo_redo(true); // nothing left to undo
        assert!(h.doc().mask == states[0]);
        for (i, state) in states.iter().enumerate().skip(1) {
            h.app.undo_redo(false);
            assert!(h.doc().mask == *state, "redo to state {i}");
            assert_eq!(
                h.doc().has_mask(),
                state.iter().any(|&v| v != 0),
                "has_mask at {i}"
            );
        }
    }

    /// The real model's limit of 14 clicks, half of them changing nothing: all 14 count, the
    /// 15th is refused, only the changing ones are steps, and after an undo a new selection
    /// starts from the undone mask.
    #[test]
    fn tester_fourteen_clicks_with_some_that_change_nothing() {
        let mut h = Harness::new("t-c8-limit");
        h.ready();
        h.fake.reply(Reply::MaxClicks(14));
        h.app.poll_worker();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        for i in 0..14u32 {
            // Clicks 0 and 1 give the same mask, then 2 and 3, and so on.
            let sent = select_with(
                &mut h,
                click(i as f32, 1.0),
                rect_mask(0, 0, 2 * (i / 2 + 1), 2),
            );
            assert_eq!(
                sent.len(),
                i as usize + 1,
                "a selection sends all its clicks"
            );
            assert!(h.app.message.is_none(), "click {i}: {:?}", h.app.message);
        }
        assert_eq!(
            h.doc().history().len(),
            1 + 7,
            "seven of the clicks changed the mask"
        );
        let selection = h.app.selection.as_ref().unwrap();
        assert_eq!((selection.clicks.len(), selection.shown), (14, 14));

        h.app.add_click(click(20.0, 20.0));
        assert!(h.fake.idle(), "the 15th click isn't sent");
        assert!(h.app.message.take().unwrap().contains("up to 14 clicks"));
        assert_eq!(h.doc().mask, rect_mask(0, 0, 14, 2));

        h.app.undo_redo(true);
        assert!(h.app.selection.is_none(), "an undo ends the selection");
        assert_eq!(h.doc().mask, rect_mask(0, 0, 12, 2));
        let sent = select_with(&mut h, click(30.0, 20.0), rect_mask(28, 18, 32, 22));
        assert_eq!(sent, vec![click(30.0, 20.0)], "a new selection");
        assert_eq!(
            h.doc().mask,
            union(&rect_mask(0, 0, 12, 2), &rect_mask(28, 18, 32, 22))
        );
        assert_eq!(
            h.doc().history().len(),
            1 + 7,
            "the undone step was replaced"
        );
        h.app.undo_redo(false);
        assert_eq!(
            h.doc().mask,
            union(&rect_mask(0, 0, 12, 2), &rect_mask(28, 18, 32, 22)),
            "nothing to redo"
        );
    }

    /// An erase of an inverted mask undoes to the inverted mask, then to the mask before the
    /// invert; redo erases again.
    #[test]
    fn tester_an_erase_after_an_invert_undoes_in_order() {
        let mut h = Harness::new("t-c8-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.cursor = CursorTool::Select;
        select_with(&mut h, click(5.0, 5.0), rect_mask(0, 0, 10, 10));
        h.app.invert_mask();
        let inverted_mask = h.doc().mask.clone();
        let before = h.doc().picture.pixels.clone();
        h.app.run_tool();
        let (mut pixels, mask) = h.fake.erase();
        assert_eq!(mask, inverted_mask, "the erase gets the inverted mask");
        let rect = gumka_core::Rect::new(20, 10, 5, 5);
        let old = gumka_core::crop(&pixels, rect);
        for y in 10..15 {
            for x in 20..25 {
                pixels.put_pixel(x, y, Rgb([1, 2, 3]));
            }
        }
        let after = pixels.clone();
        h.fake.reply(Reply::Erased {
            pixels,
            result: Ok(Some(Erased {
                rect,
                old,
                seconds: 0.1,
            })),
        });
        h.app.poll_worker();
        assert!(!h.doc().has_mask(), "the erase used up the mask");
        h.app.undo_redo(true);
        assert!(h.doc().picture.pixels == before && h.doc().mask == inverted_mask);
        h.app.undo_redo(true);
        assert_eq!(h.doc().mask, rect_mask(0, 0, 10, 10));
        h.app.undo_redo(false);
        h.app.undo_redo(false);
        assert!(h.doc().picture.pixels == after && !h.doc().has_mask());
    }

    // ---- Upscale ----

    /// Switches to Upscale and starts it at `factor`; returns what the worker was given.
    fn start_upscale(
        h: &mut Harness,
        factor: Factor,
    ) -> (Rgb16, Option<Vec<u16>>, Arc<AtomicBool>) {
        h.app.tool = Tool::Upscale;
        h.app.factor = factor;
        h.app.run_tool();
        assert!(h.app.busy && h.app.upscaling.is_some());
        let asked = h.fake.upscale();
        assert_eq!(asked.factor, factor);
        assert!(!asked.premultiplied, "a PNG's alpha is straight");
        (asked.pixels, asked.alpha, asked.cancel)
    }

    /// The worker's answer: the pixels back, and the result at the factor's size, each pixel
    /// (and alpha value) taken from the nearest one.
    fn upscaled(pixels: Rgb16, alpha: Option<Vec<u16>>, factor: Factor) -> Reply {
        let (w, h) = pixels.dimensions();
        let (rw, rh) = result_size(w, h, factor).unwrap();
        let from = |x: u32, y: u32| (x * w / rw, y * h / rh);
        let big = Rgb16::from_fn(rw, rh, |x, y| {
            let (sx, sy) = from(x, y);
            *pixels.get_pixel(sx, sy)
        });
        let alpha = alpha.map(|a| {
            (0..rw * rh)
                .map(|i| {
                    let (sx, sy) = from(i % rw, i / rw);
                    a[(sy * w + sx) as usize]
                })
                .collect()
        });
        Reply::Upscaled {
            pixels,
            result: Ok(Some(crate::worker::Upscaled {
                pixels: big,
                alpha,
                factor,
                model: upscaler_name(factor.model_scale()),
                seconds: 2.5,
            })),
        }
    }

    #[test]
    fn a_fractional_factor_upscales_to_its_own_size() {
        let mut h = Harness::new("upscale-fraction");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let factor = Factor::from_quarters(10).unwrap();
        let (pixels, alpha, _) = start_upscale(&mut h, factor);
        h.fake.reply(upscaled(pixels, alpha, factor));
        h.frame(vec![]);
        assert_eq!(h.doc().size(), (100, 75));
        let history = h.doc().history();
        let last = history.last().unwrap();
        assert_eq!(
            (last.label.as_str(), last.detail.as_str()),
            ("Upscale 2.5×", "RealPLKSR 4× · 2.5 s")
        );
    }

    #[test]
    fn an_upscale_makes_the_photo_larger_and_undo_brings_it_back() {
        let mut h = Harness::new("upscale");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let (pixels, alpha, _) = start_upscale(&mut h, Factor::X2);
        assert!(alpha.is_none());
        h.fake.reply(Reply::Progress { done: 1, total: 4 });
        h.frame(vec![]);
        let progress = h.app.upscaling.as_ref().map(|r| (r.done, r.total));
        assert_eq!(progress, Some((1, 4)));
        h.fake.reply(upscaled(pixels, None, Factor::X2));
        h.frame(vec![]);
        assert!(!h.app.busy && h.app.upscaling.is_none());
        assert_eq!(h.doc().size(), (80, 60));
        assert!(h.app.is_dirty());
        let history = h.doc().history();
        let last = history.last().unwrap();
        assert_eq!(
            (last.label.as_str(), last.detail.as_str()),
            ("Upscale 2×", "RealPLKSR 2× · 2.5 s")
        );
        assert_eq!(h.app.last_run, Some(("upscale", 2.5)));

        h.frame(vec![key(Key::Z, Modifiers::COMMAND)]);
        assert_eq!(h.doc().size(), (40, 30));
        h.frame(vec![key(Key::Z, Modifiers::COMMAND.plus(Modifiers::SHIFT))]);
        assert_eq!(h.doc().size(), (80, 60));
        assert!(h.app.save());
        assert_eq!(Picture::open(&a).unwrap().pixels.dimensions(), (80, 60));
    }

    /// Where the tool's main action is, once the inspector's panels have settled (they size
    /// themselves from the frames before).
    fn action_place(h: &mut Harness) -> Option<egui::Rect> {
        for _ in 0..3 {
            h.frame(vec![]);
        }
        h.ctx.read_response(widgets::action_id()).map(|r| r.rect)
    }

    /// While an upscale runs, its button keeps its place and size, and a click on it stops the
    /// upscale: no Cancel button appears and pushes the inspector around.
    #[test]
    fn the_upscale_button_stops_the_upscale_in_place() {
        let mut h = Harness::new("upscale-stop");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.tool = Tool::Upscale;
        let idle = action_place(&mut h).expect("the action is shown");
        let (pixels, _, cancel) = start_upscale(&mut h, Factor::X2);
        h.fake.reply(Reply::Progress { done: 1, total: 4 });
        assert_eq!(
            action_place(&mut h),
            Some(idle),
            "the same place while it runs"
        );
        let at = idle.center();
        h.frame(vec![Event::PointerMoved(at)]);
        let clickable = |h: &Harness| {
            let action = h.ctx.read_response(widgets::action_id());
            action.is_some_and(|r| r.sense.senses_click())
        };
        assert!(clickable(&h), "while it runs, the button stops it");
        h.frame(vec![press(at.x, at.y, true)]);
        h.frame(vec![press(at.x, at.y, false)]);
        assert!(
            cancel.load(Ordering::Relaxed),
            "a click on it stops the upscale"
        );
        // The worker finishes the tile it is on first: until then the button says it is
        // stopping, in the same place, and takes no more clicks.
        assert_eq!(action_place(&mut h), Some(idle));
        assert!(!clickable(&h), "stopping");
        h.fake.reply(Reply::Upscaled {
            pixels,
            result: Ok(None),
        });
        assert_eq!(action_place(&mut h), Some(idle), "and after it");
        assert!(h.app.upscaling.is_none());
        assert_eq!(h.doc().size(), (40, 30));
        assert!(!h.doc().can_undo(), "the photo is as it was");
    }

    #[test]
    fn a_cancelled_upscale_leaves_the_photo_as_it_was() {
        let mut h = Harness::new("upscale-cancel");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let (pixels, _, cancel) = start_upscale(&mut h, Factor::X4);
        h.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(cancel.load(Ordering::Relaxed), "Escape cancels");
        h.fake.reply(Reply::Upscaled {
            pixels,
            result: Ok(None),
        });
        h.frame(vec![]);
        assert!(!h.app.busy && h.app.upscaling.is_none() && h.app.message.is_none());
        assert_eq!(h.doc().size(), (40, 30));
        assert!(!h.app.is_dirty() && !h.doc().can_undo());
    }

    #[test]
    fn escape_and_the_mask_tools_keys_leave_upscale_alone() {
        let mut h = Harness::new("upscale-escape");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let doc = h.app.doc.as_mut().unwrap();
        let before = doc.mask.clone();
        doc.mask.put_pixel(3, 3, Luma([255]));
        doc.push_stroke(&before, gumka_core::Rect::new(3, 3, 1, 1), false);
        h.app.tool = Tool::Upscale;
        h.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(
            h.doc().has_mask(),
            "Upscale doesn't show the mask, so Escape leaves it"
        );
        // The mask tools' keys don't switch tools or change the brush there.
        h.app.cursor = CursorTool::Remove;
        let brush = h.app.canvas.as_ref().unwrap().brush_radius;
        for k in [Key::B, Key::E, Key::S, Key::OpenBracket, Key::CloseBracket] {
            h.frame(vec![key(k, Modifiers::NONE)]);
            assert_eq!(
                (h.app.tool, h.app.cursor),
                (Tool::Upscale, CursorTool::Remove),
                "{k:?}"
            );
        }
        assert_eq!(h.app.canvas.as_ref().unwrap().brush_radius, brush);
        // In Erase they work. H picks nothing: Space and a drag pan in every tool.
        h.frame(vec![key(Key::Num1, Modifiers::NONE)]);
        h.frame(vec![key(Key::B, Modifiers::NONE)]);
        assert_eq!((h.app.tool, h.app.cursor), (Tool::Erase, CursorTool::Brush));
        h.frame(vec![key(Key::H, Modifiers::NONE)]);
        assert_eq!(h.app.cursor, CursorTool::Brush);
    }

    #[test]
    fn saving_during_an_upscale_waits_and_saves_the_larger_photo() {
        let mut h = Harness::new("upscale-save");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let (pixels, _, _) = start_upscale(&mut h, Factor::X2);
        h.frame(vec![key(Key::S, Modifiers::COMMAND)]);
        assert_eq!(h.app.waiting_save.as_deref(), Some(a.as_path()));
        assert_eq!(Picture::open(&a).unwrap().pixels.dimensions(), (40, 30));
        h.fake.reply(upscaled(pixels, None, Factor::X2));
        h.frame(vec![]);
        assert_eq!(Picture::open(&a).unwrap().pixels.dimensions(), (80, 60));
        assert!(!h.app.is_dirty());
    }

    #[test]
    fn save_and_return_after_an_upscale_hands_back_the_larger_photo() {
        let mut h = Harness::new("upscale-return");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        open_sent(&mut h, &a);
        let (pixels, _, _) = start_upscale(&mut h, Factor::X2);
        h.fake.reply(upscaled(pixels, None, Factor::X2));
        h.frame(vec![]);
        h.app.save_and_return(&h.ctx.clone());
        assert!(h.app.allow_close);
        assert_eq!(Picture::open(&a).unwrap().pixels.dimensions(), (80, 60));
    }

    #[test]
    fn a_thread_that_stops_during_an_upscale_closes_the_photo() {
        let mut h = Harness::new("upscale-stopped");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        start_upscale(&mut h, Factor::X2);
        h.fake.reply(Reply::Stopped("gone".into()));
        h.frame(vec![]);
        assert!(h.app.doc.is_none() && h.app.upscaling.is_none() && !h.app.busy);
        assert!(
            h.app
                .message
                .as_deref()
                .is_some_and(|m| m.contains("lost the photo"))
        );
        assert!(
            h.app
                .upscalers
                .iter()
                .all(|s| matches!(s, Status::Unavailable(_)))
        );
    }

    /// An upscaler that is unavailable refuses the scales it makes, as shown: 4× chosen for a
    /// 20,000 px panorama shows 3.25×, which the 4× model makes.
    #[test]
    fn an_upscale_that_cant_run_says_why_and_starts_nothing() {
        let mut h = Harness::new("upscale-refused");
        h.ready();
        let wide = h.picture("wide.png", 20000, 2);
        h.open(&wide);
        h.app.tool = Tool::Upscale;
        h.fake.reply(Reply::Status(
            Which::Upscaler(4),
            Status::Unavailable("its files are missing".into()),
        ));
        h.app.poll_worker();
        h.app.factor = Factor::X4;
        h.app.run_tool();
        assert!(!h.app.busy && h.fake.idle());
        let message = h.app.message.take().unwrap();
        assert!(
            message.contains("Upscale is unavailable") && message.contains("missing"),
            "{message}"
        );

        h.app.factor = Factor::X2;
        h.app.run_tool();
        assert_eq!(h.fake.upscale().factor, Factor::X2, "the 2× model is there");
    }

    /// The scale comes down to the largest that fits the photo, so that the slider, its label
    /// and the action agree, and the action runs at it.
    #[test]
    fn the_scale_comes_down_to_what_fits_the_photo() {
        let mut h = Harness::new("upscale-fit");
        h.ready();
        h.app.factor = Factor::X4;
        // 20,000 px wide: 3.25× makes 65,000 px, 3.5× would be past 65,535.
        let wide = h.picture("wide.png", 20000, 2);
        h.open(&wide);
        h.app.tool = Tool::Upscale;
        h.frame(vec![]);
        assert_eq!(h.app.effective_factor().to_string(), "3.25×");
        assert_eq!(h.app.factor, Factor::X4, "the choice stays");
        assert_eq!(
            result_size(20000, 2, h.app.effective_factor()),
            Ok((65000, 7))
        );
        assert_eq!(h.app.upscale_refused(), None, "the action is enabled");
        h.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        let asked = h.fake.upscale();
        assert_eq!(asked.factor, Factor::from_quarters(13).unwrap());
        assert_eq!(
            h.app.upscaling.as_ref().map(|r| r.factor),
            Some(asked.factor)
        );
    }

    /// The scale chosen stays the choice: a photo it doesn't fit shows the largest that fits,
    /// and the next photo, which it fits, gets it again (review round 2: it stuck at 3.25×).
    #[test]
    fn the_chosen_scale_comes_back_for_a_photo_it_fits() {
        let mut h = Harness::new("upscale-fit-next");
        h.ready();
        h.app.factor = Factor::X4;
        h.app.tool = Tool::Upscale;
        let wide = h.picture("wide.png", 20000, 2);
        h.open(&wide);
        h.frame(vec![]);
        assert_eq!(h.app.effective_factor().to_string(), "3.25×");
        // Erase on, as Upscale off, changes nothing either.
        h.app.tool = Tool::Erase;
        h.frame(vec![]);
        let photo = h.picture("photo.png", 60, 40);
        h.open(&photo);
        h.app.tool = Tool::Upscale;
        h.frame(vec![]);
        assert_eq!(
            (h.app.factor, h.app.effective_factor()),
            (Factor::X4, Factor::X4)
        );
    }

    /// The memory a large result can take, counted as the inspector does.
    #[test]
    fn the_memory_hint_counts_the_pyramids_and_the_undo_copy() {
        // 16 MP to 64 MP: 64 x (7 + 2 x 5.33) + 16 x (7 + 5.33) million bytes.
        let bytes = memory_needed(16_000_000, 64_000_000, false);
        assert!((bytes - 1_328e6).abs() < 1e6, "{bytes}");
        // An alpha channel is 2 bytes a pixel more, before and after.
        let with_alpha = memory_needed(16_000_000, 64_000_000, true);
        assert!((with_alpha - bytes - 160e6).abs() < 1e6, "{with_alpha}");
    }

    /// When not even the smallest scale fits, Upscale says so and doesn't run.
    #[test]
    fn a_photo_too_large_for_any_scale_says_so() {
        let mut h = Harness::new("upscale-none-fits");
        h.ready();
        // 1.25× of 60,000 px is past 65,535.
        let huge = h.picture("huge.png", 60000, 2);
        h.open(&huge);
        h.app.tool = Tool::Upscale;
        h.frame(vec![]);
        assert_eq!(
            h.app.upscale_refused().as_deref(),
            Some(AS_LARGE_AS_IT_GETS)
        );
        h.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(!h.app.busy && h.fake.idle());
        assert_eq!(h.app.message.as_deref(), Some(AS_LARGE_AS_IT_GETS));
    }

    /// A scale that fits stays as chosen: only one too large for the photo comes down, and
    /// nothing raises it again.
    #[test]
    fn a_scale_that_fits_is_left_as_chosen() {
        let mut h = Harness::new("upscale-fit-kept");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        h.app.tool = Tool::Upscale;
        for quarters in [5, 10, 13, 16] {
            let factor = Factor::from_quarters(quarters).unwrap();
            h.app.factor = factor;
            h.frame(vec![]);
            h.frame(vec![]);
            assert_eq!(h.app.factor, factor);
        }
    }

    /// An upscale can leave the scale too large for the larger photo; what Upscale makes comes
    /// down then, so that the next upscale isn't refused, and goes back to the chosen scale
    /// after an undo (review round 2: it stayed down).
    #[test]
    fn after_an_upscale_the_scale_comes_down_to_what_still_fits() {
        let mut h = Harness::new("upscale-fit-after");
        h.ready();
        let wide = h.picture("wide.png", 20000, 2);
        h.open(&wide);
        let (pixels, alpha, _) = start_upscale(&mut h, Factor::X2);
        h.frame(vec![]);
        assert_eq!(h.app.factor, Factor::X2, "while it runs");
        h.fake.reply(upscaled(pixels, alpha, Factor::X2));
        h.frame(vec![]);
        assert_eq!(h.doc().size(), (40000, 4));
        // 1.5× of 40,000 px is 60,000; 1.75× would be 70,000.
        assert_eq!(h.app.effective_factor().to_string(), "1.5×");
        assert_eq!(h.app.factor, Factor::X2, "the choice stays");
        assert_eq!(h.app.upscale_refused(), None);
        h.frame(vec![key(Key::Z, Modifiers::COMMAND)]);
        h.frame(vec![]);
        assert_eq!(h.doc().size(), (20000, 2));
        assert_eq!(h.app.effective_factor(), Factor::X2);
    }

    /// A TIFF's associated alpha goes to the upscale as premultiplied: 2 x 1, 8-bit RGBA,
    /// ExtraSamples 1.
    #[test]
    fn an_associated_alpha_is_upscaled_as_premultiplied() {
        let mut h = Harness::new("upscale-associated");
        h.ready();
        let entry = |tag: u16, ty: u16, count: u32, value: u32| {
            [
                &tag.to_le_bytes()[..],
                &ty.to_le_bytes(),
                &count.to_le_bytes(),
                &value.to_le_bytes(),
            ]
            .concat()
        };
        // The directory at 8: 10 entries, so the bit depths start at 8 + 2 + 120 + 4 = 134 and
        // the pixels at 142.
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend(10u16.to_le_bytes());
        for (tag, ty, count, value) in [
            (256, 3, 1, 2),
            (257, 3, 1, 1),
            (258, 3, 4, 134),
            (259, 3, 1, 1),
            (262, 3, 1, 2),
            (273, 4, 1, 142),
            (277, 3, 1, 4),
            (278, 3, 1, 1),
            (279, 4, 1, 8),
            (338, 3, 1, 1),
        ] {
            tiff.extend(entry(tag, ty, count, value));
        }
        tiff.extend(0u32.to_le_bytes());
        tiff.extend([8, 0, 8, 0, 8, 0, 8, 0]);
        tiff.extend([10, 20, 30, 128, 1, 2, 3, 255]);
        let a = h.dir.join("associated.tif");
        std::fs::write(&a, tiff).unwrap();
        h.open(&a);
        h.app.tool = Tool::Upscale;
        h.app.run_tool();
        let asked = h.fake.upscale();
        assert_eq!(asked.alpha, Some(vec![128 * 257, 65535]));
        assert!(asked.premultiplied);
    }

    #[test]
    fn an_erase_after_an_upscale_works_at_the_new_size() {
        let mut h = Harness::new("upscale-erase");
        h.ready();
        let a = h.picture("a.png", 40, 30);
        h.open(&a);
        let (pixels, _, _) = start_upscale(&mut h, Factor::X2);
        h.fake.reply(upscaled(pixels, None, Factor::X2));
        h.frame(vec![]);
        let big = h.doc().picture.pixels.clone();

        h.app.tool = Tool::Erase;
        let doc = h.app.doc.as_mut().unwrap();
        let before = doc.mask.clone();
        doc.mask.put_pixel(70, 50, Luma([255]));
        doc.push_stroke(&before, gumka_core::Rect::new(70, 50, 1, 1), false);
        h.app.run_tool();
        let (pixels, mask) = h.fake.erase();
        assert_eq!(
            (pixels.dimensions(), mask.dimensions()),
            ((80, 60), (80, 60))
        );
        h.fake.reply(erased(pixels));
        h.frame(vec![]);
        assert_ne!(h.doc().picture.pixels, big);
        h.app.undo_redo(true); // the erase
        h.app.undo_redo(true); // the stroke
        assert_eq!(h.doc().picture.pixels, big);
        h.app.undo_redo(true); // the upscale
        assert_eq!(h.doc().size(), (40, 30));
    }

    #[test]
    fn an_alpha_channel_is_upscaled_and_saved_with_the_photo() {
        let mut h = Harness::new("upscale-alpha");
        h.ready();
        let a = h.dir.join("rgba.png");
        let rgba = gumka_core::image::ImageBuffer::<gumka_core::image::Rgba<u16>, _>::from_fn(
            10,
            6,
            |x, y| {
                gumka_core::image::Rgba([
                    x as u16 * 5000,
                    y as u16 * 9000,
                    700,
                    1000 + x as u16 * 6000,
                ])
            },
        );
        rgba.save(&a).unwrap();
        h.open(&a);
        let (pixels, alpha, _) = start_upscale(&mut h, Factor::X2);
        assert_eq!(alpha.as_ref().map(Vec::len), Some(60));
        h.fake.reply(upscaled(pixels, alpha, Factor::X2));
        h.frame(vec![]);
        assert!(h.app.save());
        let back = Picture::open(&a).unwrap();
        assert_eq!(back.pixels.dimensions(), (20, 12));
        assert_eq!(back.alpha().map(<[u16]>::len), Some(240));
        assert_eq!(back.alpha().unwrap()[0], 1000);
    }
}
