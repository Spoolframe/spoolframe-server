//! The web UI's files (P3-01), served on the admin listener.
//!
//! Built with `embed-web`, the server carries the `dx bundle` output named
//! by `SPOOLFRAME_WEB_DIST` at compile time: `nix build` puts it there. A
//! debug build reads that directory at run time instead (rust-embed's own
//! behaviour), so `dx bundle` and a page reload are the whole loop. A
//! debug build without the feature takes every page from `dx serve`
//! ([`dev_server`]), hot reload included. A release build without it has
//! no pages, and says so.

use axum::body::Body;
use axum::extract::Request;
use axum::http::Response;
#[cfg(feature = "embed-web")]
use axum::http::header;
#[cfg(not(all(debug_assertions, not(feature = "embed-web"))))]
use axum::{http::StatusCode, response::IntoResponse};

#[cfg(all(debug_assertions, not(feature = "embed-web")))]
mod dev_server;

#[cfg(feature = "embed-web")]
#[derive(rust_embed::RustEmbed)]
#[folder = "$SPOOLFRAME_WEB_DIST"]
struct Bundle;

/// The web UI's pages, as this build has them.
#[derive(Debug, Clone, Default)]
pub struct Web {
    #[cfg(all(debug_assertions, not(feature = "embed-web")))]
    dev_server: dev_server::DevServer,
}

impl Web {
    /// The answer to `req`, which no route of the listener took.
    pub async fn page(&self, req: Request) -> Response<Body> {
        #[cfg(feature = "embed-web")]
        {
            bundled(req.uri().path())
        }
        #[cfg(all(debug_assertions, not(feature = "embed-web")))]
        {
            self.dev_server.proxy(req).await
        }
        #[cfg(all(not(debug_assertions), not(feature = "embed-web")))]
        {
            let _ = req;
            (
                StatusCode::NOT_FOUND,
                "the web UI is not built into this server",
            )
                .into_response()
        }
    }

    /// Whether a WebSocket upgrade at `path` is the pages' rather than the
    /// admin API's: dx serve's hot-reload socket, in a build that takes
    /// its pages from dx serve. A build that cannot have it lets the API
    /// answer every upgrade, as it always has.
    pub fn takes_upgrade(&self, path: &str) -> bool {
        cfg!(all(debug_assertions, not(feature = "embed-web"))) && path.starts_with("/_dioxus")
    }
}

/// The file at `path`, or the app's page for any path that is not a file:
/// the app routes `/admin`, `/login` and the rest itself.
#[cfg(feature = "embed-web")]
fn bundled(path: &str) -> Response<Body> {
    let path = path.trim_start_matches('/');
    if let Some(file) = Bundle::get(path) {
        return file_response(path, file);
    }
    // A missing file with an extension is a missing file, not a route.
    let last = path.rsplit('/').next().unwrap_or_default();
    if !last.contains('.')
        && let Some(index) = Bundle::get("index.html")
    {
        return file_response("index.html", index);
    }
    (StatusCode::NOT_FOUND, "no such file").into_response()
}

#[cfg(feature = "embed-web")]
fn file_response(path: &str, file: rust_embed::EmbeddedFile) -> Response<Body> {
    let mime = file.metadata.mimetype().to_owned();
    // dx names assets by their hash: they never change under one name.
    // The page itself must be asked for again, or a new build is not seen.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::new(Body::from(file.data.into_owned()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        mime.parse()
            .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(cache),
    );
    response
}

#[cfg(all(test, debug_assertions, not(feature = "embed-web")))]
mod tests {
    use super::*;

    #[test]
    fn only_dx_serve_s_own_socket_is_taken_from_the_api() {
        let web = Web::default();
        assert!(web.takes_upgrade("/_dioxus"));
        // Where the CLI and the web UI open the admin API's socket.
        assert!(!web.takes_upgrade("/"));
        assert!(!web.takes_upgrade("/jsonrpc"));
    }
}

#[cfg(all(test, feature = "embed-web"))]
mod embedded_tests {
    use super::*;

    /// The page served at `/` and at any route is the bundle's
    /// `index.html`, which keeps a browser's night mode (Dark Reader,
    /// Firefox for iOS) off the already-dark pages: in the page itself,
    /// as Dark Reader decides as it loads.
    #[tokio::test]
    async fn the_served_page_tells_night_modes_it_is_dark_already() {
        for path in ["/", "/admin"] {
            let response = bundled(path);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let page = String::from_utf8(body.to_vec()).unwrap();
            let head = &page[..page.find("</head>").expect("a head")];
            assert!(
                head.contains(r#"<meta name="color-scheme" content="light dark">"#),
                "{path}: {head}"
            );
            assert!(
                head.contains(r#"<meta name="darkreader-lock">"#),
                "{path}: {head}"
            );
            assert!(head.contains("<title>Spoolframe</title>"), "{path}: {head}");
        }
    }
}
