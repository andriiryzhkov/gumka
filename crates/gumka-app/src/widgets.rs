// SPDX-License-Identifier: GPL-3.0-or-later

//! The design's controls: icon and rail buttons, collapsible inspector sections, labelled
//! sliders, segmented controls, switches, chips and the primary action button.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, Response,
    RichText, Role, Sense, Stroke, Ui, Vec2, WidgetInfo, pos2, vec2,
};

use crate::icons::{self, Icon};
use crate::theme::*;

/// A toolbar icon button.
pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    tooltip: &str,
    selected: bool,
    enabled: bool,
    size: Vec2,
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| WidgetInfo::selected(Role::Button, enabled, selected, tooltip));
    let bg = if selected {
        BG_SELECTED
    } else if enabled && response.hovered() {
        Color32::from_gray(0x33)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6.0, bg);
    let color = match (enabled, selected) {
        (false, _) => TEXT_DISABLED,
        (true, true) => ACCENT,
        (true, false) => ICON,
    };
    icon.paint(
        ui,
        Rect::from_center_size(rect.center(), vec2(18.0, 18.0)),
        color,
    );
    response.on_hover_text(tooltip)
}

/// A button in the left rail: icon over a short label. `tint` colors the icon while it is
/// selected: the accent for the active tool, a neutral for a page such as Models.
pub fn rail_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    tooltip: &str,
    selected: bool,
    tint: Color32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(64.0, 58.0), Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, label));
    let bg = if selected {
        BG_SELECTED
    } else if response.hovered() {
        Color32::from_gray(0x2c)
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, bg);
    let icon_rect =
        Rect::from_center_size(pos2(rect.center().x, rect.top() + 21.0), vec2(22.0, 22.0));
    icon.paint(
        ui,
        icon_rect,
        if selected {
            tint
        } else {
            Color32::from_gray(0xc8)
        },
    );
    painter.text(
        pos2(rect.center().x, rect.bottom() - 13.0),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        if selected { TEXT_STRONG } else { TEXT_WEAK },
    );
    response.on_hover_text(tooltip)
}

/// A collapsible inspector section, as in pro photo editors: a header strip with a chevron,
/// a summary while closed, and a reset button. Returns true when reset was clicked.
pub fn section(
    ui: &mut Ui,
    title: &str,
    open: &mut bool,
    summary: Option<String>,
    can_reset: bool,
    body: impl FnOnce(&mut Ui),
) -> bool {
    let width = ui.available_width();
    let (rect, header) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    header.widget_info(|| WidgetInfo::selected(Role::DisclosureTriangle, true, *open, title));
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, BG_SECTION);
    let chevron = if *open {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    chevron.paint(
        ui,
        Rect::from_center_size(pos2(rect.left() + 15.0, rect.center().y), vec2(14.0, 14.0)),
        TEXT_LABEL,
    );
    painter.text(
        pos2(rect.left() + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        FontId::new(12.5, medium()),
        Color32::from_gray(0xe4),
    );

    let mut reset = false;
    let mut right = rect.right() - 8.0;
    if can_reset {
        let button = Rect::from_center_size(pos2(right - 13.0, rect.center().y), vec2(26.0, 26.0));
        let response = ui.interact(button, ui.scope_id().with((title, "reset")), Sense::click());
        if response.hovered() {
            ui.painter()
                .rect_filled(button, 5.0, Color32::from_gray(0x3a));
        }
        icons::RESET.paint(
            ui,
            Rect::from_center_size(button.center(), vec2(14.0, 14.0)),
            TEXT_WEAK,
        );
        reset = response.on_hover_text(format!("Reset {title}")).clicked();
        right = button.left() - 4.0;
    }
    if !*open && let Some(summary) = summary {
        ui.painter().text(
            pos2(right, rect.center().y),
            Align2::RIGHT_CENTER,
            summary,
            FontId::proportional(11.5),
            TEXT_WEAK,
        );
    }
    if header.clicked() && !reset {
        *open = !*open;
    }
    if *open {
        Frame::NONE
            .inner_margin(Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 14,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 10.0;
                body(ui);
            });
    }
    let line = ui.cursor().top();
    ui.painter()
        .hline(rect.x_range(), line, Stroke::new(1.0, DIVIDER));
    ui.add_space(1.0);
    reset
}

