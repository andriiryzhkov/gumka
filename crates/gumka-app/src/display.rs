// SPDX-License-Identifier: GPL-3.0-or-later

//! The window's color space.
//!
//! The canvas converts the photo from its own profile into [`SCREEN`]. On macOS the window's
//! Metal layer is tagged Display P3 to match, and the system color-matches it to whatever
//! display the window is on. Untagged, the layer's values would go to the display as they are,
//! as if every photo were in that display's color space.
//!
//! egui's renderer clears the tag whenever it reconfigures the surface, which it does on every
//! resize, so a key-value observer on the layer puts it back right away.
//!
//! The UI colors are the design's sRGB values used as they are, so on macOS they are shown
//! as Display P3: the same as they looked before the window was tagged.

use gumka_core::color::Screen;

#[cfg(target_os = "macos")]
pub const SCREEN: Screen = Screen::DisplayP3;
#[cfg(not(target_os = "macos"))]
pub const SCREEN: Screen = Screen::Srgb;

#[cfg(target_os = "macos")]
pub use macos::ColorSpaceTag;

/// Elsewhere the window is sRGB already.
#[cfg(not(target_os = "macos"))]
pub struct ColorSpaceTag;

#[cfg(not(target_os = "macos"))]
impl ColorSpaceTag {
    pub fn new() -> Self {
        Self
    }

    pub fn keep(&mut self, _frame: &eframe::Frame) {}
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::ptr;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AllocAnyThread, define_class, msg_send};
    use objc2_core_foundation::CFType;
    use objc2_core_graphics::{CGColorSpace, kCGColorSpaceDisplayP3};
    use objc2_foundation::{
        NSDictionary, NSKeyValueChangeKey, NSKeyValueObservingOptions,
        NSObjectNSKeyValueObserverRegistration, NSString, ns_string,
    };
    use objc2_quartz_core::{CALayer, CAMetalLayer};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    fn key() -> &'static NSString {
        ns_string!("colorspace")
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `Observer` implements no
        // `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "GumkaColorSpaceObserver"]
        struct Observer;

        impl Observer {
            // SAFETY: the signature is NSKeyValueObserving's.
            #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
            fn observe(
                &self,
                _key_path: Option<&NSString>,
                object: Option<&AnyObject>,
                _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
                _context: *mut c_void,
            ) {
                if let Some(layer) = object.and_then(|o| o.downcast_ref::<CAMetalLayer>()) {
                    tag(layer);
                }
            }
        }
    );

    impl Observer {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: NSObject's designated initializer.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Keeps the window's Metal layer tagged Display P3.
    pub struct ColorSpaceTag {
        /// The layer being watched, and its observer; both kept alive until the observer is
        /// removed.
        watching: Option<(Retained<CAMetalLayer>, Retained<Observer>)>,
    }

    impl ColorSpaceTag {
        pub fn new() -> Self {
            Self { watching: None }
        }

        /// Tags the window's layer and starts watching it, unless it is watched already. Cheap
        /// enough to call every frame; that also picks up a new layer if the renderer replaces
        /// its surface.
        pub fn keep(&mut self, window: &impl HasWindowHandle) {
            let Some(layer) = metal_layer(window) else {
                return;
            };
            if self
                .watching
                .as_ref()
                .is_some_and(|(watched, _)| Retained::as_ptr(watched) == Retained::as_ptr(&layer))
            {
                return;
            }
            self.stop();
            tag(&layer);
            let observer = Observer::new();
            // SAFETY: `Observer` implements the observing method, and the layer and the
            // observer are both retained until `stop` removes the observation.
            unsafe {
                layer.addObserver_forKeyPath_options_context(
                    &observer,
                    key(),
                    NSKeyValueObservingOptions::empty(),
                    ptr::null_mut(),
                );
            }
            self.watching = Some((layer, observer));
        }

        fn stop(&mut self) {
            if let Some((layer, observer)) = self.watching.take() {
                // SAFETY: this observer was added for this key path in `keep`.
                unsafe { layer.removeObserver_forKeyPath(&observer, key()) };
            }
        }
    }

    impl Drop for ColorSpaceTag {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Sets the layer's color space to Display P3, if it isn't already (setting it again would
    /// notify the observer again).
    fn tag(layer: &CAMetalLayer) {
        // SAFETY: a constant defined by Core Graphics.
        let p3: &CFType = unsafe { kCGColorSpaceDisplayP3 };
        let tagged = layer
            .colorspace()
            .and_then(|space| CGColorSpace::name(Some(&space)))
            .is_some_and(|name| {
                let name: &CFType = &name;
                name == p3
            });
        if !tagged {
            // SAFETY: as above.
            let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 }));
            layer.setColorspace(space.as_deref());
        }
    }

    /// The Metal layer wgpu draws the window into: the view's own layer, or its newest Metal
    /// sublayer. A surface the renderer recreates comes with a new sublayer, and the old one
    /// stays behind.
    fn metal_layer(window: &impl HasWindowHandle) -> Option<Retained<CAMetalLayer>> {
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: an AppKit window handle points to the window's NSView, which lives as long
        // as the window.
        let view: &AnyObject = unsafe { handle.ns_view.cast().as_ref() };
        // SAFETY: NSView's `layer` returns a CALayer or nil.
        let layer: Option<Retained<CALayer>> = unsafe { msg_send![view, layer] };
        let layer = match layer?.downcast::<CAMetalLayer>() {
            Ok(metal) => return Some(metal),
            Err(layer) => layer,
        };
        // SAFETY: reading the layer tree on the main thread, where the app runs.
        let sublayers = unsafe { layer.sublayers() }?;
        sublayers
            .iter()
            .filter_map(|sublayer| sublayer.downcast::<CAMetalLayer>().ok())
            .last()
    }
}
