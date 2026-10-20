// SPDX-License-Identifier: GPL-3.0-or-later

//! Files other apps hand to Gumka: Finder's Open With, a file dropped on the Dock icon, a RAW
//! editor's Edit With.
//!
//! On macOS these arrive as an "open documents" event, `application:openURLs:` on the
//! application delegate, which winit's delegate doesn't handle, so it is added at runtime.
//! When a file is what starts Gumka, the event comes while the app is still launching, before
//! the app is created: the method is added as soon as winit has made its delegate, before the
//! event loop runs ([`install`]), and the file waits until the app takes it ([`take`]).
//! Gumka edits one photo at a time: of the files in one event it opens the first, and a newer
//! event replaces a file that hasn't been opened yet.
//!
//! Elsewhere the file comes as a command-line argument.

#[cfg(target_os = "macos")]
pub use macos::{attach, install, take};

/// Installs the handler. Call on the main thread once the event loop is built, before it runs.
#[cfg(not(target_os = "macos"))]
pub fn install() {}

/// Gives the handler the app's context, to wake the app when a file comes in.
#[cfg(not(target_os = "macos"))]
pub fn attach(_ctx: &eframe::egui::Context) {}

/// The file another app asked Gumka to open, if one is waiting.
#[cfg(not(target_os = "macos"))]
pub fn take() -> Option<std::path::PathBuf> {
    None
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use eframe::egui;
    use objc2::encode::{Encode, Encoding};
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::sel;
    use objc2_foundation::{NSArray, NSURL};

    /// The file waiting to be opened.
    static WAITING: Mutex<Option<PathBuf>> = Mutex::new(None);
    /// Woken when a file comes in.
    static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

    type OpenUrls = extern "C" fn(&AnyObject, Sel, *mut AnyObject, *mut AnyObject);

    /// Installs the handler. Call on the main thread once the event loop is built, before it
    /// runs.
    pub fn install() {
        // void, then self, _cmd, the application and the URLs.
        let types = format!(
            "{}{}{}{}{}",
            Encoding::Void,
            <&AnyObject>::ENCODING,
            Sel::ENCODING,
            <*mut AnyObject>::ENCODING,
            <*mut AnyObject>::ENCODING,
        );
        let method: OpenUrls = open_urls;
        // SAFETY: `Imp` is the runtime's type-erased method pointer. It is only ever called
        // by `objc_msgSend` with the receiver, the selector and the argument and return types
        // that `types` gives, and those match `open_urls`'s signature.
        let imp = unsafe { std::mem::transmute::<OpenUrls, Imp>(method) };
        // SAFETY: `open_urls` has the signature of NSApplicationDelegate's
        // `application:openURLs:`, which `types` encodes.
        let added =
            unsafe { crate::delegate::add_method(sel!(application:openURLs:), imp, &types) };
        if let Err(why) = added {
            eprintln!("{why}; files sent by other apps won't open");
        }
    }

    /// Gives the handler the app's context, to wake the app when a file comes in.
    pub fn attach(ctx: &egui::Context) {
        let _ = CONTEXT.set(ctx.clone());
    }

    /// The file another app asked Gumka to open, if one is waiting.
    pub fn take() -> Option<PathBuf> {
        waiting().take()
    }

    fn waiting() -> MutexGuard<'static, Option<PathBuf>> {
        // Holding only a path, the lock can't be left in a broken state.
        WAITING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `-[NSApplicationDelegate application:openURLs:]`. Called on the main thread.
    extern "C" fn open_urls(
        _this: &AnyObject,
        _cmd: Sel,
        _app: *mut AnyObject,
        urls: *mut AnyObject,
    ) {
        // SAFETY: AppKit passes an `NSArray<NSURL *>`, alive for the call.
        let Some(urls) = (unsafe { urls.cast::<NSArray<NSURL>>().as_ref() }) else {
            return;
        };
        // Only file URLs: `to_file_path` would read a web address's path as a file's.
        let mut files = urls
            .iter()
            .filter(|url| url.isFileURL())
            .filter_map(|url| url.to_file_path());
        let Some(first) = files.next() else {
            return;
        };
        let more = files.count();
        if more > 0 {
            eprintln!(
                "Gumka edits one photo at a time: opening {} and not the {more} other file(s)",
                first.display()
            );
        }
        *waiting() = Some(first);
        if let Some(ctx) = CONTEXT.get() {
            ctx.request_repaint();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use objc2::rc::Retained;

        /// What AppKit hands the delegate becomes the file the app opens next; only the first
        /// of several, and only file URLs.
        #[test]
        fn the_first_file_of_an_open_event_waits_to_be_opened() {
            let url = |path: &str| NSURL::from_file_path(path).unwrap();
            let web = NSURL::URLWithString(&objc2_foundation::NSString::from_str(
                "https://example.org/a.tif",
            ))
            .unwrap();
            let urls: Retained<NSArray<NSURL>> = NSArray::from_retained_slice(&[
                web,
                url("/tmp/gumka-open/a.tif"),
                url("/tmp/gumka-open/b.tif"),
            ]);
            let receiver: &AnyObject = &urls;
            let pointer = Retained::as_ptr(&urls).cast_mut().cast::<AnyObject>();

            open_urls(
                receiver,
                sel!(application:openURLs:),
                std::ptr::null_mut(),
                pointer,
            );
            assert_eq!(take(), Some(PathBuf::from("/tmp/gumka-open/a.tif")));
            assert_eq!(take(), None, "taken once");

            // An event with no files leaves nothing waiting.
            let none: Retained<NSArray<NSURL>> = NSArray::new();
            let pointer = Retained::as_ptr(&none).cast_mut().cast::<AnyObject>();
            open_urls(
                receiver,
                sel!(application:openURLs:),
                std::ptr::null_mut(),
                pointer,
            );
            assert_eq!(take(), None);
        }
    }
}
