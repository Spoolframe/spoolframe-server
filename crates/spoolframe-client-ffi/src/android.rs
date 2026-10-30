//! Hands the app's Context to ndk-context (the `android` feature, on
//! Android only). The SDK's network crates (iroh's DNS resolver, through
//! hickory and n0-dns-resolver) read it to call Android's Java APIs;
//! without it they abort with "android context was not initialized".
//!
//! UniFFI calls through JNA and carries no JNIEnv, so this one call is
//! plain JNI, on a class beside the generated bindings, which the app
//! declares once:
//!
//! ```kotlin
//! package uniffi.spoolframe_client_ffi
//!
//! object AndroidContext {
//!     init { System.loadLibrary("spoolframe_client_ffi") }
//!
//!     @JvmStatic external fun init(context: android.content.Context)
//! }
//! ```
//!
//! `AndroidContext.init(context.applicationContext)` before
//! `SpoolClient.open`. It takes once per process: a later call does
//! nothing (one with a null Context is logged, and the next may try). ndk-context holds one Context per process; if another
//! library in the app set it first, this call logs and leaves it.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use jni::JNIEnv;
use jni::objects::{JClass, JObject};

/// ndk-context holds a Context: this call set it, or another library
/// did. A call that failed before that (a null Context) leaves it unset,
/// for the next call to try again.
static HELD: Mutex<bool> = Mutex::new(false);

/// `uniffi.spoolframe_client_ffi.AndroidContext.init(context)`: JNI
/// writes each `_` of the package's name as `_1`. Never unwinds into
/// the JVM: a failure is logged and the app goes on, as a panic across
/// `extern "system"` would abort it at start.
#[unsafe(no_mangle)]
pub extern "system" fn Java_uniffi_spoolframe_1client_1ffi_AndroidContext_init(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let Ok(mut held) = HELD.lock() else {
        return;
    };
    if *held {
        return;
    }
    match hand_over(&env, &context) {
        Ok(()) => *held = true,
        Err(Failed::Taken) => {
            *held = true;
            tracing::warn!("ndk-context already holds a Context: another library set it");
        }
        Err(Failed::Not(why)) => {
            tracing::error!("the Android Context was not handed over: {why}");
        }
    }
}

enum Failed {
    /// ndk-context held a Context already.
    Taken,
    /// Nothing was handed over, and why.
    Not(String),
}

fn hand_over(env: &JNIEnv, context: &JObject) -> Result<(), Failed> {
    if context.is_null() {
        return Err(Failed::Not("the Context is null".into()));
    }
    let vm = env
        .get_java_vm()
        .map_err(|e| Failed::Not(format!("no JavaVM: {e}")))?;
    // Kept for the life of the process: ndk-context holds the raw
    // reference, so the global ref must never be released.
    let context = env
        .new_global_ref(context)
        .map_err(|e| Failed::Not(format!("no global reference to the Context: {e}")))?;
    // ndk-context asserts it is set only once in the process, and another
    // library may have set it. A panic is caught here; with
    // `panic = "abort"` it still aborts, as ndk-context has no way to ask.
    let set = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: both pointers are valid for the life of the process:
        // the VM always, the Context through the global ref leaked below.
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_java_vm_pointer().cast(),
                context.as_obj().as_raw().cast(),
            );
        }
    }));
    std::mem::forget(context);
    set.map_err(|_| Failed::Taken)
}
