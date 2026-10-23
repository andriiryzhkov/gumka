// SPDX-License-Identifier: GPL-3.0-or-later

//! The inference thread: loads the models once and runs erases, selections and upscales off
//! the UI thread.
//!
//! Core ML models are not `Send`, so they are created on this thread and stay here. The
//! picture's pixels travel to the thread when a model needs them (an erase, or encoding the
//! picture for selection) and come back with the result; while they are away the UI only
//! shows its textures. They come back even when the request fails: a panic while handling it
//! becomes an error, a model that panics while loading is only unavailable, and a request the
//! thread can no longer take is answered without it.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Instant;

use eframe::egui;
use gumka_core::erase::EraseSettings;
use gumka_core::select::{Click, mask_from_scores, model_input, to_input};
use gumka_core::upscale::{Alpha, Factor, Upscaler};
use gumka_core::{Mask, Rect, Rgb16};

pub enum Status {
    Loading,
    Ready {
        model: String,
        runs_on: String,
    },
    /// Found complete and loaded when first used: the upscalers, which most sessions never
    /// need, aren't loaded at startup.
    NotLoaded,
    Unavailable(String),
}

impl Status {
    pub fn is_ready(&self) -> bool {
        matches!(self, Status::Ready { .. })
    }
}

/// Which model a status is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    Inpainter,
    Segmenter,
    /// The upscaler of this scale (2 or 4).
    Upscaler(u32),
}

/// The computer models run on, as the app names it.
pub const THIS_COMPUTER: &str = if cfg!(target_os = "macos") {
    "This Mac"
} else {
    "This computer"
};

/// What runs the models here, before one is loaded.
pub const BACKEND: &str = if cfg!(target_os = "macos") {
    "Core ML"
} else {
    "ONNX Runtime"
};

/// A model found complete, as the app says it: macOS's come with the app, elsewhere they are
/// downloaded.
pub const FOUND: &str = if cfg!(target_os = "macos") {
    "Included"
} else {
    "Downloaded"
};

/// Why the local runtime didn't load, once a model has tried. Only ONNX Runtime can be missing.
#[cfg(target_os = "linux")]
pub fn runtime_problem() -> Option<&'static str> {
    gumka_infer::onnx::problem()
}

#[cfg(not(target_os = "linux"))]
pub fn runtime_problem() -> Option<&'static str> {
    None
}

/// The upscalers there are, by scale.
pub const UPSCALERS: [u32; 2] = [2, 4];

/// Where the `scale`× upscaler is in lists ordered like [`UPSCALERS`]. Scales come from
/// `Factor::model_scale`, which only gives those there are.
pub fn upscaler_slot(scale: u32) -> usize {
    UPSCALERS.iter().position(|&s| s == scale).unwrap_or(0)
}

/// The `scale`× upscaler's name, as the model registry gives it.
pub fn upscaler_name(scale: u32) -> String {
    gumka_infer::models::upscaler_id(scale)
        .and_then(gumka_infer::models::model)
        .map_or_else(|_| format!("{scale}× upscaler"), |model| model.name.clone())
}

pub struct Erased {
    pub rect: Rect,
    /// The pixels `rect` had before.
    pub old: Rgb16,
    pub seconds: f32,
}

/// An upscale's result.
pub struct Upscaled {
    pub pixels: Rgb16,
    pub alpha: Option<Vec<u16>>,
    pub factor: Factor,
    /// The model's name, for the history.
    pub model: String,
    pub seconds: f32,
}

pub enum Reply {
    Status(Which, Status),
    /// Tiles of the running upscale done, of how many.
    Progress {
        done: usize,
        total: usize,
    },
    /// The most clicks one selection can have, once the selection model is loaded.
    MaxClicks(usize),
    /// The pixels always come back, changed or not.
    Erased {
        pixels: Rgb16,
        result: Result<Option<Erased>, String>,
    },
    /// The picture is ready for clicks (or failed); the pixels come back unchanged.
    Encoded {
        pixels: Rgb16,
        generation: u64,
        result: Result<f32, String>,
    },
    /// The picture's pixels always come back, as they were; the larger ones with them, unless
    /// the upscale failed (`Err`) or was cancelled (`Ok(None)`).
    Upscaled {
        pixels: Rgb16,
        result: Result<Option<Upscaled>, String>,
    },
    /// A selection mask for the clicks of request `id`, picture-sized, and the seconds taken.
    Selected {
        id: u64,
        result: Result<(Mask, f32), String>,
    },
    /// The inference thread is gone. A panic doesn't end it (see `serve`), so this is a last
    /// resort: pixels it had are lost.
    Stopped(String),
}