/// A label with its value on the right, over a full-width slider.
#[allow(clippy::too_many_arguments)]
pub fn slider_row(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    step: f64,
    unit: &str,
    decimals: usize,
    logarithmic: bool,
) -> Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(12.0).color(TEXT_LABEL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{value:.decimals$}{unit}"))
                        .font(mono(12.0))
                        .color(TEXT),
                );
            });
        });
        ui.spacing_mut().slider_width = ui.available_width();
        // The handle's size follows the row height: 16 px gives the design's 12 px knob.
        ui.spacing_mut().interact_size.y = 16.0;
        let mut slider = egui::Slider::new(value, range)
            .show_value(false)
            .handle_shape(egui::style::HandleShape::Circle)
            .logarithmic(logarithmic);
        if step > 0.0 {
            slider = slider.step_by(step);
        }
        ui.add(slider)
    })
    .inner
}

/// A label with `text` (the value as people write it) on the right, over a full-width slider
/// of whole steps.
pub fn step_slider_row(
    ui: &mut Ui,
    label: &str,
    text: &str,
    value: &mut u32,
    range: RangeInclusive<u32>,
) -> Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(12.0).color(TEXT_LABEL));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(text).font(mono(12.0)).color(TEXT));
            });
        });
        ui.spacing_mut().slider_width = ui.available_width();
        ui.spacing_mut().interact_size.y = 16.0;
        ui.add(
            egui::Slider::new(value, range)
                .show_value(false)
                .handle_shape(egui::style::HandleShape::Circle),
        )
    })
    .inner
}

/// A row of mutually exclusive options. Returns the newly picked index.
pub fn segmented(
    ui: &mut Ui,
    label: Option<&str>,
    options: &[&str],
    selected: usize,
) -> Option<usize> {
    let mut picked = None;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        if let Some(label) = label {
            ui.label(RichText::new(label).size(12.0).color(TEXT_LABEL));
        }
        Frame::NONE
            .fill(BG_FIELD)
            .stroke(Stroke::new(1.0, STROKE_FIELD))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(2))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let n = options.len().max(1) as f32;
                let width = (ui.available_width() - 2.0 * (n - 1.0)) / n;
                ui.horizontal(|ui| {
                    for (i, option) in options.iter().enumerate() {
                        let (rect, response) =
                            ui.allocate_exact_size(vec2(width, 26.0), Sense::click());
                        let on = i == selected;
                        response
                            .widget_info(|| WidgetInfo::selected(Role::Button, true, on, *option));
                        let bg = if on {
                            BG_SEGMENT_ON
                        } else if response.hovered() {
                            Color32::from_gray(0x2c)
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(rect, 4.0, bg);
                        ui.painter().text(
                            rect.center(),
                            Align2::CENTER_CENTER,
                            *option,
                            FontId::proportional(12.0),
                            if on {
                                Color32::WHITE
                            } else {
                                Color32::from_gray(0xbd)
                            },
                        );
                        if response.clicked() && !on {
                            picked = Some(i);
                        }
                    }
                });
            });
    });
    picked
}

/// A label with an on/off switch on the right. Returns true when it changed.
pub fn toggle_row(ui: &mut Ui, label: &str, on: &mut bool) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(TEXT_LABEL));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (rect, response) = ui.allocate_exact_size(vec2(34.0, 20.0), Sense::click());
            response.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
            if response.clicked() {
                *on = !*on;
            }
            let t = ui.ctx().animate_bool(response.id, *on);
            let track = Color32::from_gray(if *on { 0x8a } else { 0x4a });
            ui.painter().rect_filled(rect, 10.0, track);
            let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
            ui.painter()
                .circle_filled(pos2(x, rect.center().y), 7.0, Color32::from_gray(0xf4));
            response.clicked()
        })
        .inner
    })
    .inner
}

