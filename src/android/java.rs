// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Calls into the Java side that do not belong to USB: the runtime permission for the microphone.
//! Every call attaches the calling thread for as long as it takes.

use jni::Env;
use jni::errors::Result as JResult;
use jni::objects::{JObject, JObjectArray, JValue};

use crate::android::env::{EnvExt, vm_of};

const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
const PERMISSION_GRANTED: i32 = 0;
const MICROPHONE_REQUEST_CODE: i32 = 1;
/// `WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE`.
const BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE: i32 = 2;

/// The raw pointers `AndroidApp` hands out, valid for the life of the process.
#[derive(Clone, Copy)]
pub struct Java {
    vm: usize,
    activity: usize,
}

impl Java {
    pub fn new(vm: usize, activity: usize) -> Self {
        Self { vm, activity }
    }

    /// The raw pointers, for a caller that attaches threads on its own.
    pub(crate) fn raw(self) -> (usize, usize) {
        (self.vm, self.activity)
    }

    pub(crate) fn call<R>(self, body: impl FnOnce(&mut Env, &JObject) -> JResult<R>) -> Option<R> {
        let out = vm_of(self.vm).attach_current_thread(|env| -> JResult<JResult<R>> {
            // SAFETY: the activity is a global reference of the framework, valid for the process.
            let activity = unsafe { JObject::from_raw(env, self.activity as jni::sys::jobject) };
            let out = env.with_local_frame(16, |env| body(env, &activity));
            if out.is_err() {
                env.exception_clear();
            }
            Ok(out)
        });
        match out {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) | Err(e) => {
                eprintln!("[java] {e}");
                None
            }
        }
    }

    /// Hides the status and navigation bars, which come back for a moment on a swipe from the edge.
    pub fn hide_system_bars(self) {
        self.call(|env, activity| {
            let window = env
                .call(activity, "getWindow", "()Landroid/view/Window;", &[])?
                .l()?;
            let controller = env
                .call(
                    &window,
                    "getInsetsController",
                    "()Landroid/view/WindowInsetsController;",
                    &[],
                )?
                .l()?;
            if controller.is_null() {
                eprintln!("[java] the window has no insets controller yet");
                return Ok(());
            }
            let bars = env
                .call_static("android/view/WindowInsets$Type", "systemBars", "()I", &[])?
                .i()?;
            env.call(&controller, "hide", "(I)V", &[JValue::Int(bars)])?;
            env.call(
                &controller,
                "setSystemBarsBehavior",
                "(I)V",
                &[JValue::Int(BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE)],
            )?;
            Ok(())
        });
    }

    pub fn has_microphone_permission(self) -> bool {
        self.call(|env, activity| {
            let name = env.new_string(RECORD_AUDIO)?;
            env.call(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&name)],
            )?
            .i()
        }) == Some(PERMISSION_GRANTED)
    }

    /// Shows the system's dialog. The answer is not read, the next capture finds the permission.
    pub fn ask_for_microphone(self) {
        self.call(|env, activity| {
            let string = env.class("java/lang/String")?;
            let name = env.new_string(RECORD_AUDIO)?;
            let list: JObjectArray = env.new_object_array(1, string, &name)?;
            env.call(
                activity,
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[JValue::Object(&list), JValue::Int(MICROPHONE_REQUEST_CODE)],
            )
            .map(|_| ())
        });
    }
}