enum Request {
    Erase {
        pixels: Rgb16,
        mask: Mask,
        settings: EraseSettings,
        /// The picture's ICC profile, which decides the model's color space.
        profile: Option<Vec<u8>>,
    },
    Encode {
        pixels: Rgb16,
        generation: u64,
    },
    Select {
        id: u64,
        clicks: Vec<Click>,
        size: (u32, u32),
    },
    Upscale {
        pixels: Rgb16,
        alpha: Option<Vec<u16>>,
        /// The colors are premultiplied by the alpha.
        premultiplied: bool,
        factor: Factor,
        profile: Option<Vec<u8>>,
        /// Set to stop between tiles.
        cancel: Arc<AtomicBool>,
    },
}

impl Request {
    /// The reply to a request that never ran: its pixels go back with `error`.
    fn failed(self, error: &str) -> Reply {
        let error = error.to_owned();
        match self {
            Request::Erase { pixels, .. } => Reply::Erased {
                pixels,
                result: Err(error),
            },
            Request::Encode { pixels, generation } => Reply::Encoded {
                pixels,
                generation,
                result: Err(error),
            },
            Request::Select { id, .. } => Reply::Selected {
                id,
                result: Err(error),
            },
            Request::Upscale { pixels, .. } => Reply::Upscaled {
                pixels,
                result: Err(error),
            },
        }
    }
}

const STOPPED: &str = "the models stopped working";

pub struct Worker {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    /// Replies to requests the thread could not take, handed out before its own.
    failed: Vec<Reply>,
    /// [`Reply::Stopped`] has been handed out.
    stopped: bool,
    ctx: egui::Context,
}

impl Worker {
    pub fn start(ctx: egui::Context) -> Self {
        let (requests, request_rx) = mpsc::channel::<Request>();
        let (reply_tx, replies) = mpsc::channel();
        let thread_ctx = ctx.clone();
        thread::Builder::new()
            .name("gumka-inference".into())
            .spawn(move || serve(request_rx, reply_tx, thread_ctx, run))
            .expect("spawn inference thread");
        Self {
            requests,
            replies,
            failed: Vec::new(),
            stopped: false,
            ctx,
        }
    }

    pub fn erase(
        &mut self,
        pixels: Rgb16,
        mask: Mask,
        settings: EraseSettings,
        profile: Option<Vec<u8>>,
    ) {
        self.send(Request::Erase {
            pixels,
            mask,
            settings,
            profile,
        });
    }

    /// Encodes the picture for selection; `generation` names this version of the pixels.
    pub fn encode(&mut self, pixels: Rgb16, generation: u64) {
        self.send(Request::Encode { pixels, generation });
    }

    /// Selects from clicks in stored picture pixels, on the last encoded picture.
    pub fn select(&mut self, id: u64, clicks: Vec<Click>, size: (u32, u32)) {
        self.send(Request::Select { id, clicks, size });
    }

    /// Upscales the picture by `factor`; `premultiplied` says how `alpha` goes with the colors.
    /// Setting the flag it returns stops the upscale after the tile it is on.
    pub fn upscale(
        &mut self,
        pixels: Rgb16,
        alpha: Option<Vec<u16>>,
        premultiplied: bool,
        factor: Factor,
        profile: Option<Vec<u8>>,
    ) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.send(Request::Upscale {
            pixels,
            alpha,
            premultiplied,
            factor,
            profile,
            cancel: cancel.clone(),
        });
        cancel
    }

    fn send(&mut self, request: Request) {
        if let Err(mpsc::SendError(request)) = self.requests.send(request) {
            self.failed.push(request.failed(STOPPED));
            self.ctx.request_repaint();
        }
    }

    pub fn try_recv(&mut self) -> Option<Reply> {
        if !self.failed.is_empty() {
            return Some(self.failed.remove(0));
        }
        let reply = match self.replies.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) if self.stopped => return None,
            Err(TryRecvError::Disconnected) => Reply::Stopped("it ended unexpectedly".into()),
        };
        self.stopped |= matches!(reply, Reply::Stopped(_));
        Some(reply)
    }
}

