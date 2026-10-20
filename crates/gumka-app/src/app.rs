// SPDX-License-Identifier: GPL-3.0-or-later

//! The app window, laid out as in the design: toolbar, tool rail, viewer, inspector and
//! status bar, plus the start screen and dialogs.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Key, KeyboardShortcut, Layout,
    Margin, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2, vec2,
};
use gumka_core::erase::EraseSettings;
use gumka_core::picture::FileType;

use crate::canvas::{Canvas, CursorTool, ShowOptions};
use crate::display::ColorSpaceTag;
use crate::document::{Changed, Document};
use crate::icons::{self, Icon};
use crate::theme::*;
use crate::widgets;
use crate::worker::{Reply, Status, Which, Worker};
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
const PAN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::H);
const SELECT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::S);
const ERASE_TOOL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Num1);

/// Height of the History list before it scrolls: about five rows.
const HISTORY_LIST_HEIGHT: f32 = 135.0;

/// A tool in the rail. Only Erase exists so far; Fill, Extend and Upscale slot in here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Erase,
}

struct ToolInfo {
    name: &'static str,
    input: &'static str,
    blurb: &'static str,
    action: &'static str,
    footnote: &'static str,
    icon: Icon,
    shortcut: KeyboardShortcut,
}

impl Tool {
    const ALL: [Tool; 1] = [Tool::Erase];

    fn info(self) -> ToolInfo {
        match self {
            Tool::Erase => ToolInfo {
                name: "Erase",
                input: "Mask",
                blurb: "Remove objects and rebuild the area from its surroundings.",
                action: "Erase",
                footnote: "Pixels beyond the mask’s soft edge stay untouched.",
                icon: icons::ERASE,
                shortcut: ERASE_TOOL,
            },
        }
    }
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
    Models,
}

