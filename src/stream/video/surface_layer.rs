// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! A video layer made through the Java `SurfaceControl` API, for systems where
//! `ASurfaceControl_createFromWindow` refuses the activity's window (Android 13, whose window
//! surface carries no SurfaceControl handle). The root control comes from the decor view's
//! `getRootSurfaceControl`, the child layers are reparented under it, and the decoder renders into
//! the `Surface` of the video one. No window of our own and no change to the UI.

use std::ffi::c_void;
use std::sync::Mutex;

use jni::Env;
use jni::errors::Error as JError;
use jni::objects::{JObject, JValue};
use ndk::native_window::NativeWindow;

use crate::android::env::{EnvExt, Ref, with_env};
use crate::stream::video::layer::{self, ARect};

const CONTROL: &str = "android/view/SurfaceControl";
const BUILDER: &str = "android/view/SurfaceControl$Builder";
const TRANSACTION_SIG: &str = "Landroid/view/SurfaceControl$Transaction;";
const FORMAT_OPAQUE: i32 = -1;

unsafe extern "C" {
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut c_void;
}

struct Child {
    vm: usize,
    control: Ref,
    surface: Ref,
    native: NativeWindow,
}

impl Child {
    fn new(env: &mut Env, vm: usize, name: &str, size: (i32, i32)) -> Result<Self, JError> {
        let builder = env.construct(BUILDER, "()V", &[])?;
        let name = env.new_string(name)?;
        let set_name = format!("(Ljava/lang/String;)L{BUILDER};");
        env.call(&builder, "setName", &set_name, &[JValue::Object(&name)])?;
        let sized = format!("(II)L{BUILDER};");
        env.call(
            &builder,
            "setBufferSize",
            &sized,
            &[JValue::Int(size.0), JValue::Int(size.1)],
        )?;
        let formatted = format!("(I)L{BUILDER};");
        env.call(
            &builder,
            "setFormat",
            &formatted,
            &[JValue::Int(FORMAT_OPAQUE)],
        )?;
        let built = format!("()L{CONTROL};");
        let control = env.call(&builder, "build", &built, &[])?.l()?;
        let surface = env.construct(
            "android/view/Surface",
            format!("(L{CONTROL};)V"),
            &[JValue::Object(&control)],
        )?;
        // SAFETY: both are live local references, the call returns its own reference to the window.
        let native = unsafe {
            let raw = ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast());
            NativeWindow::from_ptr(
                std::ptr::NonNull::new(raw.cast()).ok_or(JError::NullPtr("native window"))?,
            )
        };
        Ok(Self {
            vm,
            control: env.global(&control)?,
            surface: env.global(&surface)?,
            native,
        })
    }

    /// Puts the layer under the root control at `z`, `rect`'s corner, shown.
    fn place(&self, env: &mut Env, root: &JObject, z: i32, rect: ARect) -> Result<(), JError> {
        let sig = format!("(L{CONTROL};){TRANSACTION_SIG}");
        let transaction = env
            .call(
                root,
                "buildReparentTransaction",
                &sig,
                &[JValue::Object(&self.control)],
            )?
            .l()?;
        if transaction.is_null() {
            return Err(JError::NullPtr("the root control gave no transaction"));
        }
        let layered = format!("(L{CONTROL};I){TRANSACTION_SIG}");
        env.call(
            &transaction,
            "setLayer",
            &layered,
            &[JValue::Object(&self.control), JValue::Int(z)],
        )?;
        let positioned = format!("(L{CONTROL};FF){TRANSACTION_SIG}");
        env.call(
            &transaction,
            "setPosition",
            &positioned,
            &[
                JValue::Object(&self.control),
                JValue::Float(rect.left as f32),
                JValue::Float(rect.top as f32),
            ],
        )?;
        let shown = format!("(L{CONTROL};){TRANSACTION_SIG}");
        env.call(
            &transaction,
            "show",
            &shown,
            &[JValue::Object(&self.control)],
        )?;
        env.call(&transaction, "apply", "()V", &[])?;
        env.call(&transaction, "close", "()V", &[])?;
        Ok(())
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = with_env(self.vm, |env| {
            env.call(&self.surface, "release", "()V", &[])?;
            env.call(&self.control, "release", "()V", &[])?;
            Ok(())
        });
    }
}

