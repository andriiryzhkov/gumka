// SPDX-License-Identifier: GPL-3.0-or-later

//! The Config page: Gumka's version; then every model Gumka uses, with its license and source,
//! where it runs, where its files are and whether it is ready; then the places models can run.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Sense,
    Stroke, Ui, pos2, vec2,
};
use gumka_infer::models::{self, ModelInfo};

use crate::theme::*;
use crate::widgets;
use crate::worker::{BACKEND, FOUND, Status, THIS_COMPUTER, runtime_problem};

/// Gumka's version, from build.rs (see docs/packaging.md).
pub const VERSION: &str = env!("GUMKA_VERSION");

/// A catalog entry with what was found on disk.
pub struct ModelRow {
    info: &'static ModelInfo,
    path: Result<PathBuf, String>,
    /// Not found complete, but part of it is there (an interrupted download).
    incomplete: bool,
    size: u64,
}

/// Looks every catalog model for this platform up on disk. Cheap: a few metadata reads per
/// model.
pub fn load_rows() -> Vec<ModelRow> {
    models::registry()
        .iter()
        .filter(|info| info.for_this_platform())
        .map(|info| {
            let found = info.find_packages().map(|paths| {
                paths
                    .into_iter()
                    .map(|p| p.canonicalize().unwrap_or(p))
                    .collect::<Vec<PathBuf>>()
            });
            let incomplete = found.as_ref().is_err_and(|e| e.incomplete);
            let found = found.map_err(String::from);
            let size = found
                .as_ref()
                .map_or(0, |paths| paths.iter().map(|p| models::disk_size(p)).sum());
            let path =
                found.and_then(|paths| paths.into_iter().next().ok_or_else(|| "no files".into()));
            ModelRow {
                info,
                path,
                incomplete,
                size,
            }
        })
        .collect()
}

/// `status` is the worker's state of a model, for those it has loaded or tried to.
pub fn show<'a>(ui: &mut Ui, rows: &[ModelRow], status: &dyn Fn(&ModelInfo) -> Option<&'a Status>) {
    let status_of = |row: &ModelRow| status(row.info);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let width = (ui.available_width() - 72.0).clamp(560.0, 1100.0);
            let side = ((ui.available_width() - width) / 2.0).max(36.0);
            ui.add_space(32.0);
            ui.horizontal_top(|ui| {
                ui.add_space(side);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(
                        RichText::new("Version")
                            .font(FontId::new(20.0, semibold()))
                            .color(TEXT),
                    );
                    // Selectable, to copy into a bug report.
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("Gumka {VERSION}"))
                                .size(13.0)
                                .color(TEXT),
                        )
                        .selectable(true),
                    );
                    ui.add_space(28.0);
                    ui.label(
                        RichText::new("Models")
                            .font(FontId::new(20.0, semibold()))
                            .color(TEXT),
                    );
                    let intro = if cfg!(target_os = "macos") {
                        "Models that run on this Mac come with Gumka, so every tool works \
                         offline from the start. Only models whose license allows commercial \
                         use are included."
                    } else if cfg!(target_os = "linux") {
                        "Models run on this computer, so every tool works offline. Only models \
                         whose license allows commercial use are listed."
                    } else {
                        "Local models aren't available on this platform yet."
                    };
                    ui.label(
                        RichText::new(intro)
                            .size(13.0)
                            .color(Color32::from_gray(0xb5)),
                    );
                    ui.add_space(16.0);
                    if !rows.is_empty() {
                        table(ui, rows, &status_of);
                    }
                    ui.add_space(28.0);
                    ui.label(
                        RichText::new("Where models run")
                            .font(FontId::new(15.0, semibold()))
                            .color(TEXT),
                    );
                    ui.add_space(4.0);
                    ui.columns(2, |columns| {
                        this_computer_card(&mut columns[0], rows);
                        api_card(&mut columns[1]);
                    });
                });
            });
            ui.add_space(40.0);
        });
}

const HEADERS: [&str; 5] = ["Tool", "Model", "License", "Runs on", "Status"];