/// The thread's body: `run`, and should it panic, every model becomes unavailable and the
/// thread goes on answering requests without running them until the worker is dropped, so
/// every request still gets its pixels back.
fn serve(
    requests: Receiver<Request>,
    replies: Sender<Reply>,
    ctx: egui::Context,
    run: impl FnOnce(&Receiver<Request>, &Sender<Reply>, &egui::Context),
) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| run(&requests, &replies, &ctx)));
    if let Err(payload) = result {
        let reason = format!("{STOPPED}: {}", panic_message(&*payload));
        let upscalers = UPSCALERS.map(Which::Upscaler);
        for which in [Which::Inpainter, Which::Segmenter]
            .into_iter()
            .chain(upscalers)
        {
            let _ = replies.send(Reply::Status(which, Status::Unavailable(reason.clone())));
        }
        ctx.request_repaint();
        for request in &requests {
            let _ = replies.send(request.failed(STOPPED));
            ctx.request_repaint();
        }
    }
}

fn run(requests: &Receiver<Request>, replies: &Sender<Reply>, ctx: &egui::Context) {
    let send = |reply| {
        let _ = replies.send(reply);
        ctx.request_repaint();
    };
    // The upscalers are only looked for: they load when first used.
    for scale in UPSCALERS {
        let status = match guarded(|| backend::find_upscaler(scale)) {
            Ok(()) => Status::NotLoaded,
            Err(reason) => Status::Unavailable(reason),
        };
        send(Reply::Status(Which::Upscaler(scale), status));
    }
    let mut inpainter = load(&send, Which::Inpainter, backend::load_inpainter);
    let mut segmenter = load(&send, Which::Segmenter, backend::load_segmenter);
    if let Some(segmenter) = &segmenter {
        send(Reply::MaxClicks(segmenter.max_clicks()));
    }
    let mut upscalers: [Option<(Box<dyn Upscaler>, String)>; UPSCALERS.len()] =
        std::array::from_fn(|_| None);

    for request in requests {
        match request {
            Request::Erase {
                mut pixels,
                mask,
                settings,
                profile,
            } => {
                let result = guarded(|| match inpainter.as_deref_mut() {
                    Some(inpainter) => {
                        let t = Instant::now();
                        gumka_core::erase::erase(
                            &mut pixels,
                            &mask,
                            inpainter,
                            &settings,
                            profile.as_deref(),
                        )
                        .map(|patch| {
                            patch.map(|patch| Erased {
                                rect: patch.rect,
                                old: patch.pixels,
                                seconds: t.elapsed().as_secs_f32(),
                            })
                        })
                        .map_err(|e| e.to_string())
                    }
                    None => Err("the erase model is not loaded".into()),
                });
                send(Reply::Erased { pixels, result });
            }
            Request::Encode { pixels, generation } => {
                let result = guarded(|| match segmenter.as_deref_mut() {
                    Some(segmenter) => {
                        let t = Instant::now();
                        let input = model_input(&pixels, segmenter.input_size());
                        segmenter
                            .set_image(&input)
                            .map(|()| t.elapsed().as_secs_f32())
                            .map_err(|e| e.to_string())
                    }
                    None => Err("the selection model is not loaded".into()),
                });
                send(Reply::Encoded {
                    pixels,
                    generation,
                    result,
                });
            }
            Request::Select { id, clicks, size } => {
                let result = guarded(|| match segmenter.as_deref_mut() {
                    Some(segmenter) => {
                        let t = Instant::now();
                        let input = to_input(&clicks, size, segmenter.input_size());
                        segmenter
                            .segment(&input)
                            .map(|scores| {
                                (mask_from_scores(&scores, size), t.elapsed().as_secs_f32())
                            })
                            .map_err(|e| e.to_string())
                    }
                    None => Err("the selection model is not loaded".into()),
                });
                send(Reply::Selected { id, result });
            }
            Request::Upscale {
                pixels,
                alpha,
                premultiplied,
                factor,
                profile,
                cancel,
            } => {
                let scale = factor.model_scale();
                let slot = &mut upscalers[upscaler_slot(scale)];
                if slot.is_none() {
                    let which = Which::Upscaler(scale);
                    send(Reply::Status(which, Status::Loading));
                    let name = upscaler_name(scale);
                    *slot = load(&send, which, || backend::load_upscaler(scale))
                        .map(|model| (model, name));
                }
                let result = guarded(|| match slot.as_mut() {
                    Some((upscaler, name)) => {
                        let t = Instant::now();
                        let alpha = alpha.as_deref().map(|values| Alpha {
                            values,
                            premultiplied,
                        });
                        gumka_core::upscale::upscale(
                            &pixels,
                            alpha,
                            factor,
                            upscaler.as_mut(),
                            profile.as_deref(),
                            &mut |done, total| {
                                send(Reply::Progress { done, total });
                                !cancel.load(Ordering::Relaxed)
                            },
                        )
                        .map(|done| {
                            done.map(|up| Upscaled {
                                pixels: up.pixels,
                                alpha: up.alpha,
                                factor,
                                model: name.clone(),
                                seconds: t.elapsed().as_secs_f32(),
                            })
                        })
                        .map_err(|e| e.to_string())
                    }
                    None => Err("the upscale model is unavailable".into()),
                });
                send(Reply::Upscaled { pixels, result });
            }
        }
    }
}

