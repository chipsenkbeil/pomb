//! KF-420: refuse cross-origin writes that would ride the session cookie.
//!
//! `SameSite=Lax` only stops cross-SITE requests, and production shares the
//! site `chip.network` with sibling services, so a page on any
//! `*.chip.network` host could otherwise auto-submit a form here with the
//! admin's cookie attached. This layer wraps the whole router, so every
//! route (including `/login`, `/setup`, and routes added later) is covered
//! without per-route code. Bearer-token requests are not cookie-based and
//! pass through untouched.

use axum::{
    extract::Request,
    http::{header::HOST, HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::auth;

/// Body of the 403 sent for a refused request.
pub const REFUSED_BODY: &str = "cross-site request refused";

/// Middleware that answers 403 to a cross-origin state-changing browser
/// request.
pub async fn refuse_cross_origin_writes(req: Request, next: Next) -> Response {
    if is_cross_origin_write(req.method(), req.headers(), req.uri()) {
        return (StatusCode::FORBIDDEN, REFUSED_BODY).into_response();
    }
    next.run(req).await
}

/// True when a browser marked this non-Bearer write as coming from another
/// origin.
///
/// `Sec-Fetch-Site` decides when present (it does not depend on the proxy's
/// Host handling); otherwise `Origin` must equal this server's own origin.
/// Requests carrying neither header (curl, scripts) are not browser CSRF
/// and are allowed.
fn is_cross_origin_write(method: &Method, headers: &HeaderMap, uri: &Uri) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return false;
    }
    if auth::bearer_token_from_headers(headers).is_some() {
        return false;
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        return !matches!(site.to_str(), Ok("same-origin" | "none"));
    }
    match headers.get("origin") {
        Some(origin) => match (origin.to_str(), own_origin(headers, uri)) {
            (Ok(origin), Some(own)) => normalize_origin(origin) != own,
            _ => true,
        },
        None => false,
    }
}

/// This server's origin as the browser sees it: the scheme from the TLS
/// proxy headers (the same rule as the cookie `Secure` flag) plus Host.
fn own_origin(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = match headers.get(HOST) {
        Some(host) => host.to_str().ok()?,
        None => uri.authority()?.as_str(),
    };
    let scheme = if auth::request_is_https(headers) {
        "https"
    } else {
        "http"
    };
    Some(normalize_origin(&format!("{scheme}://{host}")))
}

/// Lowercase an origin and drop the scheme's default port, so
/// `https://Host:443` and `https://host` compare equal.
fn normalize_origin(origin: &str) -> String {
    let origin = origin.trim().to_ascii_lowercase();
    for (prefix, default_port) in [("https://", ":443"), ("http://", ":80")] {
        if origin.starts_with(prefix) {
            if let Some(stripped) = origin.strip_suffix(default_port) {
                return stripped.to_string();
            }
        }
    }
    origin
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn post_is_refused(pairs: &[(&'static str, &'static str)]) -> bool {
        is_cross_origin_write(
            &Method::POST,
            &headers(pairs),
            &Uri::from_static("/settings"),
        )
    }

    #[test]
    fn normalize_origin_should_drop_default_ports_and_case() {
        assert_eq!(
            normalize_origin("https://Chipflow.Chip.Network:443"),
            "https://chipflow.chip.network"
        );
        assert_eq!(normalize_origin("http://127.0.0.1:80"), "http://127.0.0.1");
        assert_eq!(
            normalize_origin("http://127.0.0.1:3390"),
            "http://127.0.0.1:3390"
        );
        assert_eq!(normalize_origin("https://host:80"), "https://host:80");
    }

    #[test]
    fn cross_origin_write_should_respect_the_proxy_scheme() {
        let behind_caddy = [
            ("host", "chipflow.chip.network"),
            ("x-forwarded-proto", "https"),
            ("origin", "https://chipflow.chip.network"),
        ];
        assert!(!post_is_refused(&behind_caddy));
        let sibling = [
            ("host", "chipflow.chip.network"),
            ("x-forwarded-proto", "https"),
            ("origin", "https://other.chip.network"),
        ];
        assert!(post_is_refused(&sibling));
        let scheme_mismatch = [
            ("host", "chipflow.chip.network"),
            ("origin", "https://chipflow.chip.network"),
        ];
        assert!(post_is_refused(&scheme_mismatch));
    }

    #[test]
    fn cross_origin_write_should_let_sec_fetch_site_decide() {
        assert!(!post_is_refused(&[("sec-fetch-site", "same-origin")]));
        assert!(!post_is_refused(&[("sec-fetch-site", "none")]));
        assert!(post_is_refused(&[("sec-fetch-site", "same-site")]));
        assert!(post_is_refused(&[("sec-fetch-site", "cross-site")]));
        assert!(post_is_refused(&[("sec-fetch-site", "bogus")]));
    }

    #[test]
    fn cross_origin_write_should_ignore_safe_methods() {
        let map = headers(&[("sec-fetch-site", "cross-site"), ("origin", "null")]);
        let uri = Uri::from_static("/settings");
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_cross_origin_write(&method, &map, &uri));
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_cross_origin_write(&method, &map, &uri));
        }
    }
}
