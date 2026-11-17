#![allow(clippy::disallowed_types)]
//! Fork: browser boundary for the unauthenticated local (desktop) mode.
//!
//! Local mode treats every request as the default user, so a web page must not
//! be able to reach it through the user's browser. Allow only this machine's
//! app: a loopback `Host` (blocks DNS rebinding), and either no `Origin`
//! (Node/Rust clients) or the AionUi renderer's own origin. Proxies such as the
//! WebUI host authenticate browsers themselves and forward without `Origin`.

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Uri, header};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;

/// The packaged renderer is a `file://` page; Electron sends this origin on
/// WebSocket upgrades. Browsers serialize opaque origins as `null`, which is
/// deliberately not accepted.
const PACKAGED_RENDERER_ORIGIN: &str = "file://";

/// Reject local-mode requests that a browser page outside the app could send.
pub async fn local_origin_guard(request: Request, next: Next) -> Result<Response, ApiError> {
    if let Err(reason) = check_local_request(request.headers()) {
        tracing::warn!(
            reason,
            method = %request.method(),
            path = %request.uri().path(),
            "rejected local-mode request from outside this machine's app"
        );
        return Err(ApiError::Forbidden(reason.to_owned()));
    }
    Ok(next.run(request).await)
}

/// Whether a CORS request from `origin` may read local-mode responses.
pub fn is_allowed_local_origin(origin: &HeaderValue) -> bool {
    origin.to_str().is_ok_and(allowed_origin)
}

fn check_local_request(headers: &HeaderMap) -> Result<(), &'static str> {
    // HTTP/1.1 browsers always send Host; only non-browser clients omit it.
    if let Some(host) = headers.get(header::HOST)
        && !host.to_str().is_ok_and(loopback_authority)
    {
        return Err("local backend only accepts loopback Host headers");
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && !is_allowed_local_origin(origin)
    {
        return Err("local backend only accepts requests from this machine's app");
    }
    Ok(())
}

fn allowed_origin(origin: &str) -> bool {
    if origin == PACKAGED_RENDERER_ORIGIN {
        return true;
    }
    let Ok(uri) = origin.parse::<Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri
            .path_and_query()
            .is_none_or(|path| path.as_str().is_empty() || path.as_str() == "/")
        && uri.host().is_some_and(loopback_host)
}

/// `host[:port]` as sent in the Host header.
fn loopback_authority(authority: &str) -> bool {
    authority
        .parse::<axum::http::uri::Authority>()
        .is_ok_and(|authority| loopback_host(authority.host()))
}

fn loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn accepts_the_app_and_non_browser_clients() {
        for pairs in [
            vec![],
            vec![(header::HOST, "127.0.0.1:60601")],
            vec![
                (header::HOST, "localhost:60601"),
                (header::ORIGIN, "http://localhost:5300"),
            ],
            vec![(header::HOST, "[::1]:60601"), (header::ORIGIN, "http://[::1]:5173")],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "http://127.0.0.1:60601"),
            ],
            vec![(header::HOST, "127.0.0.1:60601"), (header::ORIGIN, "file://")],
        ] {
            assert_eq!(check_local_request(&headers(&pairs)), Ok(()), "{pairs:?}");
        }
    }

    #[test]
    fn rejects_web_pages_and_rebinding_hosts() {
        for pairs in [
            vec![(header::HOST, "attacker.example:60601")],
            vec![(header::HOST, "192.168.0.10:60601")],
            vec![(header::HOST, "127.0.0.1.attacker.example")],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "https://evil.example"),
            ],
            vec![(header::HOST, "127.0.0.1:60601"), (header::ORIGIN, "null")],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "http://localhost.evil.example"),
            ],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "http://192.168.0.10:25808"),
            ],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "file:///C:/page.html"),
            ],
            vec![
                (header::HOST, "127.0.0.1:60601"),
                (header::ORIGIN, "chrome-extension://abc"),
            ],
        ] {
            assert!(check_local_request(&headers(&pairs)).is_err(), "{pairs:?}");
        }
    }

    #[test]
    fn cors_allows_only_app_origins() {
        assert!(is_allowed_local_origin(&HeaderValue::from_static(
            "http://127.0.0.1:5300"
        )));
        assert!(is_allowed_local_origin(&HeaderValue::from_static("file://")));
        assert!(!is_allowed_local_origin(&HeaderValue::from_static(
            "https://evil.example"
        )));
        assert!(!is_allowed_local_origin(&HeaderValue::from_static("null")));
    }
}
