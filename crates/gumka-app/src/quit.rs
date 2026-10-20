// SPDX-License-Identifier: GPL-3.0-or-later

//! Quitting asks about unsaved changes, as closing the window does.
//!
//! On macOS, ⌘Q, Quit in the app and Dock menus, and logging out all end in
//! `-[NSApplication terminate:]`, which winit lets through: its application delegate has no
//! `applicationShouldTerminate:`. One is added to it at runtime. While there are unsaved
//! changes it cancels the quit, brings the window back, and asks the window to close instead;
//! the app answers that with "Save changes?" and closes once it has an answer, which ends the
//! app. A logout is cancelled along with the quit; the user logs out again after answering.
//! Without unsaved changes the quit goes ahead.
//!
//! Elsewhere quitting is closing the window, which the app handles already.

#[cfg(target_os = "macos")]
pub use macos::{install, set_unsaved};

/// Installs the quit handler. Call once, on the main thread, before the first frame.
#[cfg(not(target_os = "macos"))]
pub fn install(_ctx: &eframe::egui::Context) {}

/// Tells the quit handler whether quitting now would lose changes.
#[cfg(not(target_os = "macos"))]
pub fn set_unsaved(_unsaved: bool) {}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use eframe::egui;
    use objc2::encode::Encode;
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{MainThreadMarker, sel};
    use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};

    /// Set by the app after each frame; read when a quit comes in.
    static UNSAVED: AtomicBool = AtomicBool::new(false);
    /// Where the close request goes.
    static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

    type ShouldTerminate =
        extern "C" fn(&AnyObject, Sel, *mut AnyObject) -> NSApplicationTerminateReply;

    /// Installs the quit handler. Call once, on the main thread, before the first frame.
    pub fn install(ctx: &egui::Context) {
        if CONTEXT.set(ctx.clone()).is_err() {
            return; // installed already
        }
        // NSApplicationTerminateReply, then self, _cmd and the sender.
        let types = format!(
            "{}{}{}{}",
            NSApplicationTerminateReply::ENCODING,
            <&AnyObject>::ENCODING,
            Sel::ENCODING,
            <*mut AnyObject>::ENCODING,
        );
        let method: ShouldTerminate = should_terminate;
        // SAFETY: `Imp` is the runtime's type-erased method pointer. It is only ever called
        // by `objc_msgSend` with the receiver, the selector and the argument and return types
        // that `types` gives, and those match `should_terminate`'s signature.
        let imp = unsafe { std::mem::transmute::<ShouldTerminate, Imp>(method) };
        // SAFETY: `should_terminate` has the signature of NSApplicationDelegate's
        // `applicationShouldTerminate:`, which `types` encodes.
        let added =
            unsafe { crate::delegate::add_method(sel!(applicationShouldTerminate:), imp, &types) };
        if let Err(why) = added {
            eprintln!("{why}; quitting won't ask about unsaved changes");
        }
    }

    /// Tells the quit handler whether quitting now would lose changes.
    pub fn set_unsaved(unsaved: bool) {
        UNSAVED.store(unsaved, Ordering::Relaxed);
    }

    /// `-[NSApplicationDelegate applicationShouldTerminate:]`. Called on the main thread,
    /// between frames.
    extern "C" fn should_terminate(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
    ) -> NSApplicationTerminateReply {
        let (Some(mtm), Some(ctx)) = (MainThreadMarker::new(), CONTEXT.get()) else {
            return NSApplicationTerminateReply::TerminateNow;
        };
        if !UNSAVED.load(Ordering::Relaxed) {
            return NSApplicationTerminateReply::TerminateNow;
        }
        // The window may be hidden or in the Dock; the question needs it on screen.
        let app = NSApplication::sharedApplication(mtm);
        app.unhide(None);
        for window in app.windows() {
            if window.isMiniaturized() {
                window.deminiaturize(None);
            }
        }
        #[allow(deprecated)] // its replacement, `activate`, needs macOS 14
        app.activateIgnoringOtherApps(true);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        NSApplicationTerminateReply::TerminateCancel
    }
}
