// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! The video layer behind the window: an `ASurfaceControl` child of the activity's window at a
//! negative z-order. The decoder's buffers go to it untouched and the display hardware converts
//! and scales them, under the window's transparent hole where the UI says the picture is. Not
//! every system lets a window have such a child, then `ensure` says no and `surface_layer` makes
//! the layers through Java instead.

use std::ffi::{c_char, c_int, c_void};
use std::sync::{Mutex, OnceLock};

use ndk::hardware_buffer::{
    HardwareBuffer, HardwareBufferDesc, HardwareBufferRef, HardwareBufferUsage,
};
use ndk::hardware_buffer_format::HardwareBufferFormat;
use slint::android::AndroidApp;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[link(name = "android")]
unsafe extern "C" {
    fn ASurfaceControl_createFromWindow(parent: *mut c_void, name: *const c_char) -> *mut c_void;
    fn ASurfaceControl_release(control: *mut c_void);
    fn ASurfaceTransaction_create() -> *mut c_void;
    fn ASurfaceTransaction_delete(txn: *mut c_void);
    fn ASurfaceTransaction_apply(txn: *mut c_void);
    fn ASurfaceTransaction_setBuffer(
        txn: *mut c_void,
        control: *mut c_void,
        buffer: *mut c_void,
        fence: c_int,
    );
    fn ASurfaceTransaction_setGeometry(
        txn: *mut c_void,
        control: *mut c_void,
        src: *const ARect,
        dst: *const ARect,
        transform: i32,
    );
    fn ASurfaceTransaction_setVisibility(txn: *mut c_void, control: *mut c_void, visible: i8);
    fn ASurfaceTransaction_setZOrder(txn: *mut c_void, control: *mut c_void, z: i32);
    fn ASurfaceTransaction_setBufferTransparency(txn: *mut c_void, control: *mut c_void, t: i8);
}

const HIDE: i8 = 0;
const SHOW: i8 = 1;
const OPAQUE: i8 = 2;

struct Layer {
    parent: *mut c_void,
    control: *mut c_void,
    // Black behind everything, so a window with nothing in it is not see-through.
    backdrop: *mut c_void,
    _black: HardwareBufferRef,
}

// SAFETY: the handles are ids of compositor objects, any thread may use them.
unsafe impl Send for Layer {}

impl Drop for Layer {
    fn drop(&mut self) {
        // SAFETY: both came from `ASurfaceControl_createFromWindow` and are released once.
        unsafe {
            ASurfaceControl_release(self.control);
            ASurfaceControl_release(self.backdrop);
        }
    }
}

#[derive(Default)]
struct State {
    layer: Option<Layer>,
    window: (i32, i32),
    src: Option<ARect>,
    refused: bool,
}

static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();
static LAYER_STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    let mut guard = LAYER_STATE.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(State::default);
    guard
}

/// The app, once `install` has run.
pub(crate) fn app() -> Option<&'static AndroidApp> {
    ANDROID_APP.get()
}

