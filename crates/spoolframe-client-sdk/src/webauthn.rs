//! App passkeys in WebAuthn's JSON forms: the options a platform's
//! passkey API takes as JSON (Android's Credential Manager, a browser's
//! `PublicKeyCredential.parseRequestOptionsFromJSON`), and its answer
//! read back. Byte strings are base64url, as WebAuthn writes them. The
//! Apple app maps the same bytes to `ASAuthorization` itself; each other
//! platform needs no more than this.
//!
//! The server wants the user verified every time, so every option asks
//! for it (`"required"`); left at the default, a platform may sign
//! without it, and the server refuses that.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, PasskeyCreation, PasskeyRequest,
};

use crate::SdkError;

/// How long a platform may wait for the user: a challenge lives 5
/// minutes on the server.
const TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// ES256, the one algorithm an app passkey is made with.
const ES256: i64 = -7;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `text` as bytes, in any base64 a platform writes: url-safe or not,
/// padded or not.
fn unb64(field: &str, text: &str) -> Result<Vec<u8>, SdkError> {
    let trimmed = text.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(text))
        .or_else(|_| STANDARD.decode(text))
        .map_err(|_| SdkError::InvalidInput(format!("the passkey's {field} is not base64")))
}

fn descriptors(ids: &[Vec<u8>]) -> Vec<Value> {
    ids.iter()
        .map(|id| json!({ "type": "public-key", "id": b64(id) }))
        .collect()
}

/// `request` as WebAuthn's `PublicKeyCredentialRequestOptionsJSON`.
pub fn request_options(request: &PasskeyRequest) -> String {
    json!({
        "challenge": b64(&request.challenge),
        "rpId": request.rp_id,
        "allowCredentials": descriptors(&request.allow_credential_ids),
        "userVerification": "required",
        "timeout": TIMEOUT_MS,
    })
    .to_string()
}