/// Column widths for a table `width` wide; the model column takes the rest.
fn columns(width: f32) -> [f32; 5] {
    let fixed = [120.0, 0.0, 190.0, 200.0, 150.0];
    let model = (width - 28.0 - fixed.iter().sum::<f32>()).max(220.0);
    [fixed[0], model, fixed[2], fixed[3], fixed[4]]
}

fn table<'a>(ui: &mut Ui, rows: &[ModelRow], status_of: &impl Fn(&ModelRow) -> Option<&'a Status>) {
    Frame::NONE
        .stroke(Stroke::new(1.0, Color32::from_gray(0x34)))
        .corner_radius(CornerRadius::same(8))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let width = ui.available_width();
            let widths = columns(width);
            let (rect, _) = ui.allocate_exact_size(vec2(width, 34.0), Sense::hover());
            let top = CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            };
            ui.painter().rect_filled(rect, top, BG_SECTION);
            let mut x = rect.left() + 14.0;
            for (title, w) in HEADERS.iter().zip(widths) {
                ui.painter().text(
                    pos2(x, rect.center().y),
                    Align2::LEFT_CENTER,
                    title,
                    FontId::new(12.5, medium()),
                    TEXT_LABEL,
                );
                x += w;
            }
            for row in rows {
                let line = ui.cursor().top();
                ui.painter().hline(
                    rect.x_range(),
                    line,
                    Stroke::new(1.0, Color32::from_gray(0x34)),
                );
                model_row(ui, row, &widths, status_of(row));
            }
        });
}

fn model_row(ui: &mut Ui, row: &ModelRow, widths: &[f32; 5], status: Option<&Status>) {
    let info = row.info;
    Frame::NONE
        .inner_margin(Margin {
            left: 14,
            right: 14,
            top: 12,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                cell(ui, widths[0], |ui| {
                    ui.label(
                        RichText::new(&info.tool)
                            .font(FontId::new(13.0, medium()))
                            .color(TEXT),
                    );
                });
                cell(ui, widths[1], |ui| {
                    ui.label(RichText::new(&info.name).size(13.0).color(TEXT));
                    small(ui, &info.summary);
                    let size = match &row.path {
                        Ok(_) => format!(
                            "{} · {} · {}",
                            info.format,
                            format_size(row.size),
                            FOUND.to_lowercase()
                        ),
                        Err(_) if row.incomplete => format!("{} · incomplete", info.format),
                        Err(_) => format!("{} · not downloaded", info.format),
                    };
                    small(ui, &size);
                });
                cell(ui, widths[2], |ui| {
                    ui.label(
                        RichText::new(&info.license)
                            .font(mono(12.0))
                            .color(Color32::from_gray(0xcf)),
                    );
                    ui.hyperlink_to(
                        RichText::new(&info.source).size(12.0).underline(),
                        &info.source_url,
                    );
                });
                cell(ui, widths[3], |ui| {
                    let runs_on = match status {
                        Some(Status::Ready { runs_on, .. }) => {
                            format!("{THIS_COMPUTER} · {runs_on}")
                        }
                        _ => format!("{THIS_COMPUTER} · {BACKEND}"),
                    };
                    ui.label(RichText::new(runs_on).size(12.5).color(TEXT));
                });
                cell(ui, widths[4], |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    status_line(ui, row, status);
                    if let Ok(path) = &row.path
                        && widgets::button(ui, reveal_label(), true, None).clicked()
                    {
                        reveal(path);
                    }
                });
            });
        });
}

/// The model's state: missing or incomplete on disk, else the worker's (`None`: not loaded
/// yet, and found complete).
fn status_line(ui: &mut Ui, row: &ModelRow, status: Option<&Status>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if let Err(reason) = &row.path {
            widgets::dot(ui, WARN);
            let label = if row.incomplete {
                "Incomplete"
            } else {
                "Missing"
            };
            ui.label(RichText::new(label).size(12.5).color(WARN))
                .on_hover_text(reason);
            return;
        }
        match status {
            Some(Status::Ready { .. }) => {
                widgets::dot(ui, OK);
                ui.label(RichText::new("Ready").size(12.5).color(TEXT));
            }
            None | Some(Status::NotLoaded) => {
                widgets::dot(ui, OK);
                ui.label(RichText::new(FOUND).size(12.5).color(TEXT))
                    .on_hover_text("Loads when the tool first runs");
            }
            Some(Status::Loading) => {
                ui.spinner();
                ui.label(RichText::new("Loading…").size(12.5).color(TEXT_LABEL));
            }
            Some(Status::Unavailable(reason)) => {
                widgets::dot(ui, WARN);
                ui.label(RichText::new("Unavailable").size(12.5).color(WARN))
                    .on_hover_text(reason);
            }
        }
    });
}

