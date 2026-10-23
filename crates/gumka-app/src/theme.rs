// SPDX-License-Identifier: GPL-3.0-or-later

//! Colors, fonts and widget styling from the design (see docs/ui.md):
//! neutral dark grays, a mid-gray viewer, one amber accent for the primary action and the
//! active tool.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle,
};

pub const ACCENT: Color32 = Color32::from_rgb(0xf2, 0xa5, 0x41);
/// The accent under the pointer.
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xff, 0xb5, 0x55);
/// The accent of an action that can't run now, or is running.
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x6b, 0x55, 0x36);
/// Text on the accent color.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);

pub const BG_STATUS: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
pub const BG_RAIL: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f);
pub const BG_HISTORY: Color32 = Color32::from_rgb(0x23, 0x23, 0x23);
pub const BG_TOOLBAR: Color32 = Color32::from_rgb(0x24, 0x24, 0x24);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
pub const BG_CARD: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x2b);
pub const BG_SECTION: Color32 = Color32::from_rgb(0x2d, 0x2d, 0x2d);
pub const BG_VIEWER: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
pub const BG_FIELD: Color32 = Color32::from_rgb(0x1d, 0x1d, 0x1d);
pub const BG_BUTTON: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
pub const BG_SELECTED: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
pub const BG_SEGMENT_ON: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
pub const BG_CURRENT_ROW: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);

pub const DIVIDER: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
pub const STROKE_FIELD: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
pub const STROKE_BUTTON: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);

pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
pub const TEXT_LABEL: Color32 = Color32::from_rgb(0xc4, 0xc4, 0xc4);
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0xab, 0xab, 0xab);
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x80, 0x80, 0x80);
pub const ICON: Color32 = Color32::from_rgb(0xd4, 0xd4, 0xd4);

pub const OK: Color32 = Color32::from_rgb(0x36, 0xc9, 0x8a);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xc2, 0x7a);

/// Mask overlay color (unmultiplied) and its opacity.
pub const MASK_RGB: [u8; 3] = [0xff, 0x4a, 0x5e];
pub const MASK_OPACITY: f32 = 0.5;

/// Font families beyond egui's proportional and monospace.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (TextStyle::Heading, FontId::new(15.0, semibold())),
            (
                TextStyle::Monospace,
                FontId::new(12.0, FontFamily::Monospace),
            ),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 6.0);
        s.button_padding = egui::vec2(10.0, 4.0);
        s.interact_size.y = 28.0;
        s.slider_rail_height = 3.0;
        s.menu_margin = Margin::same(6);

        let v = &mut style.visuals;
        v.panel_fill = BG_PANEL;
        v.window_fill = BG_CARD;
        v.window_stroke = Stroke::new(1.0, STROKE_BUTTON);
        v.window_corner_radius = CornerRadius::same(10);
        v.extreme_bg_color = BG_FIELD;
        v.faint_bg_color = BG_SECTION;
        v.override_text_color = None;
        // Links and selected buttons stay neutral: the accent is for the primary action and
        // the active tool only. Links are underlined where they are made.
        v.hyperlink_color = TEXT;
        v.selection.bg_fill = BG_SEGMENT_ON;
        v.selection.stroke = Stroke::new(1.0, TEXT_STRONG);
        v.slider_trailing_fill = false;

        let radius = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG_PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.bg_fill = BG_BUTTON;
        w.inactive.weak_bg_fill = BG_BUTTON;
        w.inactive.bg_stroke = Stroke::new(1.0, STROKE_BUTTON);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = radius;
        w.hovered.bg_fill = BG_SELECTED;
        w.hovered.weak_bg_fill = BG_SELECTED;
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(0x5a));
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
        w.hovered.corner_radius = radius;
        w.active.bg_fill = BG_SEGMENT_ON;
        w.active.weak_bg_fill = BG_SEGMENT_ON;
        w.active.bg_stroke = Stroke::new(1.0, Color32::from_gray(0x66));
        w.active.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
        w.active.corner_radius = radius;
        w.open = w.active;
    });
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "plex",
        include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf"),
    );
    add(
        &mut fonts,
        "plex-medium",
        include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf"),
    );
    add(
        &mut fonts,
        "plex-semibold",
        include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf"),
    );
    add(
        &mut fonts,
        "plex-mono",
        include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    );

    // Fallbacks for glyphs Plex lacks: the system's symbol font for ⌘ ⌥ ⇧ ↩, then egui's own.
    let fallbacks: Vec<String> = symbol_font(&mut fonts).into_iter().collect();
    let defaults = |family: &FontFamily| -> Vec<String> {
        let mut list = fallbacks.clone();
        list.extend(fonts.families.get(family).cloned().unwrap_or_default());
        list
    };
    let proportional = defaults(&FontFamily::Proportional);
    let monospace = defaults(&FontFamily::Monospace);
    let with = |first: &str, rest: &[String]| {
        std::iter::once(first.to_owned())
            .chain(rest.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts
        .families
        .insert(FontFamily::Proportional, with("plex", &proportional));
    fonts
        .families
        .insert(FontFamily::Monospace, with("plex-mono", &monospace));
    fonts
        .families
        .insert(medium(), with("plex-medium", &proportional));
    fonts
        .families
        .insert(semibold(), with("plex-semibold", &proportional));
    fonts
}

/// macOS ships a symbol font with ⌘ ⌥ ⇧ ↩. It is read at runtime, not bundled.
#[cfg(target_os = "macos")]
fn symbol_font(fonts: &mut FontDefinitions) -> Option<String> {
    let bytes = std::fs::read("/System/Library/Fonts/Apple Symbols.ttf").ok()?;
    fonts
        .font_data
        .insert("symbols".into(), Arc::new(FontData::from_owned(bytes)));
    Some("symbols".into())
}

#[cfg(not(target_os = "macos"))]
fn symbol_font(_: &mut FontDefinitions) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accent is for the primary action and the active tool: links and selected buttons
    /// (such as Before / After) stay neutral.
    #[test]
    fn links_and_selected_buttons_are_not_accent_colored() {
        let ctx = egui::Context::default();
        apply(&ctx);
        ctx.all_styles_mut(|style| {
            let v = &style.visuals;
            assert_ne!(v.hyperlink_color, ACCENT);
            assert_ne!(v.selection.stroke.color, ACCENT);
            assert_ne!(v.selection.bg_fill, ACCENT);
        });
    }
}