/// `creation` as WebAuthn's `PublicKeyCredentialCreationOptionsJSON`: a
/// discoverable ES256 passkey, the user verified, no attestation.
pub fn creation_options(creation: &PasskeyCreation) -> String {
    json!({
        "challenge": b64(&creation.challenge),
        "rp": { "id": creation.rp_id, "name": creation.rp_id },
        "user": {
            "id": b64(&creation.user_id),
            "name": creation.user_name,
            "displayName": creation.display_name,
        },
        "pubKeyCredParams": [{ "type": "public-key", "alg": ES256 }],
        "excludeCredentials": descriptors(&creation.exclude_credential_ids),
        "authenticatorSelection": {
            "residentKey": "required",
            "requireResidentKey": true,
            "userVerification": "required",
        },
        "attestation": "none",
        "timeout": TIMEOUT_MS,
    })
    .to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credential<R> {
    /// The credential id, base64url: `rawId`, else `id`, which WebAuthn
    /// makes the same.
    raw_id: Option<String>,
    id: Option<String>,
    response: R,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Assertion {
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    authenticator_data: String,
    signature: String,
    user_handle: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    #[serde(rename = "clientDataJSON")]
    client_data_json: String,
    attestation_object: String,
}

fn parsed<R: serde::de::DeserializeOwned>(json: &str) -> Result<Credential<R>, SdkError> {
    serde_json::from_str(json)
        .map_err(|e| SdkError::InvalidInput(format!("not a passkey's answer: {e}")))
}

fn credential_id<R>(credential: &Credential<R>) -> Result<Vec<u8>, SdkError> {
    let id = credential
        .raw_id
        .as_deref()
        .or(credential.id.as_deref())
        .ok_or_else(|| SdkError::InvalidInput("the passkey's answer has no id".into()))?;
    unb64("id", id)
}

/// A platform's answer to [`request_options`], WebAuthn's
/// `AuthenticationResponseJSON`, as the sign-in to finish
/// ([`crate::Client::finish_passkey_sign_in`],
/// [`crate::Session::finish_confirm_passkey`]). An empty user handle is
/// none, which the server takes.
pub fn assertion(json: &str) -> Result<FinishPasskeySignIn, SdkError> {
    let credential: Credential<Assertion> = parsed(json)?;
    let r = &credential.response;
    let user_handle = match r.user_handle.as_deref() {
        Some(handle) => Some(unb64("user handle", handle)?).filter(|h| !h.is_empty()),
        None => None,
    };
    Ok(FinishPasskeySignIn {
        credential_id: credential_id(&credential)?,
        authenticator_data: unb64("authenticator data", &r.authenticator_data)?,
        client_data_json: unb64("client data", &r.client_data_json)?,
        signature: unb64("signature", &r.signature)?,
        user_handle,
    })
}

/// A platform's answer to [`creation_options`], WebAuthn's
/// `RegistrationResponseJSON`, as the passkey to keep, called `name`
/// ([`crate::Session::finish_passkey_registration`]).
pub fn registration(json: &str, name: &str) -> Result<FinishPasskeyRegistration, SdkError> {
    let credential: Credential<Registration> = parsed(json)?;
    let r = &credential.response;
    Ok(FinishPasskeyRegistration {
        credential_id: credential_id(&credential)?,
        attestation_object: unb64("attestation", &r.attestation_object)?,
        client_data_json: unb64("client data", &r.client_data_json)?,
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> PasskeyRequest {
        PasskeyRequest {
            rp_id: "abc.spoolframe.app".into(),
            origin: "https://abc.spoolframe.app".into(),
            challenge: vec![0xfb, 0xff, 0x01],
            allow_credential_ids: vec![vec![1, 2, 3]],
        }
    }

    #[test]
    fn a_request_is_webauthns_json_with_the_user_verified() {
        let options: Value = serde_json::from_str(&request_options(&request())).unwrap();
        assert_eq!(
            options,
            json!({
                "challenge": "-_8B",
                "rpId": "abc.spoolframe.app",
                "allowCredentials": [{ "type": "public-key", "id": "AQID" }],
                "userVerification": "required",
                "timeout": 300_000,
            })
        );
    }

    #[test]
    fn a_creation_asks_for_a_discoverable_es256_passkey() {
        let creation = PasskeyCreation {
            rp_id: "abc.spoolframe.app".into(),
            origin: "https://abc.spoolframe.app".into(),
            challenge: vec![1],
            user_id: vec![2],
            user_name: "anna @ Home Media".into(),
            display_name: "Anna".into(),
            exclude_credential_ids: Vec::new(),
        };
        let options: Value = serde_json::from_str(&creation_options(&creation)).unwrap();
        assert_eq!(
            (
                &options["user"]["id"],
                &options["pubKeyCredParams"][0]["alg"],
                &options["authenticatorSelection"]["residentKey"],
            ),
            (&json!("Ag"), &json!(-7), &json!("required"))
        );
    }

    #[test]
    fn an_answer_reads_back_as_the_sign_in_to_finish() {
        let answer = json!({
            "id": "AQID",
            "rawId": "AQID",
            "type": "public-key",
            "response": {
                "clientDataJSON": "e30",
                "authenticatorData": "BAU",
                "signature": "Bgc=",
                "userHandle": "CA",
            },
        });
        assert_eq!(
            assertion(&answer.to_string()).unwrap(),
            FinishPasskeySignIn {
                credential_id: vec![1, 2, 3],
                authenticator_data: vec![4, 5],
                client_data_json: b"{}".to_vec(),
                signature: vec![6, 7],
                user_handle: Some(vec![8]),
            }
        );
    }

    #[test]
    fn an_empty_user_handle_is_none() {
        let answer = json!({
            "id": "AQID",
            "response": {
                "clientDataJSON": "e30",
                "authenticatorData": "BAU",
                "signature": "Bgc",
                "userHandle": "",
            },
        });
        assert_eq!(assertion(&answer.to_string()).unwrap().user_handle, None);
    }

    #[test]
    fn what_is_not_an_answer_is_invalid_input() {
        assert!(matches!(assertion("{}"), Err(SdkError::InvalidInput(_))));
    }
}
