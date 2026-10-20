// SPDX-License-Identifier: GPL-3.0-or-later

//! Model backends for Gumka.
//!
//! Only the Core ML backend (macOS) exists so far. Models plug into the pipeline through
//! [`gumka_core::erase::Inpainter`] (LaMa) and [`gumka_core::select::Segmenter`] (SAM).

#[cfg(target_os = "macos")]
pub mod coreml;
#[cfg(target_os = "macos")]
mod lama;
pub mod models;
#[cfg(target_os = "macos")]
mod sam;

#[cfg(target_os = "macos")]
pub use lama::CoreMlLama;
#[cfg(target_os = "macos")]
pub use sam::CoreMlSam;