/// A label on the left and a value on the right.
pub fn info_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0).color(TEXT_WEAK));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).size(12.0).color(TEXT));
        });
    });
}

pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(TEXT_WEAK));
}

/// A small rounded tag.
pub fn chip(ui: &mut Ui, text: &str) {
    Frame::NONE
        .fill(BG_FIELD)
        .stroke(Stroke::new(1.0, STROKE_FIELD))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(TEXT_LABEL));
        });
}

/// A keyboard key cap, for hints.
pub fn key_cap(ui: &mut Ui, key: &str) {
    Frame::NONE
        .fill(Color32::from_gray(0x33))
        .stroke(Stroke::new(1.0, Color32::from_gray(0x48)))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(5, 0))
        .show(ui, |ui| {
            // Symbol keys (⌥, ⌘, ↩) are small in the mono font; give them a larger face.
            let font = if key.is_ascii() {
                mono(11.0)
            } else {
                FontId::proportional(14.0)
            };
            ui.label(RichText::new(key).font(font).color(TEXT));
        });
}

/// The tool's main action: full width, accent colored, with its shortcut.
pub fn primary_button(
    ui: &mut Ui,
    text: &str,
    shortcut: &str,
    enabled: bool,
    busy: bool,
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = action_rect(ui, sense);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, text));
    let fill = match (enabled, response.hovered()) {
        (false, _) => ACCENT_DIM,
        (true, true) => ACCENT_HOVER,
        (true, false) => ACCENT,
    };
    paint_action(ui, rect, fill, text, shortcut, busy);
    response
}

/// The tool's main action while it runs, in the same place: `text` with a spinner, dimmed;
/// under the pointer it lights up as `stop` with its shortcut, for a click stops the run.
pub fn running_button(ui: &mut Ui, text: &str, stop: &str, shortcut: &str) -> Response {
    let (rect, response) = action_rect(ui, Sense::click());
    let hovered = response.hovered();
    let label = if hovered { stop } else { text };
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    let fill = if hovered { ACCENT } else { ACCENT_DIM };
    let shortcut = if hovered { shortcut } else { "" };
    paint_action(ui, rect, fill, label, shortcut, !hovered);
    response
}

/// The id of the tool's main action, the same whatever its state.
pub fn action_id() -> egui::Id {
    egui::Id::unique("tool-action")
}

/// The tool's main action's place: the full width, 38 px high.
fn action_rect(ui: &mut Ui, sense: Sense) -> (Rect, Response) {
    let (_, rect) = ui.allocate_space(vec2(ui.available_width(), 38.0));
    (rect, ui.interact(rect, action_id(), sense))
}

/// The action's face: `text` and its `shortcut` centered, after a spinner when `busy`.
fn paint_action(ui: &Ui, rect: Rect, fill: Color32, text: &str, shortcut: &str, busy: bool) {
    let painter = ui.painter();
    painter.rect_filled(rect, 7.0, fill);
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::new(13.5, semibold()), ON_ACCENT);
    let key = painter.layout_no_wrap(shortcut.to_owned(), FontId::proportional(12.5), ON_ACCENT);
    let spinner = if busy { 22.0 } else { 0.0 };
    let total = spinner + galley.size().x + 10.0 + key.size().x;
    let mut x = rect.center().x - total / 2.0;
    if busy {
        egui::Spinner::new().size(14.0).color(ON_ACCENT).paint_at(
            ui,
            Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)),
        );
        x += spinner;
    }
    let painter = ui.painter();
    painter.galley(
        pos2(x, rect.center().y - galley.size().y / 2.0),
        galley.clone(),
        ON_ACCENT,
    );
    x += galley.size().x + 10.0;
    painter.galley(
        pos2(x, rect.center().y - key.size().y / 2.0),
        key,
        ON_ACCENT,
    );
}

