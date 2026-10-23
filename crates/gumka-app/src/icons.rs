// SPDX-License-Identifier: GPL-3.0-or-later

//! Stroke icons from the design, as SVG drawn white and tinted when painted.

use eframe::egui::{self, Color32, ImageSource, Rect, Ui};

macro_rules! icon {
    ($name:ident, $path:literal) => {
        pub const $name: Icon = Icon {
            uri: concat!("bytes://gumka/", stringify!($name), ".svg"),
            svg: concat!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d=""#,
                $path,
                r#""/></svg>"#
            ),
        };
    };
}

#[derive(Clone, Copy)]
pub struct Icon {
    uri: &'static str,
    svg: &'static str,
}

impl Icon {
    pub fn image(self) -> egui::Image<'static> {
        egui::Image::new(ImageSource::Bytes {
            uri: self.uri.into(),
            bytes: egui::load::Bytes::Static(self.svg.as_bytes()),
        })
    }

    pub fn paint(self, ui: &Ui, rect: Rect, color: Color32) {
        self.image().tint(color).paint_at(ui, rect);
    }
}

icon!(
    ERASE,
    "M20 20H9M3.6 14.4l9-9a2 2 0 0 1 2.8 0l3.2 3.2a2 2 0 0 1 0 2.8L12 18H7.6l-4-4a1 1 0 0 1 0-1.4zM9 9l6 6"
);
// Arrows out from the corners: the picture made larger.
icon!(UPSCALE, "M14 4h6v6M20 4l-6 6M10 20H4v-6M4 20l6-6");
icon!(
    OPEN,
    "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"
);
icon!(SAVE, "M12 3v12M7 10l5 5 5-5M4 20h16");
icon!(UNDO, "M9 14L4 9l5-5M4 9h10.5a5.5 5.5 0 0 1 0 11H11");
icon!(REDO, "M15 14l5-5-5-5M20 9H9.5a5.5 5.5 0 0 0 0 11H13");
icon!(
    BRUSH,
    "M9.5 14.5c-2 0-3.5 1.5-3.5 3.5 0 1-1 2-2 2 1 1 3 1.5 4.5 1.5 2.5 0 4.5-2 4.5-4.5 0-1.4-1.1-2.5-2.5-2.5zM12.5 14L21 5.5a1.9 1.9 0 0 0-2.7-2.7L10 11.3"
);
icon!(REMOVE, "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18zM8 12h8");
icon!(
    SELECT,
    "M15 4V2M15 16v-2M8 9h2M20 9h2M17.8 11.8l1.4 1.4M17.8 6.2l1.4-1.4M12.2 6.2l-1.4-1.4M3 21l9-9"
);
icon!(
    MODELS,
    "M7 7h10v10H7zM10 10h4v4h-4zM9 3v4M15 3v4M9 17v4M15 17v4M3 9h4M3 15h4M17 9h4M17 15h4"
);
icon!(COMPARE, "M4 5h16v14H4zM12 5v14");
icon!(CHEVRON_DOWN, "M6 9l6 6 6-6");
icon!(CHEVRON_RIGHT, "M9 6l6 6-6 6");
icon!(RESET, "M3 12a9 9 0 1 0 3-6.7L3 8M3 3v5h5");
icon!(
    PHOTO,
    "M4 5h16v14H4zM4 15l4.5-4.5 3.5 3.5 2.5-2.5L20 17M15.5 8.5a1.5 1.5 0 1 0 0 .01"
);
