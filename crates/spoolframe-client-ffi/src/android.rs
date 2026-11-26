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
//! nothing (one with a null Context is logged, and the next may try).
//!
//! ndk-context holds one Context per process, and 0.1.1 cannot be asked
//! whether one is set without a panic. If another library in the app
//! set it first, ndk-context replaces that Context with this one and
//! then panics on its assert: the panic is caught and logged, and the
//! Context it holds from then on is ours. Both are the app's, and the
//! other library's global reference stays valid, so its reads still
//! work. With `panic = "abort"` the panic aborts the app instead, so an
//! app with two libraries that set ndk-context calls only one of them.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use jni::objects::{JClass, JObject};
use jni::{Env, EnvUnowned, Outcome};

/// ndk-context holds a Context: this call set it, or another library
/// did. A call that failed before that (a null Context) leaves it unset,
/// for the next call to try again.
static HELD: Mutex<bool> = Mutex::new(false);

/// `uniffi.spoolframe_client_ffi.AndroidContext.init(context)`: JNI
/// writes each `_` of the package's name as `_1`. Never unwinds into
/// the JVM: `with_env` catches a panic, and a failure is logged and the
/// app goes on, as a panic across `extern "system"` would abort it at
/// start.
#[unsafe(no_mangle)]
pub extern "system" fn Java_uniffi_spoolframe_1client_1ffi_AndroidContext_init<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
) {
    let Ok(mut held) = HELD.lock() else {
        return;
    };
    if *held {
        return;
    }
    match env.with_env(|env| hand_over(env, &context)).into_outcome() {
        Outcome::Ok(()) => *held = true,
        Outcome::Err(Failed::Taken) => {
            *held = true;
            tracing::warn!(
                "ndk-context already held a Context another library set; it holds this app's Context from here on"
            );
        }
        Outcome::Err(Failed::Not(why)) => {
            tracing::error!("the Android Context was not handed over: {why}");
        }
        Outcome::Panic(_) => {
            tracing::error!("the Android Context was not handed over: a panic");
        }
    }
}

enum Failed {
    /// ndk-context held a Context already; it holds ours now.
    Taken,
    /// Nothing was handed over, and why.
    Not(String),
}

/// `with_env` needs its error to take JNI's; every JNI call below maps
/// its own with what it was for, so this is only the fallback.
impl From<jni::errors::Error> for Failed {
    fn from(e: jni::errors::Error) -> Self {
        Failed::Not(e.to_string())
    }
}

fn hand_over(env: &Env<'_>, context: &JObject<'_>) -> Result<(), Failed> {
    if context.is_null() {
        return Err(Failed::Not("the Context is null".into()));
    }
    let vm = env
        .get_java_vm()
        .map_err(|e| Failed::Not(format!("no JavaVM: {e}")))?;
    // Kept for the life of the process: ndk-context holds the raw
    // reference, so the global ref is never released. `into_raw` gives
    // it up without deleting it.
    let context = env
        .new_global_ref(context)
        .map_err(|e| Failed::Not(format!("no global reference to the Context: {e}")))?
        .into_raw();
    // ndk-context asserts it is set only once in the process, and another
    // library may have set it. It replaces the Context before it asserts,
    // so on a panic ours is already the one it holds. A panic is caught
    // here; with `panic = "abort"` it still aborts. There is no check to
    // make first: `android_context()` panics when none is set, so asking
    // would panic (or abort) on every first call.
    let set = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: both pointers are valid for the life of the process:
        // the VM always, the Context through the global ref never deleted.
        unsafe {
            ndk_context::initialize_android_context(vm.get_raw().cast(), context.cast());
        }
    }));
    set.map_err(|_| Failed::Taken)
}
