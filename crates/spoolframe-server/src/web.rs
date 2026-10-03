//! The web UI's files (P3-01), served on the admin listener.
//!
//! Built with `embed-web`, the server carries the `dx bundle` output named
//! by `SPOOLFRAME_WEB_DIST` at compile time: `nix build` puts it there. A
//! debug build reads that directory at run time instead (rust-embed's own
//! behaviour), so `dx bundle` and a page reload are the whole loop. Without
//! the feature there are no pages, and the server says so.

use axum::body::Body;
#[cfg(feature = "embed-web")]
use axum::http::header;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;

#[cfg(feature = "embed-web")]
#[derive(rust_embed::RustEmbed)]
#[folder = "$SPOOLFRAME_WEB_DIST"]
struct Bundle;

/// The file at `path`, or the app's page for any path that is not a file:
/// the app routes `/admin`, `/login` and the rest itself.
pub fn page(path: &str) -> Response<Body> {
    #[cfg(feature = "embed-web")]
    {
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
    #[cfg(not(feature = "embed-web"))]
    {
        let _ = path;
        (
            StatusCode::NOT_FOUND,
            "the web UI is not built into this server",
        )
            .into_response()
    }
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

#[cfg(all(test, not(feature = "embed-web")))]
mod tests {
    use super::*;

    #[test]
    fn without_the_bundle_every_page_says_so() {
        assert_eq!(page("/admin").status(), StatusCode::NOT_FOUND);
    }
}
