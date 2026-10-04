//! The browser's passkey prompts: `navigator.credentials.create` and
//! `.get`, given the server's options and answering what the server reads.
//!
//! WebAuthn's own JSON forms (`parseCreationOptionsFromJSON`, `toJSON`)
//! would do this, but not every browser in use has them, Safari least of
//! all. So the options' binary fields are turned into bytes here, and the
//! credential's back into base64url.

use js_sys::{Array, Function, Object, Reflect, Uint8Array};
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::logic::{base64url, from_base64url, passkey_prompt_error};

/// Whether this page can use passkeys at all: the browser has WebAuthn,
/// and the page is in a secure context (HTTPS, or `localhost`).
pub fn supported() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    window.is_secure_context()
        && Reflect::has(&window, &"PublicKeyCredential".into()).unwrap_or(false)
}

/// Make a passkey: the browser's prompt for the server's `options`
/// (`{"publicKey": …}` from `me.passkeyAddBegin`), and what it made, for
/// `me.passkeyAddFinish`.
pub async fn create(options: &Value) -> Result<Value, String> {
    prompt(options, true).await.map_err(|e| explain(&e, true))
}

/// Sign with a passkey: the browser's prompt for the server's `options`
/// (from `/auth/passkey/begin`), and what the passkey signed.
pub async fn get(options: &Value) -> Result<Value, String> {
    prompt(options, false).await.map_err(|e| explain(&e, false))
}

fn get_in(target: &JsValue, key: &str) -> Result<JsValue, JsValue> {
    Reflect::get(target, &key.into())
}

/// Replace the base64url text at `key` of `target` by its bytes.
fn bytes_at(target: &JsValue, key: &str) -> Result<(), JsValue> {
    let bytes = get_in(target, key)?
        .as_string()
        .and_then(|text| from_base64url(&text))
        .ok_or_else(|| JsValue::from_str(&format!("the server's {key} is not base64url")))?;
    Reflect::set(target, &key.into(), &Uint8Array::from(bytes.as_slice()))?;
    Ok(())
}

/// [`bytes_at`] `id` for each credential listed at `key`, if any is.
fn ids_at(target: &JsValue, key: &str) -> Result<(), JsValue> {
    let listed = get_in(target, key)?;
    if listed.is_undefined() || listed.is_null() {
        return Ok(());
    }
    for credential in Array::from(&listed).iter() {
        bytes_at(&credential, "id")?;
    }
    Ok(())
}

/// The base64url of the buffer at `key` of `source`; `None` if there is
/// none.
fn text_of(source: &JsValue, key: &str) -> Result<Option<String>, JsValue> {
    let buffer = get_in(source, key)?;
    if buffer.is_undefined() || buffer.is_null() {
        return Ok(None);
    }
    Ok(Some(base64url(&Uint8Array::new(&buffer).to_vec())))
}

async fn prompt(options: &Value, adding: bool) -> Result<Value, JsValue> {
    let key = js_sys::JSON::parse(&options["publicKey"].to_string())?;
    bytes_at(&key, "challenge")?;
    if adding {
        bytes_at(&get_in(&key, "user")?, "id")?;
        ids_at(&key, "excludeCredentials")?;
    } else {
        ids_at(&key, "allowCredentials")?;
    }
    let request = Object::new();
    Reflect::set(&request, &"publicKey".into(), &key)?;
    let credentials = web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window"))?
        .navigator()
        .credentials();
    let promise = if adding {
        credentials.create_with_options(request.unchecked_ref())?
    } else {
        credentials.get_with_options(request.unchecked_ref())?
    };
    let credential = JsFuture::from(promise).await?;
    if credential.is_null() || credential.is_undefined() {
        return Err(JsValue::from_str("the browser gave no credential"));
    }
    let response = get_in(&credential, "response")?;
    let response = if adding {
        json!({
            "clientDataJSON": text_of(&response, "clientDataJSON")?,
            "attestationObject": text_of(&response, "attestationObject")?,
            "transports": transports(&response),
        })
    } else {
        json!({
            "clientDataJSON": text_of(&response, "clientDataJSON")?,
            "authenticatorData": text_of(&response, "authenticatorData")?,
            "signature": text_of(&response, "signature")?,
            "userHandle": text_of(&response, "userHandle")?,
        })
    };
    // The client's extension results are left out: the server needs none
    // of them, and their shapes differ between browsers.
    Ok(json!({
        "id": get_in(&credential, "id")?.as_string(),
        "rawId": text_of(&credential, "rawId")?,
        "type": get_in(&credential, "type")?.as_string(),
        "response": response,
        "extensions": {},
    }))
}

/// How the new passkey's authenticator can be reached, where the browser
/// says (`getTransports` is newer than the rest).
fn transports(response: &JsValue) -> Option<Vec<String>> {
    let function: Function = get_in(response, "getTransports").ok()?.dyn_into().ok()?;
    let listed = function.call0(response).ok()?;
    Some(
        Array::from(&listed)
            .iter()
            .filter_map(|transport| transport.as_string())
            .collect(),
    )
}

/// What a failed prompt means, for the person: by the `DOMException`'s
/// name where it is one.
fn explain(error: &JsValue, adding: bool) -> String {
    let name = get_in(error, "name").ok().and_then(|name| name.as_string());
    match (name, error.as_string()) {
        (Some(name), _) => passkey_prompt_error(&name, adding),
        (None, Some(text)) => text,
        (None, None) => passkey_prompt_error("unknown", adding),
    }
}