/// Runs `work`, turning a panic into an error: a request still gets its reply, with the pixels
/// it brought, and the models stay in use afterwards.
fn guarded<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(work))
        .unwrap_or_else(|payload| Err(format!("internal error: {}", panic_message(&*payload))))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    match (
        payload.downcast_ref::<&str>(),
        payload.downcast_ref::<String>(),
    ) {
        (Some(message), _) => (*message).to_owned(),
        (_, Some(message)) => message.clone(),
        _ => "unknown panic".into(),
    }
}

type Loaded<T> = Result<(Box<T>, String, String), String>;

/// Loads a model and reports its status. A panic while loading makes it unavailable.
fn load<T: ?Sized>(
    send: &impl Fn(Reply),
    which: Which,
    load: impl FnOnce() -> Loaded<T>,
) -> Option<Box<T>> {
    match guarded(load) {
        Ok((model, name, runs_on)) => {
            send(Reply::Status(
                which,
                Status::Ready {
                    model: name,
                    runs_on,
                },
            ));
            Some(model)
        }
        Err(reason) => {
            send(Reply::Status(which, Status::Unavailable(reason)));
            None
        }
    }
}

/// Whether the `scale`× upscaler's files are all there.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn upscaler_files(scale: u32) -> Result<(), String> {
    gumka_infer::models::model(gumka_infer::models::upscaler_id(scale)?)?
        .find_packages()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
mod backend {
    use gumka_core::erase::Inpainter;
    use gumka_core::select::Segmenter;
    use gumka_core::upscale::Upscaler;
    use gumka_infer::coreml::ComputeUnits;
    use gumka_infer::{CoreMlLama, CoreMlSam, CoreMlUpscaler, models};

    use super::Loaded;

    pub fn load_inpainter() -> Loaded<dyn Inpainter> {
        let package = models::model(models::LAMA)?.find_packages()?.remove(0);
        let lama = CoreMlLama::load(&package, &models::cache_dir(), ComputeUnits::CpuAndGpu)
            .map_err(|e| e.to_string())?;
        Ok((Box::new(lama), "LaMa".into(), "Core ML · GPU".into()))
    }

    pub fn load_segmenter() -> Loaded<dyn Segmenter> {
        let sam = CoreMlSam::load(&models::cache_dir(), ComputeUnits::CpuAndGpu)
            .map_err(|e| e.to_string())?;
        Ok((
            Box::new(sam),
            "SAM 2.1 Small".into(),
            "Core ML · GPU".into(),
        ))
    }

