//! A debug build's pages, from `dx serve` (mainframe's
//! mainframe-rpc-server/src/web.rs, which has scribe's shape): the browser
//! talks only to the admin listener, as it will in production, so the
//! cookie, the origin check and `/auth` are the real ones while the app
//! hot-reloads.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Response, StatusCode, header};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where `dx serve` listens: host:port, because the upgrade path dials it
/// with a bare `TcpStream`. devenv.nix's `web` process serves there.
const ADDR: &str = "127.0.0.1:8080";

/// Bounds the head read off the dev server's answer to an upgrade, so a
/// misbehaving one cannot grow the buffer without limit.
const MAX_UPGRADE_HEAD: usize = 16 * 1024;

/// The proxy to the Dioxus dev server.
#[derive(Debug, Clone)]
pub struct DevServer {
    client: reqwest::Client,
    addr: String,
}

impl Default for DevServer {
    fn default() -> Self {
        Self::at(ADDR.to_owned())
    }
}

impl DevServer {
    /// The dev server at `addr` (host:port).
    pub fn at(addr: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            addr,
        }
    }

    /// The dev server's answer to `req`. A WebSocket upgrade (dx's
    /// hot-reload socket) is tunnelled; anything else is one buffered
    /// round trip, which cannot carry a WebSocket.
    pub async fn proxy(&self, req: Request) -> Response<Body> {
        if wants_websocket(req.headers()) {
            self.tunnel(req).await
        } else {
            self.buffered(req).await
        }
    }

    async fn buffered(&self, req: Request) -> Response<Body> {
        let (parts, body) = req.into_parts();
        let target = format!(
            "http://{}{}",
            self.addr,
            parts.uri.path_and_query().map_or("/", |pq| pq.as_str())
        );
        let body = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "the request to proxy to dx serve could not be read");
                return plain(
                    StatusCode::BAD_REQUEST,
                    "the request body could not be read",
                );
            }
        };
        let mut request = self.client.request(parts.method, &target);
        // reqwest names the dev server as the host itself.
        for (name, value) in parts.headers.iter().filter(|(n, _)| *n != header::HOST) {
            request = request.header(name, value);
        }
        let answer = match request.body(body).send().await {
            Ok(answer) => answer,
            Err(e) => {
                tracing::debug!(error = %e, "dx serve did not answer");
                return unavailable(&self.addr);
            }
        };
        let status = answer.status();
        let headers = answer.headers().clone();
        match answer.bytes().await {
            Ok(body) => {
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = status;
                *response.headers_mut() = headers;
                response.extensions_mut().insert(crate::admin_http::DevPage);
                response
            }
            Err(e) => {
                tracing::warn!(error = %e, "dx serve's answer could not be read");
                plain(
                    StatusCode::BAD_GATEWAY,
                    "dx serve's answer could not be read",
                )
            }
        }
    }

    /// Replay the handshake to the dev server over a raw `TcpStream` and,
    /// once it answers `101 Switching Protocols`, take the client's
    /// connection from hyper and pipe bytes both ways.
    async fn tunnel(&self, mut req: Request) -> Response<Body> {
        // Taken while `req` is whole: hyper left it among the extensions
        // when it saw the `Upgrade` header.
        let on_upgrade = hyper::upgrade::on(&mut req);

        let mut dev = match TcpStream::connect(&self.addr).await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(error = %e, "dx serve did not take the WebSocket");
                return unavailable(&self.addr);
            }
        };
        let path = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
        let mut handshake = format!("{} {path} HTTP/1.1\r\n", req.method()).into_bytes();
        for (name, value) in req.headers().iter().filter(|(n, _)| *n != header::HOST) {
            handshake.extend_from_slice(name.as_str().as_bytes());
            handshake.extend_from_slice(b": ");
            handshake.extend_from_slice(value.as_bytes());
            handshake.extend_from_slice(b"\r\n");
        }
        handshake.extend_from_slice(format!("host: {}\r\n\r\n", self.addr).as_bytes());
        if let Err(e) = dev.write_all(&handshake).await {
            tracing::warn!(error = %e, "the WebSocket handshake could not be sent to dx serve");
            return plain(StatusCode::BAD_GATEWAY, "dx serve could not be reached");
        }

        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let (head, leftover) = loop {
            match dev.read(&mut chunk).await {
                Ok(0) => {
                    tracing::warn!("dx serve closed the connection during a WebSocket handshake");
                    return plain(StatusCode::BAD_GATEWAY, "dx serve closed the connection");
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "dx serve's WebSocket handshake could not be read");
                    return plain(
                        StatusCode::BAD_GATEWAY,
                        "dx serve's answer could not be read",
                    );
                }
            }
            if let Some(end) = head_end(&buf) {
                let leftover = buf.split_off(end + 4);
                buf.truncate(end);
                break (buf, leftover);
            }
            if buf.len() > MAX_UPGRADE_HEAD {
                tracing::warn!("dx serve's WebSocket handshake is too long");
                return plain(StatusCode::BAD_GATEWAY, "dx serve's answer is too long");
            }
        };
        let Some((status, headers)) = parse_head(&head) else {
            tracing::warn!("dx serve's WebSocket handshake is not HTTP");
            return plain(StatusCode::BAD_GATEWAY, "dx serve's answer is not HTTP");
        };
        let mut response = Response::builder().status(status);
        for (name, value) in &headers {
            response = response.header(name, value);
        }
        let response = response.body(Body::empty()).expect("the head was parsed");
        if status != StatusCode::SWITCHING_PROTOCOLS {
            return response;
        }

        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(e) => {
                    tracing::warn!(error = %e, "the browser's connection could not be taken for the WebSocket");
                    return;
                }
            };
            let mut client = hyper_util::rt::TokioIo::new(upgraded);
            // What dx sent after its head, before the browser had a tunnel.
            if !leftover.is_empty() && client.write_all(&leftover).await.is_err() {
                return;
            }
            if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut dev).await {
                tracing::debug!(error = %e, "the WebSocket to dx serve closed");
            }
        });
        response
    }
}

