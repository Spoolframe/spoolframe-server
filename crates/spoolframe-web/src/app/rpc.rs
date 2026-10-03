//! The admin JSON-RPC socket, at `/jsonrpc` on the page's own origin. The
//! session cookie rides on the upgrade, as the CLI's Bearer token does.

use std::cell::RefCell;
use std::rc::Rc;

use jsonrpsee::core::client::Error;
use jsonrpsee::wasm_client::{Client, WasmClientBuilder};

use crate::logic::ApiError;

thread_local! {
    static CLIENT: RefCell<Option<Rc<Client>>> = const { RefCell::new(None) };
}

fn url() -> String {
    let location = web_sys::window().expect("a page has a window").location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss",
        _ => "ws",
    };
    format!("{scheme}://{}/jsonrpc", location.host().unwrap_or_default())
}

/// The socket, opened on first use and again after it dropped.
pub async fn client() -> Result<Rc<Client>, ApiError> {
    if let Some(client) = CLIENT.with(|c| c.borrow().clone())
        && client.is_connected()
    {
        return Ok(client);
    }
    let client = Rc::new(
        WasmClientBuilder::default()
            .build(url())
            .await
            .map_err(|e| ApiError::transport(e.to_string()))?,
    );
    CLIENT.with(|c| *c.borrow_mut() = Some(client.clone()));
    Ok(client)
}

/// Forget the socket: after signing out, its cookie is gone.
pub fn forget() {
    CLIENT.with(|c| c.borrow_mut().take());
}

/// A call's error as the server put it.
pub fn error(e: Error) -> ApiError {
    match e {
        Error::Call(e) => ApiError {
            code: e.code(),
            message: e.message().to_owned(),
            data: e.data().and_then(|d| serde_json::from_str(d.get()).ok()),
        },
        other => ApiError::transport(other.to_string()),
    }
}