    /// Whether the `scale`× upscaler's files are all there, without loading it.
    pub fn find_upscaler(scale: u32) -> Result<(), String> {
        super::upscaler_files(scale)
    }

    pub fn load_upscaler(scale: u32) -> Loaded<dyn Upscaler> {
        let upscaler = CoreMlUpscaler::load(scale, &models::cache_dir(), ComputeUnits::CpuAndGpu)
            .map_err(|e| e.to_string())?;
        Ok((
            Box::new(upscaler),
            super::upscaler_name(scale),
            "Core ML · GPU".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use gumka_core::erase::Inpainter;
    use gumka_core::select::Segmenter;
    use gumka_core::upscale::Upscaler;
    use gumka_infer::models;
    use gumka_infer::onnx::{OnnxLama, OnnxSam, OnnxUpscaler, RUNS_ON};

    use super::Loaded;

    pub fn load_inpainter() -> Loaded<dyn Inpainter> {
        let package = models::model(models::LAMA)?.find_packages()?.remove(0);
        let lama = OnnxLama::load(&package).map_err(|e| e.to_string())?;
        Ok((Box::new(lama), "LaMa".into(), RUNS_ON.into()))
    }

    pub fn load_segmenter() -> Loaded<dyn Segmenter> {
        let sam = OnnxSam::load().map_err(|e| e.to_string())?;
        Ok((Box::new(sam), "SAM 2.1 Small".into(), RUNS_ON.into()))
    }

    /// Whether the `scale`× upscaler's files are all there and ONNX Runtime loads, without
    /// loading the model: an upscale that can't run isn't offered.
    pub fn find_upscaler(scale: u32) -> Result<(), String> {
        super::upscaler_files(scale)?;
        gumka_infer::onnx::runtime().map(|_| ())
    }

    pub fn load_upscaler(scale: u32) -> Loaded<dyn Upscaler> {
        let upscaler = OnnxUpscaler::load(scale).map_err(|e| e.to_string())?;
        Ok((
            Box::new(upscaler),
            super::upscaler_name(scale),
            RUNS_ON.into(),
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod backend {
    use gumka_core::erase::Inpainter;
    use gumka_core::select::Segmenter;
    use gumka_core::upscale::Upscaler;

    use super::Loaded;

    const NONE: &str = "no inference backend on this platform yet";

    pub fn load_inpainter() -> Loaded<dyn Inpainter> {
        Err(NONE.into())
    }

    pub fn load_segmenter() -> Loaded<dyn Segmenter> {
        Err(NONE.into())
    }

    pub fn find_upscaler(_: u32) -> Result<(), String> {
        Err(NONE.into())
    }

    pub fn load_upscaler(_: u32) -> Loaded<dyn Upscaler> {
        Err(NONE.into())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use gumka_core::erase::Inpainter;

    use super::*;

    /// A worker with no thread behind it: tests of the app play the thread's part, answering
    /// requests and sending replies when they choose.
    pub(crate) struct Fake {
        requests: Receiver<Request>,
        replies: Sender<Reply>,
    }

    pub(crate) fn fake() -> (Worker, Fake) {
        let (requests, request_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        let worker = Worker {
            requests,
            replies,
            failed: Vec::new(),
            stopped: false,
            ctx: egui::Context::default(),
        };
        let fake = Fake {
            requests: request_rx,
            replies: reply_tx,
        };
        (worker, fake)
    }

    impl Fake {
        pub(crate) fn reply(&self, reply: Reply) {
            self.replies.send(reply).expect("the app has the worker");
        }

        /// Nothing has been asked since the last request was taken.
        pub(crate) fn idle(&self) -> bool {
            self.requests.try_recv().is_err()
        }

        fn next(&self, expected: &str) -> Request {
            match self.requests.try_recv() {
                Ok(request) => request,
                Err(_) => panic!("expected a request to {expected}, got none"),
            }
        }

        /// Takes an encoding request and answers that it worked. Returns its generation.
        pub(crate) fn encode(&self) -> u64 {
            let Request::Encode { pixels, generation } = self.next("encode") else {
                panic!("expected a request to encode");
            };
            self.reply(Reply::Encoded {
                pixels,
                generation,
                result: Ok(0.1),
            });
            generation
        }

        /// Takes a selection request: its id, clicks and picture size.
        pub(crate) fn select(&self) -> (u64, Vec<Click>, (u32, u32)) {
            let Request::Select { id, clicks, size } = self.next("select") else {
                panic!("expected a request to select");
            };
            (id, clicks, size)
        }

        /// Takes an erase request: the pixels and the mask.
        pub(crate) fn erase(&self) -> (Rgb16, Mask) {
            let Request::Erase { pixels, mask, .. } = self.next("erase") else {
                panic!("expected a request to erase");
            };
            (pixels, mask)
        }

        /// Takes an upscale request.
        pub(crate) fn upscale(&self) -> Asked {
            let Request::Upscale {
                pixels,
                alpha,
                premultiplied,
                factor,
                cancel,
                ..
            } = self.next("upscale")
            else {
                panic!("expected a request to upscale");
            };
            Asked {
                pixels,
                alpha,
                premultiplied,
                factor,
                cancel,
            }
        }
    }

    /// What an upscale request brought.
    pub(crate) struct Asked {
        pub pixels: Rgb16,
        pub alpha: Option<Vec<u16>>,
        pub premultiplied: bool,
        pub factor: Factor,
        pub cancel: Arc<AtomicBool>,
    }

    #[test]
    fn a_panic_becomes_an_error_and_the_pixels_stay() {
        let mut pixels = Rgb16::new(4, 3);
        let result: Result<(), String> = guarded(|| {
            pixels.put_pixel(1, 1, gumka_core::image::Rgb([7, 7, 7]));
            panic!("boom {}", 42)
        });
        assert_eq!(result, Err("internal error: boom 42".into()));
        assert_eq!(pixels.dimensions(), (4, 3));
        assert_eq!(pixels.get_pixel(1, 1).0, [7, 7, 7]);
    }

    #[test]
    fn a_stopped_thread_hands_the_pixels_back_and_says_so_once() {
        let (requests, request_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        drop((request_rx, reply_tx));
        let mut worker = Worker {
            requests,
            replies,
            failed: Vec::new(),
            stopped: false,
            ctx: egui::Context::default(),
        };
        worker.erase(
            Rgb16::new(3, 2),
            Mask::new(3, 2),
            EraseSettings::default(),
            None,
        );
        worker.encode(Rgb16::new(5, 4), 9);
        match worker.try_recv() {
            Some(Reply::Erased { pixels, result }) => {
                assert_eq!(pixels.dimensions(), (3, 2));
                assert_eq!(result.err().as_deref(), Some(STOPPED));
            }
            _ => panic!("expected the erase back"),
        }
        match worker.try_recv() {
            Some(Reply::Encoded {
                pixels,
                generation,
                result,
            }) => {
                assert_eq!((pixels.dimensions(), generation), ((5, 4), 9));
                assert!(result.is_err());
            }
            _ => panic!("expected the encoding back"),
        }
        assert!(matches!(worker.try_recv(), Some(Reply::Stopped(_))));
        assert!(worker.try_recv().is_none());
    }

    #[test]
    fn a_select_the_thread_cant_take_fails_with_its_id() {
        let (mut worker, fake) = fake();
        drop(fake);
        let click = Click {
            x: 1.0,
            y: 1.0,
            include: true,
        };
        worker.select(7, vec![click], (4, 4));
        match worker.try_recv() {
            Some(Reply::Selected { id, result }) => {
                assert_eq!(id, 7);
                assert_eq!(result.err().as_deref(), Some(STOPPED));
            }
            _ => panic!("expected the selection back"),
        }
    }

    /// The thread ending without a word (its sender dropped) is reported once, after the
    /// replies it sent before.
    #[test]
    fn replies_sent_before_the_thread_ended_still_arrive() {
        let (mut worker, fake) = fake();
        fake.reply(Reply::MaxClicks(14));
        drop(fake);
        assert!(matches!(worker.try_recv(), Some(Reply::MaxClicks(14))));
        match worker.try_recv() {
            Some(Reply::Stopped(reason)) => assert_eq!(reason, "it ended unexpectedly"),
            _ => panic!("expected the thread to be reported stopped"),
        }
        assert!(worker.try_recv().is_none());
    }

    #[test]
    fn a_model_that_panics_while_loading_is_only_unavailable() {
        fn panics() -> Loaded<dyn Inpainter> {
            panic!("no model here")
        }
        let (reply_tx, replies) = mpsc::channel();
        let send = |reply| reply_tx.send(reply).unwrap();
        assert!(load(&send, Which::Inpainter, panics).is_none());
        match replies.try_recv() {
            Ok(Reply::Status(Which::Inpainter, Status::Unavailable(reason))) => {
                assert!(reason.contains("no model here"), "{reason}");
            }
            _ => panic!("expected the model to be unavailable"),
        }
    }

    #[test]
    fn a_thread_that_panics_keeps_handing_the_pixels_back() {
        let (requests, request_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        requests
            .send(Request::Erase {
                pixels: Rgb16::new(3, 2),
                mask: Mask::new(3, 2),
                settings: EraseSettings::default(),
                profile: None,
            })
            .unwrap();
        let thread = thread::spawn(move || {
            serve(request_rx, reply_tx, egui::Context::default(), |_, _, _| {
                panic!("load failed")
            })
        });
        let next = || {
            replies
                .recv_timeout(std::time::Duration::from_secs(10))
                .ok()
        };
        let upscalers = UPSCALERS.map(Which::Upscaler);
        for expected in [Which::Inpainter, Which::Segmenter]
            .into_iter()
            .chain(upscalers)
        {
            match next() {
                Some(Reply::Status(which, Status::Unavailable(reason))) => {
                    assert!(
                        which == expected && reason.contains("load failed"),
                        "{reason}"
                    );
                }
                _ => panic!("expected the models to be unavailable"),
            }
        }
        match next() {
            Some(Reply::Erased { pixels, result }) => {
                assert_eq!(pixels.dimensions(), (3, 2));
                assert_eq!(result.err().as_deref(), Some(STOPPED));
            }
            _ => panic!("expected the queued erase back"),
        }
        // Requests sent after the panic are answered too, by the thread.
        requests
            .send(Request::Encode {
                pixels: Rgb16::new(2, 2),
                generation: 1,
            })
            .unwrap();
        assert!(matches!(
            next(),
            Some(Reply::Encoded { result: Err(_), .. })
        ));
        drop(requests);
        thread.join().unwrap();
    }

    #[test]
    fn an_upscale_the_thread_cant_take_hands_the_pixels_back() {
        let (mut worker, fake) = fake();
        drop(fake);
        let cancel = worker.upscale(Rgb16::new(6, 4), Some(vec![0; 24]), false, Factor::X2, None);
        assert!(!cancel.load(Ordering::Relaxed));
        match worker.try_recv() {
            Some(Reply::Upscaled { pixels, result }) => {
                assert_eq!(pixels.dimensions(), (6, 4));
                assert_eq!(result.err().as_deref(), Some(STOPPED));
            }
            _ => panic!("expected the upscale back"),
        }
    }

    #[test]
    fn an_upscaler_that_doesnt_load_is_unavailable() {
        let (reply_tx, replies) = mpsc::channel();
        let send = |reply| reply_tx.send(reply).unwrap();
        let model = load::<dyn Upscaler>(&send, Which::Upscaler(4), || {
            Err("RealPLKSR 4× is not downloaded".into())
        });
        assert!(model.is_none());
        match replies.try_recv() {
            Ok(Reply::Status(Which::Upscaler(4), Status::Unavailable(reason))) => {
                assert!(reason.contains("not downloaded"), "{reason}");
            }
            _ => panic!("expected the 4× upscaler to be unavailable"),
        }
    }

    #[test]
    fn a_panic_without_a_message_still_becomes_an_error() {
        let result: Result<(), String> = guarded(|| std::panic::panic_any(5));
        assert_eq!(result, Err("internal error: unknown panic".into()));
    }
}
