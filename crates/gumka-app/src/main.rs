// SPDX-License-Identifier: GPL-3.0-or-later

//! Gumka desktop app: `gumka [IMAGE]`.

mod app;
mod canvas;
#[cfg(target_os = "macos")]
mod delegate;
mod display;
mod document;
mod icons;
mod models_page;
mod open_files;
mod quit;
mod theme;
mod widgets;
mod worker;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Gumka")
            // Linux desktops match the window to the installed `.desktop` file by this id, for
            // its icon and name; the AppImage's is named after it.
            .with_app_id("io.github.andriiryzhkov.gumka")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([560.0, 360.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    // Our own event loop rather than eframe::run_native, so that the handler for files other
    // apps send is in place before the loop runs: a file that starts Gumka arrives while it is
    // still launching.
    let event_loop =
        winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
    open_files::install();
    let mut app = eframe::create_native(
        "Gumka",
        options,
        Box::new(move |cc| Ok(Box::new(app::GumkaApp::new(cc, path)))),
        &event_loop,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// The logo (assets/logo/gumka-icon.svg) rendered for the Dock and the window.
fn app_icon() -> egui::IconData {
    const SIZE: u32 = 512;
    let render = || -> Option<egui::IconData> {
        let svg = include_bytes!("../../../assets/logo/gumka-icon.svg");
        let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default()).ok()?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE)?;
        let scale = SIZE as f32 / tree.size().width();
        let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia keeps premultiplied alpha; the icon wants it unmultiplied.
        let rgba = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        Some(egui::IconData {
            rgba,
            width: SIZE,
            height: SIZE,
        })
    };
    render().unwrap_or_default()
}
