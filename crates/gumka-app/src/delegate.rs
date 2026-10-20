// SPDX-License-Identifier: GPL-3.0-or-later

//! Adding methods to winit's application delegate (macOS).
//!
//! winit's delegate handles launching and termination only. Gumka adds the
//! `NSApplicationDelegate` methods it needs to the delegate's class at runtime: quitting
//! (`quit.rs`) and files sent by other apps (`open_files.rs`). `class_addMethod` never replaces
//! a method the class already has, so winit's own stay as they are.

use std::ffi::CString;

use objc2::MainThreadMarker;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2_app_kit::NSApplication;

/// Adds `imp` as the method `sel`, of type encoding `types`, to the class of the application's
/// delegate. Says why when it can't.
///
/// # Safety
///
/// `imp` must be a function taking the receiver, the selector and `sel`'s arguments, and
/// returning `sel`'s result, with the types `types` encodes.
pub unsafe fn add_method(sel: Sel, imp: Imp, types: &str) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let delegate = NSApplication::sharedApplication(mtm)
        .delegate()
        .ok_or("no application delegate")?;
    let class: &AnyClass = AsRef::<AnyObject>::as_ref(&*delegate).class();
    let types = CString::new(types).map_err(|_| "a NUL in the type encoding")?;
    // SAFETY: the class is a registered class (the delegate's), and the caller guarantees that
    // `imp` matches `types`. `class_addMethod` adds the method only if the class has none.
    let added = unsafe {
        objc2::ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel,
            imp,
            types.as_ptr(),
        )
    };
    if added.as_bool() {
        Ok(())
    } else {
        Err(format!(
            "the application delegate has its own {}",
            sel.name().to_string_lossy()
        ))
    }
}