pub fn install(app: AndroidApp) {
    let _ = ANDROID_APP.set(app);
    // The backdrop is wanted from the start, the window has no surface yet and gets a new one
    // whenever the app comes back, so look for it from time to time.
    let _ = std::thread::Builder::new()
        .name("livi-layer".into())
        .spawn(|| {
            while !state().as_ref().is_some_and(|s| s.refused) {
                ensure();
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
}

/// Where a picture goes in a window: as large as fits with its shape kept, in the middle. While a
/// session streams the UI hides its navigation and the projection page fills the window, so this
/// is where the UI puts the picture too.
pub(crate) fn fit(src: ARect, (width, height): (i32, i32)) -> ARect {
    let (source_width, source_height) =
        ((src.right - src.left).max(1), (src.bottom - src.top).max(1));
    let scale = (f64::from(width) / f64::from(source_width))
        .min(f64::from(height) / f64::from(source_height));
    let (fitted_width, fitted_height) = (
        (f64::from(source_width) * scale).round() as i32,
        (f64::from(source_height) * scale).round() as i32,
    );
    let (left, top) = ((width - fitted_width) / 2, (height - fitted_height) / 2);
    ARect {
        left,
        top,
        right: left + fitted_width,
        bottom: top + fitted_height,
    }
}

fn commit(layer: &Layer, buffer: Option<*mut c_void>, src: Option<ARect>, window: (i32, i32)) {
    // SAFETY: the transaction is made, filled from live handles, applied and deleted here.
    unsafe {
        let txn = ASurfaceTransaction_create();
        ASurfaceTransaction_setZOrder(txn, layer.control, -1);
        if let Some(buffer) = buffer {
            ASurfaceTransaction_setBuffer(txn, layer.control, buffer, -1);
            ASurfaceTransaction_setBufferTransparency(txn, layer.control, OPAQUE);
        }
        if let Some(src) = src {
            let dst = fit(src, window);
            ASurfaceTransaction_setGeometry(txn, layer.control, &src, &dst, 0);
        }
        let visible = if src.is_some() { SHOW } else { HIDE };
        ASurfaceTransaction_setVisibility(txn, layer.control, visible);
        ASurfaceTransaction_apply(txn);
        ASurfaceTransaction_delete(txn);
    }
}

/// A one pixel black picture the compositor stretches over the window.
fn black() -> Result<HardwareBufferRef, String> {
    let buffer = HardwareBuffer::allocate(HardwareBufferDesc {
        width: 1,
        height: 1,
        layers: 1,
        format: HardwareBufferFormat::R8G8B8A8_UNORM,
        usage: HardwareBufferUsage::CPU_WRITE_OFTEN
            | HardwareBufferUsage::GPU_SAMPLED_IMAGE
            | HardwareBufferUsage::COMPOSER_OVERLAY,
        stride: 0,
    })
    .map_err(|e| format!("buffer: {e}"))?;
    let pixels = buffer
        .lock(HardwareBufferUsage::CPU_WRITE_OFTEN, None, None)
        .map_err(|e| format!("lock: {e}"))?;
    // SAFETY: the buffer is locked for writing and holds at least one pixel.
    unsafe {
        pixels
            .cast::<u8>()
            .copy_from_nonoverlapping([0u8, 0, 0, 255].as_ptr(), 4)
    };
    buffer.unlock().map_err(|e| format!("unlock: {e}"))?;
    Ok(buffer)
}

/// Shows the black picture over the whole window, as far back as layers go.
fn fill_backdrop(control: *mut c_void, buffer: &HardwareBufferRef) {
    let src = ARect {
        left: 0,
        top: 0,
        right: 1,
        bottom: 1,
    };
    // More than any screen, the window clips it.
    let dst = ARect {
        left: 0,
        top: 0,
        right: 16384,
        bottom: 16384,
    };
    // SAFETY: the transaction is made, filled from live handles, applied and deleted here.
    unsafe {
        let txn = ASurfaceTransaction_create();
        ASurfaceTransaction_setZOrder(txn, control, -2);
        ASurfaceTransaction_setBuffer(txn, control, buffer.as_ptr().cast(), -1);
        ASurfaceTransaction_setBufferTransparency(txn, control, OPAQUE);
        ASurfaceTransaction_setGeometry(txn, control, &src, &dst, 0);
        ASurfaceTransaction_setVisibility(txn, control, SHOW);
        ASurfaceTransaction_apply(txn);
        ASurfaceTransaction_delete(txn);
    }
}

/// Whether there is a layer to show pictures in, making it when the window allows one.
pub fn ensure() -> bool {
    let Some(app) = ANDROID_APP.get() else {
        return false;
    };
    let Some(window) = app.native_window() else {
        return false;
    };
    let parent: *mut c_void = window.ptr().as_ptr().cast();
    let mut guard = state();
    let state = guard.as_mut().expect("state is made above");
    if state.refused {
        return false;
    }
    let size = (window.width(), window.height());
    if state
        .layer
        .as_ref()
        .is_some_and(|layer| layer.parent == parent)
    {
        // The window turns when the device does.
        if state.window != size {
            state.window = size;
            if let Some(layer) = &state.layer {
                commit(layer, None, state.src, state.window);
            }
        }
        return true;
    }
    state.window = size;
    // SAFETY: `parent` is the activity's live window, the name is a C string.
    let control = unsafe { ASurfaceControl_createFromWindow(parent, c"livi-video".as_ptr()) };
    if control.is_null() {
        println!("[layer] the window cannot have a video layer");
        state.refused = true;
        return false;
    }
    // SAFETY: as above.
    let backdrop = unsafe { ASurfaceControl_createFromWindow(parent, c"livi-backdrop".as_ptr()) };
    let black = black();
    let (Some(black), false) = (black.ok(), backdrop.is_null()) else {
        println!("[layer] no backdrop");
        state.refused = true;
        return false;
    };
    fill_backdrop(backdrop, &black);
    println!("[layer] video layer behind the window");
    state.layer = Some(Layer {
        parent,
        control,
        backdrop,
        _black: black,
    });
    true
}

/// Shows a decoded buffer, `crop` being the part of it that is the picture.
pub fn present(buffer: *mut c_void, crop: ARect) {
    let mut guard = state();
    let state = guard.as_mut().expect("state is made above");
    state.src = Some(crop);
    if let Some(layer) = &state.layer {
        commit(layer, Some(buffer), state.src, state.window);
    }
}

/// Takes the picture down, the layer stays for the next one.
pub fn clear() {
    let mut guard = state();
    let state = guard.as_mut().expect("state is made above");
    state.src = None;
    if let Some(layer) = &state.layer {
        commit(layer, None, None, state.window);
    }
}