/// A plain button sized to the row height.
/// A toolbar-sized button in the accent color, for the main action there (Save & Return).
pub fn accent_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::new(12.5, semibold()), ON_ACCENT);
    let size = vec2(galley.size().x + 24.0, 30.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, text));
    let fill = match (enabled, response.hovered()) {
        (false, _) => ACCENT_DIM,
        (true, true) => ACCENT_HOVER,
        (true, false) => ACCENT,
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, fill);
    painter.galley(rect.center() - galley.size() / 2.0, galley, ON_ACCENT);
    response
}

pub fn button(ui: &mut Ui, text: &str, enabled: bool, width: Option<f32>) -> Response {
    let mut button = egui::Button::new(RichText::new(text).size(12.0)).min_size(vec2(0.0, 28.0));
    if let Some(width) = width {
        button = button.min_size(vec2(width, 28.0));
    }
    ui.add_enabled(enabled, button)
}

/// A filled dot, for status lines.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Strokes a rectangle with dashes, for drop zones.
pub fn dashed_rect(ui: &Ui, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    ui.painter()
        .add(egui::Shape::dashed_line(&corners, stroke, 8.0, 6.0));
}

/// A rounded panel for start-screen and settings content.
pub fn card(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    Frame::NONE
        .fill(BG_PANEL)
        .stroke(Stroke::new(1.0, Color32::from_gray(0x34)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            body(ui);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{RawInput, Shape};

    /// Fill colors of the filled rectangles and circles in `shapes`.
    fn fills(shapes: &[Shape], out: &mut Vec<Color32>) {
        for shape in shapes {
            match shape {
                Shape::Rect(rect) => out.push(rect.fill),
                Shape::Circle(circle) => out.push(circle.fill),
                Shape::Vec(shapes) => fills(shapes, out),
                _ => {}
            }
        }
    }

    /// The "Show mask" switch is neutral when on: the accent is for the primary action and
    /// the active tool.
    #[test]
    fn a_switch_that_is_on_is_not_accent_colored() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let mut on = true;
        let output = ctx.run_ui(RawInput::default(), |ui| {
            toggle_row(ui, "Show mask", &mut on);
        });
        let shapes: Vec<Shape> = output.shapes.iter().map(|s| s.shape.clone()).collect();
        output.drop_without_applying_deltas();
        let mut colors = Vec::new();
        fills(&shapes, &mut colors);
        assert!(
            colors.contains(&Color32::from_gray(0x8a)),
            "the track is drawn"
        );
        assert!(!colors.contains(&ACCENT));
    }

    /// Fill colors of `shapes`, with the vertex colors of meshes (tinted icons).
    fn colors(shapes: &[Shape], out: &mut Vec<Color32>) {
        fills(shapes, out);
        for shape in shapes {
            match shape {
                Shape::Mesh(mesh) => out.extend(mesh.vertices.iter().map(|v| v.color)),
                Shape::Vec(shapes) => colors(shapes, out),
                _ => {}
            }
        }
    }

    /// The colors a selected rail button paints with `tint`, once its icon has loaded.
    fn selected_rail_button(tint: Color32) -> Vec<Color32> {
        let ctx = egui::Context::default();
        apply(&ctx);
        egui_extras::install_image_loaders(&ctx);
        let mut out = Vec::new();
        for _ in 0..3 {
            let output = ctx.run_ui(RawInput::default(), |ui| {
                rail_button(ui, icons::MODELS, "Models", "Models", true, tint);
            });
            let shapes: Vec<Shape> = output.shapes.iter().map(|s| s.shape.clone()).collect();
            output.drop_without_applying_deltas();
            out.clear();
            colors(&shapes, &mut out);
        }
        out
    }

    /// The Models page is not a tool: its rail button is neutral when selected.
    #[test]
    fn a_selected_rail_button_takes_its_tint() {
        let tool = selected_rail_button(ACCENT);
        assert!(
            tool.contains(&ACCENT),
            "the icon is drawn, in the accent for a tool"
        );
        let page = selected_rail_button(TEXT_STRONG);
        assert!(page.contains(&TEXT_STRONG));
        assert!(!page.contains(&ACCENT));
    }
}
