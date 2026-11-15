//! What Android needs before any request: its certificate verifier, which rustls-platform-verifier
//! reaches through the JVM, given the app's context once. The app calls `Native.init` at its start
//! and its document provider's, whichever is first, before any TLS.

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JObject};
use jni::EnvUnowned;

#[jni::jni_mangle("storage.cairn.android.Native")]
pub fn init<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    context: JObject<'caller>,
) {
    env.with_env(|env| rustls_platform_verifier::android::init_with_env(env, context))
        .resolve::<ThrowRuntimeExAndDefault>();
}