/// Which inspector sections are open.
struct Sections {
    selection: bool,
    model: bool,
    edges: bool,
    advanced: bool,
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
    /// The pixels are with the worker: for an erase, or for encoding (`encoding`).
    busy: bool,
    last_erase: Option<f32>,
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
    /// Filled when the Models page opens.
    model_rows: Vec<crate::models_page::ModelRow>,
    /// The file from the command line, opened on the first frame: only then does egui know
    /// how large a texture the GPU takes, which the canvas needs.
    startup: Option<PathBuf>,
    /// A file another app sent, until it is opened: that document offers Save & Return.
    sent: Option<PathBuf>,
    /// How many of the files hovering over the window were given up on: AppKit can end a drag
    /// without telling a window that was busy, and then the hover never ends.
    stale_hover: usize,
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
            last_erase: None,
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
            },
            startup: path,
            sent: None,
            stale_hover: 0,
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

    fn show_models(&mut self) {
        self.model_rows = crate::models_page::load_rows();
        self.page = Page::Models;
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
                self.last_erase = None;
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
        if self.busy {
            return;
        }
        let Some(doc) = &mut self.doc else { return };
        match &self.model {
            Status::Ready { .. } => {}
            Status::Loading => {
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
            pixels: None,
            mask: (!rect.is_empty()).then_some(rect),
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
                pixels: None,
                mask: rect,
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
                pixels: None,
                mask: Some(rect),
            });
        }
    }

    fn redraw(&mut self, changed: Changed) {
        let (Some(doc), Some(canvas)) = (&self.doc, &mut self.canvas) else {
            return;
        };
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
                            self.last_erase = Some(erased.seconds);
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
                        self.last_erase = None;
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
        if self.page == Page::Models {
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
        if pressed(&CLEAR_MASK) && !stroking {
            self.clear_mask();
        }
        if pressed(&ERASE_TOOL) {
            self.tool = Tool::Erase;
        }
        if pressed(&BRUSH) {
            self.cursor = CursorTool::Brush;
        }
        if pressed(&REMOVE) {
            self.cursor = CursorTool::Remove;
        }
        if pressed(&SELECT) {
            if self.sam.is_ready() {
                self.cursor = CursorTool::Select;
            } else if let Status::Unavailable(reason) = &self.sam {
                self.message = Some(format!("Select object is unavailable:\n{reason}"));
            }
        }
        if pressed(&PAN) {
            self.cursor = CursorTool::Pan;
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
            if pressed(&SMALLER_BRUSH) {
                canvas.brush_radius = (canvas.brush_radius / 1.25).max(1.0);
            }
            if pressed(&BIGGER_BRUSH) {
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
                            let selected = has_doc && self.cursor == tool;
                            if widgets::icon_button(
                                ui,
                                icon,
                                tooltip,
                                selected,
                                has_doc,
                                vec2(34.0, 30.0),
                            )
                            .clicked()
                            {
                                self.cursor = tool;
                            }
                        }
                        let tooltip = match &self.sam {
                            Status::Ready { .. } => "Select object (S)".to_owned(),
                            Status::Loading => "Select object (S) · loading SAM…".to_owned(),
                            Status::Unavailable(reason) => {
                                format!("Select object (S) · unavailable: {reason}")
                            }
                        };
                        let selected = has_doc && self.cursor == CursorTool::Select;
                        if widgets::icon_button(
                            ui,
                            icons::SELECT,
                            &tooltip,
                            selected,
                            has_doc && self.sam.is_ready(),
                            vec2(34.0, 30.0),
                        )
                        .clicked()
                        {
                            self.cursor = CursorTool::Select;
                        }
                        let selected = has_doc && self.cursor == CursorTool::Pan;
                        if widgets::icon_button(
                            ui,
                            icons::PAN,
                            "Pan (H)",
                            selected,
                            has_doc,
                            vec2(34.0, 30.0),
                        )
                        .clicked()
                        {
                            self.cursor = CursorTool::Pan;
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
                if widgets::rail_button(ui, info.icon, info.name, &tooltip, selected, ACCENT)
                    .clicked()
                {
                    self.tool = tool;
                    self.page = Page::Editor;
                }
            }
        });
        // Models sits at the bottom of the rail.
        let rail = ui.max_rect();
        let spot = Rect::from_center_size(
            pos2(rail.center().x, rail.bottom() - 29.0),
            vec2(64.0, 58.0),
        );
        let selected = self.page == Page::Models;
        let clicked = ui
            .scope_builder(egui::UiBuilder::new().max_rect(spot), |ui| {
                widgets::rail_button(ui, icons::MODELS, "Models", "Models", selected, TEXT_STRONG)
                    .clicked()
            })
            .inner;
        if clicked {
            if selected {
                self.page = Page::Editor;
            } else {
                self.show_models();
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
                        widgets::chip(ui, &format!("Input · {}", info.input));
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
                let has_mask = self.doc.as_ref().is_some_and(Document::has_mask);
                let enabled = has_mask && self.model_ready() && !self.busy;
                let erasing = self.busy && !self.encoding;
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
                        self.erase_sections(ui);
                    });
            });
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
            Status::Loading => "Loading…".into(),
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
                    widgets::info_row(ui, "Runs on", &format!("This Mac · {runs_on}"));
                    widgets::hint(ui, "Included with Gumka.");
                }
                Status::Loading => {
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
                if let Some(seconds) = self.last_erase {
                    ui.label(small(format!("Last erase {seconds:.1} s")));
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match &self.model {
                        Status::Ready { model, runs_on } => {
                            ui.label(small(format!("{model} ready · {runs_on}")));
                            widgets::dot(ui, OK);
                        }
                        Status::Loading => {
                            ui.label(small("Loading model…".into()));
                            ui.spinner();
                        }
                        Status::Unavailable(reason) => {
                            ui.label(RichText::new("Erase unavailable").size(11.5).color(WARN))
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
            self.show_models();
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

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(message) = self.message.clone() {
            egui::Modal::new(egui::Id::unique("message")).show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(message);
                ui.add_space(8.0);
                if ui.button("OK").clicked()
                    || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape))
                {
                    self.message = None;
                }
            });
            return;
        }
        if self.pending.is_none() {
            return;
        }
        let name = self.doc.as_ref().map(Document::name).unwrap_or_default();
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("unsaved")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(format!("Save changes to {name}?"));
            ui.label("Your erases will be lost if you don't save them.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    answer = Some(true);
                }
                if ui.button("Don't Save").clicked() {
                    answer = Some(false);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.pending = None;
                }
            });
        });
        if let Some(save) = answer {
            self.answer(ctx, save);
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
        if self.page == Page::Models {
            egui::CentralPanel::default()
                .frame(Frame::NONE.fill(BG_PANEL))
                .show(ui, |ui| {
                    crate::models_page::show(ui, &self.model_rows, &self.model, &self.sam)
                });
        } else {
            egui::CentralPanel::default()
                .frame(Frame::NONE.fill(BG_VIEWER))
                .show(ui, |ui| {
                    let no_clicks: &[Click] = &[];
                    let options = ShowOptions {
                        tool: self.cursor,
                        can_paint: !self.busy && !self.dialog_open(),
                        show_mask: self.show_mask,
                        compare: self.compare || ui.input(|i| i.key_down(Key::Backslash)),
                        clicks: self.selection.as_ref().map_or(no_clicks, |s| &s.clicks),
                    };
                    let mut click = None;
                    match (&mut self.doc, &mut self.canvas) {
                        (Some(doc), Some(canvas)) => {
                            let events = canvas.show(ui, &mut doc.mask, &options);
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
        self.dialogs(&ctx);
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
        (Status::Loading, _) => widgets::hint(ui, "Select object is loading…"),
        (Status::Unavailable(_), _) => {}
    }
}

/// A model's name and state, for the start screen.
fn model_status_row(ui: &mut Ui, name: &str, status: &Status) {
    ui.horizontal(|ui| {
        match status {
            Status::Ready { .. } => widgets::dot(ui, OK),
            Status::Loading => {
                ui.spinner();
            }
            Status::Unavailable(_) => widgets::dot(ui, WARN),
        }
        ui.label(RichText::new(name).size(12.5).color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let state = match status {
                Status::Ready { .. } => "Ready on this Mac",
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

/// The native open and save panels. Tests never show one: they count them instead.
mod panels {
    use std::path::PathBuf;

    #[cfg(not(test))]
    pub fn pick_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
        dialog.pick_file()
    }

    #[cfg(not(test))]
    pub fn save_file(dialog: rfd::FileDialog) -> Option<PathBuf> {
        dialog.save_file()
    }

    #[cfg(test)]
    thread_local! {
        /// Panels shown on this thread (each test runs on its own).
        pub static SHOWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
            last_erase: None,
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
            },
            startup: None,
            sent: None,
            stale_hover: 0,
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
            self.app.poll_worker();
        }

        /// Runs one frame, as eframe does: `logic`, then `ui`. Returns the window commands.
        fn frame_with(&mut self, mut input: RawInput) -> Vec<ViewportCommand> {
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
            let commands = output
                .viewport_output
                .remove(&ViewportId::ROOT)
                .map(|v| v.commands)
                .unwrap_or_default();
            output.drop_without_applying_deltas();
            commands
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
        assert_eq!(crate::canvas::tests::finest_factor(canvas), 1);
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
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut h.app;
        let output = h.ctx.run_ui(RawInput::default(), |ui| {
            app.logic(ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
        let shapes: Vec<egui::Shape> = output.shapes.iter().map(|s| s.shape.clone()).collect();
        output.drop_without_applying_deltas();
        let mut all = Vec::new();
        texts(&shapes, &mut all);
        let link = all
            .iter()
            .find(|t| t.galley.job.text.contains("Manage models"))
            .expect("the start screen shows the link");
        for section in &link.galley.job.sections {
            assert!(section.format.underline.width > 0.0, "underlined");
            assert_ne!(section.format.color, ACCENT);
        }
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
    fn shows(h: &mut Harness, mut input: RawInput, text: &str) -> bool {
        fn has(shape: &egui::Shape, text: &str) -> bool {
            match shape {
                egui::Shape::Text(t) => t.galley.job.text.contains(text),
                egui::Shape::Vec(shapes) => shapes.iter().any(|s| has(s, text)),
                _ => false,
            }
        }
        h.time += 1.0 / 60.0;
        input.time = Some(h.time);
        input.screen_rect = Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 860.0)));
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut h.app;
        let output = h.ctx.run_ui(input, |ui| {
            app.logic(ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
        let found = output.shapes.iter().any(|s| has(&s.shape, text));
        output.drop_without_applying_deltas();
        found
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
}