/// The local card's name, state, text and state color; `problem` is why ONNX Runtime didn't
/// load, if it didn't.
fn local_state(
    problem: Option<&'static str>,
) -> (&'static str, &'static str, &'static str, Color32) {
    if cfg!(target_os = "macos") {
        (
            "This Mac",
            "Always on",
            "Core ML on Apple silicon, with the models that come with Gumka. Nothing leaves the \
             computer.",
            OK,
        )
    } else if !cfg!(target_os = "linux") {
        (
            "This computer",
            "Not available yet",
            "Local models need ONNX Runtime on this platform, which comes later.",
            TEXT_WEAK,
        )
    } else if let Some(reason) = problem {
        ("This computer", "Unavailable", reason, WARN)
    } else {
        (
            "This computer",
            "Always on",
            "ONNX Runtime from your system, on the CPU, with the models in Gumka's models \
             folder. Nothing leaves the computer.",
            OK,
        )
    }
}

fn this_computer_card(ui: &mut Ui, rows: &[ModelRow]) {
    widgets::card(ui, |ui| {
        let (name, state, text, color) = local_state(runtime_problem());
        card_header(ui, name, state, color);
        ui.label(RichText::new(text).size(12.5).color(TEXT_LABEL));
        let found: Vec<&ModelRow> = rows.iter().filter(|r| r.path.is_ok()).collect();
        let total = found.iter().map(|r| r.size).sum();
        widgets::info_row(
            ui,
            &format!("{FOUND} models"),
            &format!("{} · {}", found.len(), format_size(total)),
        );
        // Only Core ML compiles its models.
        if cfg!(target_os = "macos") {
            let cache = models::cache_dir();
            widgets::info_row(ui, "Compiled copies", &tilde(&cache));
            if cache.exists() && widgets::button(ui, reveal_label(), true, None).clicked() {
                reveal(&cache);
            }
        }
    });
}

fn api_card(ui: &mut Ui) {
    widgets::card(ui, |ui| {
        card_header(ui, "API", "Not available yet", TEXT_WEAK);
        ui.label(
            RichText::new(
                "Fill and Extend will run through an API with your own key, kept in the \
                 Keychain. It stays off until you turn it on, receives only the crop around \
                 the mask, and the toolbar shows a badge while it is in use.",
            )
            .size(12.5)
            .color(TEXT_LABEL),
        );
    });
}

fn card_header(ui: &mut Ui, title: &str, state: &str, state_color: Color32) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .font(FontId::new(13.5, semibold()))
                .color(TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(state).size(12.0).color(state_color));
        });
    });
}

/// A fixed-width table cell.
fn cell(ui: &mut Ui, width: f32, body: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width - 12.0);
        ui.spacing_mut().item_spacing.y = 3.0;
        body(ui);
    });
}

fn small(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(TEXT_WEAK));
}

/// Decimal units, as Finder shows them.
fn format_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn tilde(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text,
    }
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show in folder"
    }
}

/// Opens the system file browser at `path`.
fn reveal(path: &Path) {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("explorer").arg("/select,").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(e) = result {
        eprintln!("could not open {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Linux the card says when ONNX Runtime is missing, and why, instead of "Always on".
    #[test]
    #[cfg(target_os = "linux")]
    fn the_card_says_when_onnx_runtime_is_missing() {
        let reason = "ONNX Runtime is not installed.";
        let (_, state, text, color) = local_state(Some(reason));
        assert_eq!((state, text, color), ("Unavailable", reason, WARN));
        let (name, state, _, color) = local_state(None);
        assert_eq!((name, state, color), ("This computer", "Always on", OK));
        assert_eq!(FOUND, "Downloaded");
    }

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(format_size(216_646_769), "217 MB");
        assert_eq!(format_size(5_200_000_000), "5.2 GB");
    }
}
