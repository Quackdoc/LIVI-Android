// SPDX-FileCopyrightText: 2025-2026 Mitchel Stewart
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Shared JNI plumbing for the modules that call the Java framework from Rust threads.

use jni::errors::Error as JError;
use jni::objects::{JClass, JObject, JValue, JValueOwned};
use jni::signature::{RuntimeFieldSignature, RuntimeMethodSignature};
use jni::strings::JNIString;
use jni::{Env, JavaVM};

/// The attached VM for the raw pointer `AndroidApp` hands out.
pub(crate) fn vm_of(vm: usize) -> JavaVM {
    // SAFETY: the pointer comes from the running activity and stays valid for the process.
    unsafe { JavaVM::from_raw(vm as *mut jni::sys::JavaVM) }
}

/// Runs `f` on an attached thread inside a local reference frame. A Java exception that is pending
/// when a call fails is cleared and comes back as its `toString()`, so a missing permission reads
/// as `java.lang.SecurityException: ...` and not as "Java exception was thrown".
pub(crate) fn with_env<R>(
    vm: usize,
    f: impl FnOnce(&mut Env) -> Result<R, JError>,
) -> Result<R, String> {
    let attached = vm_of(vm).attach_current_thread(|env| -> Result<Result<R, String>, JError> {
        let out = env.with_local_frame(64, |env| f(env));
        Ok(out.map_err(|e| describe(env, e)))
    });
    attached.map_err(|e| e.to_string())?
}

/// What a failed call said, with the pending exception cleared.
pub(crate) fn describe(env: &mut Env, error: JError) -> String {
    if !matches!(error, JError::JavaException) {
        return error.to_string();
    }
    let text = (|| -> Result<String, JError> {
        let Some(throwable) = env.exception_occurred() else {
            return Err(JError::JavaException);
        };
        env.exception_clear();
        let string = env
            .call(&throwable, "toString", "()Ljava/lang/String;", &[])?
            .l()?;
        let string = env.cast_local::<jni::objects::JString>(string)?;
        string.try_to_string(env)
    })();
    text.unwrap_or_else(|_| {
        env.exception_clear();
        "a Java exception".to_owned()
    })
}

/// Calls by name and signature given as plain strings. The names here are built from constants in
/// part, which the compile-time signature macros cannot take.
pub(crate) trait EnvExt<'local> {
    fn call<'o>(
        &mut self,
        obj: &JObject<'o>,
        name: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JValueOwned<'local>, JError>;

    fn call_static(
        &mut self,
        class: &str,
        name: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JValueOwned<'local>, JError>;

    fn construct(
        &mut self,
        class: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JObject<'local>, JError>;

    fn class(&mut self, name: &str) -> Result<JClass<'local>, JError>;

    fn static_field(
        &mut self,
        class: &str,
        name: &str,
        sig: &str,
    ) -> Result<JValueOwned<'local>, JError>;

    /// A reference that outlives the local frame and can be shared between threads.
    fn global(&mut self, obj: &JObject) -> Result<Ref, JError>;
}

impl<'local> EnvExt<'local> for Env<'local> {
    fn call<'o>(
        &mut self,
        obj: &JObject<'o>,
        name: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JValueOwned<'local>, JError> {
        let sig = RuntimeMethodSignature::from_str(sig)?;
        self.call_method(obj, JNIString::from(name), sig.method_signature(), args)
    }

    fn call_static(
        &mut self,
        class: &str,
        name: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JValueOwned<'local>, JError> {
        let class = self.class(class)?;
        let sig = RuntimeMethodSignature::from_str(sig)?;
        self.call_static_method(&class, JNIString::from(name), sig.method_signature(), args)
    }

    fn construct(
        &mut self,
        class: &str,
        sig: impl AsRef<str>,
        args: &[JValue],
    ) -> Result<JObject<'local>, JError> {
        let class = self.class(class)?;
        let sig = RuntimeMethodSignature::from_str(sig)?;
        self.new_object(&class, sig.method_signature(), args)
    }

    fn class(&mut self, name: &str) -> Result<JClass<'local>, JError> {
        self.find_class(JNIString::from(name))
    }

    fn static_field(
        &mut self,
        class: &str,
        name: &str,
        sig: &str,
    ) -> Result<JValueOwned<'local>, JError> {
        let class = self.class(class)?;
        let sig = RuntimeFieldSignature::from_str(sig)?;
        self.get_static_field(&class, JNIString::from(name), sig.field_signature())
    }

    fn global(&mut self, obj: &JObject) -> Result<Ref, JError> {
        Ok(std::sync::Arc::new(self.new_global_ref(obj)?))
    }
}

/// A global reference to a Java object, valid on every thread. Shared, as `Global` is not `Clone`.
pub(crate) type Ref = std::sync::Arc<jni::refs::Global<JObject<'static>>>;