/// `Connection` lists `upgrade` (among other tokens, maybe) and `Upgrade`
/// is `websocket`, in any case.
fn wants_websocket(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    let websocket = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    connection && websocket
}

fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A response's status line and headers, without the blank line. By hand:
/// this only ever reads a loopback dev server's handshake.
fn parse_head(head: &[u8]) -> Option<(StatusCode, Vec<(String, String)>)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let code = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let status = StatusCode::from_u16(code).ok()?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Some((status, headers))
}

fn plain(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = status;
    response
}

/// dx serve is not running: say so, and how to start it.
fn unavailable(addr: &str) -> Response<Body> {
    let page = format!(
        r#"<!DOCTYPE html>
<html>
<head><title>Spoolframe: dx serve is not running</title></head>
<body style="font-family: system-ui; padding: 2rem;">
<h1>dx serve is not running</h1>
<p>This is a debug server without the <code>embed-web</code> feature: it takes
its pages from the Dioxus dev server at <code>http://{addr}</code>, which did
not answer.</p>
<p>Start it, with Tailwind watching, from <code>spoolframe/</code>:</p>
<pre>devenv up</pre>
<p>Or serve a bundle instead:</p>
<pre>devenv --no-tui tasks run spoolframe:web
SPOOLFRAME_WEB_DIST=$PWD/target/dx/spoolframe-web/release/web/public \
  cargo run -p spoolframe-server --features embed-web -- --config dev.ucl</pre>
</body>
</html>"#
    );
    let mut response = Response::new(Body::from(page));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn a_websocket_upgrade_is_told_from_other_requests() {
        assert!(wants_websocket(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
        ])));
        assert!(!wants_websocket(&headers(&[("connection", "Upgrade")])));
        assert!(!wants_websocket(&headers(&[("upgrade", "websocket")])));
        assert!(!wants_websocket(&HeaderMap::new()));
    }

    /// Reads one request head off `stream`.
    async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while head_end(&buf).is_none() {
            let n = stream.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "closed before a whole head");
            buf.extend_from_slice(&chunk[..n]);
        }
        buf
    }

    /// The router in front, as the admin listener has it: a fallback.
    async fn listener_in_front(dev: DevServer) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback_service(tower::service_fn(move |req: Request| {
            let dev = dev.clone();
            async move { Ok::<_, std::convert::Infallible>(dev.proxy(req).await) }
        }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// A stand-in dx serve: answers one upgrade with a 101, then echoes,
    /// so the tunnel is seen to carry bytes both ways.
    async fn echoing_dev_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_head(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\n\
                      Upgrade: websocket\r\n\
                      Connection: Upgrade\r\n\
                      Sec-WebSocket-Accept: from-dx\r\n\r\n",
                )
                .await
                .unwrap();
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn the_hot_reload_socket_is_tunnelled_both_ways() {
        let dev = echoing_dev_server().await;
        let front = listener_in_front(DevServer::at(dev.to_string())).await;
        let mut browser = TcpStream::connect(front).await.unwrap();
        browser
            .write_all(
                format!(
                    "GET /_dioxus?build_id=1 HTTP/1.1\r\n\
                     Host: {front}\r\n\
                     Connection: Upgrade\r\n\
                     Upgrade: websocket\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let head = String::from_utf8(read_head(&mut browser).await).unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(
            head.to_lowercase()
                .contains("sec-websocket-accept: from-dx"),
            "{head}"
        );

        browser.write_all(b"reload, please").await.unwrap();
        let mut echoed = [0u8; 14];
        browser.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"reload, please");
    }

    #[tokio::test]
    async fn a_page_is_dx_serve_s_answer_with_dx_serve_as_its_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dev = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await;
            let body = b"<html>from dx</html>";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
            String::from_utf8(head).unwrap().to_lowercase()
        });

        let request = Request::builder()
            .uri("/login?next=%2Fadmin")
            .header(header::HOST, "192.168.86.10:7431")
            .body(Body::empty())
            .unwrap();
        let response = DevServer::at(dev.to_string()).proxy(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"<html>from dx</html>");

        let head = seen.await.unwrap();
        assert!(head.starts_with("get /login?next=%2fadmin "), "{head}");
        assert!(head.contains(&format!("host: {dev}\r\n")), "{head}");
        assert!(!head.contains("192.168.86.10"), "{head}");
    }

    #[tokio::test]
    async fn without_dx_serve_a_page_says_how_to_start_it() {
        // A port nothing listens on: bound, learnt, let go.
        let dead = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let dev = DevServer::at(dead.to_string());
        let response = dev
            .proxy(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page = String::from_utf8_lossy(&body);
        assert!(page.contains("dx serve is not running"), "{page}");
        assert!(page.contains("devenv up"), "{page}");

        // The hot-reload socket too.
        let response = dev
            .proxy(
                Request::builder()
                    .uri("/_dioxus")
                    .header(header::CONNECTION, "Upgrade")
                    .header(header::UPGRADE, "websocket")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