struct Layer {
    video: Child,
    // Black behind the picture, so the bars next to it are not see-through.
    _backdrop: Child,
    parent: usize,
    window: (i32, i32),
    source: (i32, i32),
}

static LAYER: Mutex<Option<Layer>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Layer>> {
    LAYER.lock().unwrap_or_else(|e| e.into_inner())
}

fn fill_black(window: &NativeWindow) -> Result<(), String> {
    let mut guard = window.lock(None).map_err(|e| format!("lock: {e}"))?;
    let width = guard.stride().max(guard.width()) * 4;
    let Some(lines) = guard.lines() else {
        return Err("no lines".to_owned());
    };
    for line in lines {
        for (i, byte) in line.iter_mut().enumerate().take(width) {
            byte.write(if i % 4 == 3 { 255 } else { 0 });
        }
    }
    Ok(())
}

fn make(
    vm: usize,
    activity: usize,
    parent: usize,
    source: (i32, i32),
    window: (i32, i32),
) -> Result<Layer, String> {
    let fitted = layer::fit(
        ARect {
            left: 0,
            top: 0,
            right: source.0,
            bottom: source.1,
        },
        window,
    );
    with_env(vm, |env| {
        // SAFETY: the activity is a global reference of the framework, valid for the process.
        let activity = unsafe { JObject::from_raw(env, activity as jni::sys::jobject) };
        let win = env
            .call(&activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let decor = env
            .call(&win, "getDecorView", "()Landroid/view/View;", &[])?
            .l()?;
        let root = env
            .call(
                &decor,
                "getRootSurfaceControl",
                "()Landroid/view/AttachedSurfaceControl;",
                &[],
            )?
            .l()?;
        if root.is_null() {
            return Err(JError::NullPtr("the window has no root control"));
        }
        let backdrop = Child::new(env, vm, "livi-backdrop", window)?;
        let video = Child::new(
            env,
            vm,
            "livi-video",
            (fitted.right - fitted.left, fitted.bottom - fitted.top),
        )?;
        backdrop.place(
            env,
            &root,
            -2,
            ARect {
                left: 0,
                top: 0,
                right: window.0,
                bottom: window.1,
            },
        )?;
        video.place(env, &root, -1, fitted)?;
        Ok(Layer {
            video,
            _backdrop: backdrop,
            parent,
            window,
            source,
        })
    })
    .and_then(|layer| {
        fill_black(&layer._backdrop.native)?;
        Ok(layer)
    })
}

/// The window the decoder renders into, in a layer fitted to a `source` sized picture, made when
/// the window allows. `None` when it does not.
pub(crate) fn ensure(source: (i32, i32)) -> Option<NativeWindow> {
    let app = layer::app()?;
    let native = app.native_window()?;
    let parent = native.ptr().as_ptr() as usize;
    let window = (native.width(), native.height());
    let mut guard = lock();
    if let Some(layer) = guard
        .as_ref()
        .filter(|l| l.parent == parent && l.window == window && l.source == source)
    {
        return Some(layer.video.native.clone());
    }
    *guard = None;
    match make(
        app.vm_as_ptr() as usize,
        app.activity_as_ptr() as usize,
        parent,
        source,
        window,
    ) {
        Ok(layer) => {
            println!(
                "[surface-layer] video layer behind the window, {}x{} in {}x{}",
                source.0, source.1, window.0, window.1
            );
            let native = layer.video.native.clone();
            *guard = Some(layer);
            Some(native)
        }
        Err(e) => {
            println!("[surface-layer] {e}");
            None
        }
    }
}

pub(crate) fn clear() {
    *lock() = None;
}
