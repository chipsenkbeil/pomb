//! Password hashing (argon2), session cookies, API tokens, and the auth
//! middleware.
//!
//! Single-admin model: the admin account is created from `ADMIN_USER` /
//! `ADMIN_PASS` on first run (see [`crate::db`]). Browser requests carry a
//! random session token in an HttpOnly cookie; the token is looked up in the
//! `sessions` table on each request. Agents and scripts authenticate with a
//! named API token instead: `Authorization: Bearer pomb_...`, validated
//! against salted hashes in the `api_tokens` table (or the
//! `POMB_BOOTSTRAP_TOKEN` env var for headless bootstrap).

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{MatchedPath, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::{RngCore, thread_rng};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};
use crate::db::Db;
use crate::limit::{CredentialKey, rate_limited_response};
use crate::models::{Actor, ApiTokenRecord, AuthScope};

/// Authenticated user, inserted into request extensions by [`auth_middleware`].
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub username: String,
    /// Credential the request authenticated with. Token management endpoints
    /// require the browser session, so a token can never mint new tokens.
    pub kind: CredentialKind,
    /// Name of the API token behind the request, absent for the browser
    /// session and for the `POMB_BOOTSTRAP_TOKEN` bootstrap token.
    pub token_name: Option<String>,
    /// Highest scope the credential holds.
    pub scope: AuthScope,
    /// Unix expiry of the credential, absent when it does not expire.
    pub expires_at: Option<i64>,
}

impl AuthUser {
    /// Returns the actor to record on history events for this request.
    #[must_use]
    pub fn actor(&self) -> Actor<'_> {
        Actor {
            name: &self.username,
            token_name: self.token_name.as_deref(),
        }
    }
}

/// The kind of credential a request authenticated with.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    /// The browser session cookie.
    Session,
    /// A named API token.
    Token,
    /// The `POMB_BOOTSTRAP_TOKEN` environment token.
    Bootstrap,
}

/// Prefix identifying Pomb API tokens.
pub const API_TOKEN_PREFIX: &str = "pomb_";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Generate a fresh API token: `pomb_` + 43 chars of base64url (32 random bytes).
pub fn generate_api_token() -> String {
    let mut raw = [0u8; 32];
    thread_rng().fill_bytes(&mut raw);
    format!("{API_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
}

/// Characters of a device-pairing user code: consonants only, so codes
/// spell no words and have no look-alike characters (RFC 8628 §6.1).
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

/// Length of a user code without its separator: 20^8 ≈ 2^34.6 codes.
const USER_CODE_LEN: usize = 8;

/// A device-pairing secret: 32 random bytes as base64url (43 chars).
pub fn generate_device_code() -> String {
    let mut raw = [0u8; 32];
    thread_rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// A device-pairing user code in canonical form, e.g. "BCDFGHJK".
pub fn generate_user_code() -> String {
    use rand::Rng;
    let mut rng = thread_rng();
    (0..USER_CODE_LEN)
        .map(|_| USER_CODE_ALPHABET[rng.gen_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect()
}

/// Canonical form of a typed user code: uppercase alphabet characters
/// only, so "bcdf-ghjk" and "BCDF GHJK" match "BCDFGHJK".
pub fn normalize_user_code(input: &str) -> String {
    input
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| USER_CODE_ALPHABET.contains(&(*c as u8)) && c.is_ascii())
        .collect()
}

/// A user code as shown to people: "BCDF-GHJK".
pub fn display_user_code(code: &str) -> String {
    let split = code.len().min(USER_CODE_LEN / 2);
    format!("{}-{}", &code[..split], &code[split..])
}

/// Unsalted hex(SHA-256(code)); device codes are long random secrets, so
/// the hash is the lookup key and the code itself is never stored.
pub fn hash_device_code(code: &str) -> String {
    hex(&Sha256::digest(code.as_bytes()))
}

/// Salted hash of a raw token: hex(SHA-256(salt || token)).
fn hash_api_token(raw: &str, salt_hex: &str) -> Option<String> {
    let salt = hex_decode(salt_hex)?;
    let mut hasher = Sha256::new();
    hasher.update(&salt);
    hasher.update(raw.as_bytes());
    Some(hex(&hasher.finalize()))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Build the [`ApiTokenRecord`] for a fresh token (hashing it). The raw
/// token itself is returned alongside and must be shown to the user once —
/// it is never stored.
pub fn new_api_token_record(
    name: &str,
    scope: AuthScope,
    expires_at: Option<i64>,
) -> (ApiTokenRecord, String) {
    let raw = generate_api_token();
    let mut salt = [0u8; 16];
    thread_rng().fill_bytes(&mut salt);
    let salt_hex = hex(&salt);
    let token_hash = hash_api_token(&raw, &salt_hex).expect("hex salt is valid");
    let record = ApiTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        prefix: API_TOKEN_PREFIX.to_string(),
        last4: raw
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect(),
        token_hash,
        salt: salt_hex,
        scope,
        created_at: chrono::Utc::now().timestamp(),
        last_used_at: None,
        expires_at,
    };
    (record, raw)
}

/// Verify a presented raw token against a stored record. Never panics.
pub fn verify_api_token(record: &ApiTokenRecord, raw: &str) -> bool {
    match hash_api_token(raw, &record.salt) {
        Some(candidate) => constant_time_eq(candidate.as_bytes(), record.token_hash.as_bytes()),
        None => false,
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pull the Bearer token from `Authorization: Bearer <token>`, if present.
/// The scheme name is matched case-insensitively (RFC 7235).
pub fn bearer_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_at_checked("Bearer ".len())?;
    if !scheme.eq_ignore_ascii_case("Bearer ") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Validate a Bearer token: env bootstrap first, then the DB table.
/// Returns the record (None for the env bootstrap token, which carries
/// full read+write scopes implicitly).
fn validate_bearer(state: &AppState, raw: &str) -> Result<Option<ApiTokenRecord>, ErrorCode> {
    // Headless bootstrap: a token from the environment, for first setup
    // and automation that runs before any token is created in the UI.
    if let Some(env_token) = &state.bootstrap_token
        && env_token.matches(raw.as_bytes())
    {
        return Ok(None);
    }
    let record = state
        .db
        .find_api_token(raw)
        .ok()
        .flatten()
        .ok_or(ErrorCode::TokenInvalid)?;
    if record.is_expired(chrono::Utc::now().timestamp()) {
        return Err(ErrorCode::TokenExpired);
    }
    Ok(Some(record))
}

/// The scope each route needs where it differs from the default, as (method, axum path, scope).
///
/// Every route behind the auth layer except the plain GETs is listed. A GET needs `read` unless
/// listed, and any other method needs `admin` unless listed, so a route nobody has classified yet
/// is admin-only. A test keeps this table and the router in step.
pub const ROUTE_SCOPES: &[(Method, &str, AuthScope)] = &[
    (
        Method::GET,
        "/api/boards/:id/calendar-feed",
        AuthScope::Admin,
    ),
    (
        Method::POST,
        "/api/boards/:id/calendar-feed",
        AuthScope::Admin,
    ),
    (
        Method::POST,
        "/api/boards/:id/calendar-feed/reset",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/tasks", AuthScope::Work),
    (Method::PATCH, "/api/tasks/:id", AuthScope::Work),
    (Method::DELETE, "/api/tasks/:id", AuthScope::Admin),
    (Method::POST, "/api/tasks/:id/move", AuthScope::Work),
    (Method::POST, "/api/tasks/:id/watch", AuthScope::Work),
    (Method::POST, "/api/tasks/:id/time", AuthScope::Work),
    (Method::POST, "/api/tasks/:id/subtasks", AuthScope::Work),
    (
        Method::PUT,
        "/api/tasks/:id/subtasks/order",
        AuthScope::Admin,
    ),
    (
        Method::PATCH,
        "/api/tasks/:id/subtasks/:sub_id",
        AuthScope::Work,
    ),
    (
        Method::DELETE,
        "/api/tasks/:id/subtasks/:sub_id",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/tasks/:id/links", AuthScope::Work),
    (
        Method::DELETE,
        "/api/tasks/:id/links/:link_id",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/tasks/:id/claim", AuthScope::Work),
    (
        Method::POST,
        "/api/tasks/:id/claim/heartbeat",
        AuthScope::Work,
    ),
    (
        Method::POST,
        "/api/tasks/:id/claim/release",
        AuthScope::Work,
    ),
    (Method::POST, "/api/tasks/:id/comments", AuthScope::Work),
    (
        Method::DELETE,
        "/api/tasks/:id/comments/:comment_id",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/tasks/:id/attachments", AuthScope::Work),
    (
        Method::DELETE,
        "/api/tasks/:id/attachments/:attachment_id",
        AuthScope::Admin,
    ),
    (Method::PUT, "/api/time/entries/:id", AuthScope::Admin),
    (Method::DELETE, "/api/time/entries/:id", AuthScope::Admin),
    (Method::POST, "/api/time/manual", AuthScope::Work),
    (Method::POST, "/api/timer/start", AuthScope::Work),
    (Method::POST, "/api/timer/stop", AuthScope::Work),
    (Method::POST, "/api/timer/retarget", AuthScope::Work),
    (Method::PUT, "/api/settings", AuthScope::Admin),
    (Method::POST, "/api/columns", AuthScope::Provision),
    (Method::PATCH, "/api/columns/:id", AuthScope::Admin),
    (Method::DELETE, "/api/columns/:id", AuthScope::Admin),
    (Method::POST, "/api/columns/:id/move", AuthScope::Admin),
    (Method::POST, "/api/swimlanes", AuthScope::Provision),
    (Method::PATCH, "/api/swimlanes/:id", AuthScope::Admin),
    (Method::DELETE, "/api/swimlanes/:id", AuthScope::Admin),
    (Method::POST, "/api/swimlanes/:id/move", AuthScope::Admin),
    (Method::POST, "/api/boards", AuthScope::Provision),
    (Method::DELETE, "/api/boards/:id", AuthScope::Admin),
    (Method::PUT, "/api/boards/:id/config", AuthScope::Admin),
    (Method::POST, "/api/boards/:id/fields", AuthScope::Provision),
    (Method::PATCH, "/api/fields/:id", AuthScope::Provision),
    (Method::DELETE, "/api/fields/:id", AuthScope::Admin),
    (
        Method::POST,
        "/api/boards/:id/save-as-template",
        AuthScope::Admin,
    ),
    (Method::DELETE, "/api/templates/:id", AuthScope::Admin),
    (Method::POST, "/api/boards/:id/colors", AuthScope::Admin),
    (
        Method::PATCH,
        "/api/boards/:id/colors/:color_id",
        AuthScope::Admin,
    ),
    (
        Method::DELETE,
        "/api/boards/:id/colors/:color_id",
        AuthScope::Admin,
    ),
    (
        Method::POST,
        "/api/boards/:id/colors/copy-from",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/v1/auth/tokens", AuthScope::Admin),
    (Method::DELETE, "/api/v1/auth/tokens/:id", AuthScope::Admin),
    (Method::POST, "/logout", AuthScope::Admin),
    (Method::POST, "/settings", AuthScope::Admin),
    (Method::POST, "/b/:board_id/settings", AuthScope::Admin),
    (Method::POST, "/device", AuthScope::Admin),
    (Method::POST, "/device/approve", AuthScope::Admin),
    (Method::POST, "/device/deny", AuthScope::Admin),
];

/// Matches `path` against `pattern`, where a `:name` or `{name}` segment matches any segment.
fn path_matches(pattern: &str, path: &str) -> bool {
    let is_param = |segment: &str| segment.starts_with(':') || segment.starts_with('{');
    let mut pattern_segments = pattern.split('/');
    let mut path_segments = path.split('/');
    loop {
        match (pattern_segments.next(), path_segments.next()) {
            (None, None) => return true,
            (Some(want), Some(got)) if want == got || (is_param(want) && is_param(got)) => {}
            _ => return false,
        }
    }
}

/// Looks up the scope [`ROUTE_SCOPES`] lists for this route, where HEAD counts as GET, returning
/// [`None`] when the route is not listed.
#[must_use]
pub fn listed_scope(method: &Method, matched_path: &str) -> Option<AuthScope> {
    let method = if *method == Method::HEAD {
        &Method::GET
    } else {
        method
    };
    ROUTE_SCOPES
        .iter()
        .find(|(listed, pattern, _)| listed == method && path_matches(pattern, matched_path))
        .map(|&(_, _, scope)| scope)
}

/// Returns the scope a request needs: the listed one, else `read` for GET, HEAD and OPTIONS and
/// `admin` for every other method.
#[must_use]
pub fn required_scope(method: &Method, matched_path: &str) -> AuthScope {
    listed_scope(method, matched_path).unwrap_or(match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => AuthScope::Read,
        _ => AuthScope::Admin,
    })
}

/// Hash a password with argon2id and a random salt; returns the PHC string.
pub fn hash_password(password: &str) -> Result<String, Box<dyn std::error::Error>> {
    let salt = SaltString::generate(&mut thread_rng());
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| format!("password hashing failed: {error}"))?;
    Ok(hash.to_string())
}

/// Verify a password against a stored PHC hash. Never panics on bad input.
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Pull the `session` cookie value out of request headers, if present.
pub fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("session=")
            .map(|value| value.to_string())
    })
}

/// Look up the user behind a session token.
pub fn user_for_token(db: &Db, token: &str) -> Option<AuthUser> {
    db.user_for_token(token)
        .ok()?
        .map(|(_id, username)| AuthUser {
            username,
            kind: CredentialKind::Session,
            token_name: None,
            scope: AuthScope::Admin,
            expires_at: None,
        })
}

/// Create a session row and return the new token.
pub fn create_session(db: &Db, user_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    db.create_session(user_id)
}

/// `Set-Cookie` value for a fresh login. The `Secure` attribute is set only when the login request
/// arrived over HTTPS: browsers silently drop a `Secure` cookie sent over plain HTTP, which would
/// break cookie auth on local dev servers.
pub fn session_cookie_secure(token: &str, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!("session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000{secure_attr}")
}

/// True when the request arrived over TLS, either directly or via a TLS-terminating proxy that
/// marks the hop with `X-Forwarded-Proto` / the `Forwarded` header.
pub fn request_is_https(headers: &HeaderMap) -> bool {
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let forwarded_header = headers
        .get("forwarded")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("proto=https"))
        .unwrap_or(false);
    forwarded_proto || forwarded_header
}

/// `Set-Cookie` value that clears the session cookie (logout).
pub fn clear_session_cookie() -> String {
    "session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string()
}

fn set_cookie_header(response: &mut Response, value: &str) {
    if let Ok(header_value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(SET_COOKIE, header_value);
    }
}

/// Attach a session cookie to a redirect response (used by login/logout).
pub fn redirect_with_cookie(target: &str, cookie: &str) -> Response {
    let mut response = Redirect::to(target).into_response();
    set_cookie_header(&mut response, cookie);
    response
}

/// Middleware guarding every route except `/login`, `/setup`, and `/static`
/// (those are registered after this layer, so it never sees them).
///
/// Two credential types are accepted:
/// - Browser session cookie (the admin login). Full access.
/// - `Authorization: Bearer` API token (named tokens in the DB, or the
///   `POMB_BOOTSTRAP_TOKEN` env var). Scoped: a route needs `read`, `work` or
///   `admin` as [`ROUTE_SCOPES`] says. Token management endpoints additionally
///   require the cookie — a token can never mint new tokens.
///
/// On first run (no users yet) every request without a Bearer token redirects to `/setup` so the
/// admin account can be created in the browser. Bearer requests are validated as usual, so the
/// `POMB_BOOTSTRAP_TOKEN` bootstrap works before setup. API routes (`/api/*`) get a plain 401/403 so
/// htmx/fetch can detect it, and page routes redirect to `/login`.
pub async fn auth_middleware(
    State(state): State<AppState>,
    matched: Option<MatchedPath>,
    mut req: Request,
    next: Next,
) -> Response {
    // First run with no admin yet: send non-Bearer requests to setup
    // A DB error counts as setup done, and /setup sits after this layer so it cannot loop
    let bearer = bearer_token_from_headers(req.headers());
    if bearer.is_none() && state.db.user_count().unwrap_or(1) == 0 {
        return Redirect::to("/setup").into_response();
    }

    let needed = required_scope(
        req.method(),
        matched
            .as_ref()
            .map_or(req.uri().path(), MatchedPath::as_str),
    );

    // API tokens first: explicit credential beats ambient cookie
    if let Some(raw) = bearer {
        return match validate_bearer(&state, &raw) {
            Ok(maybe_record) => {
                let allowed = match &maybe_record {
                    None => true, // env bootstrap token: full access
                    Some(record) => record.has_scope(needed),
                };
                let credential = match &maybe_record {
                    Some(record) => CredentialKey::Token(record.id.clone()),
                    None => CredentialKey::Bootstrap,
                };
                if let Err(refusal) = state.limits.check_credential(&credential) {
                    return rate_limited_response(refusal);
                }
                if !allowed {
                    return coded_response(
                        StatusCode::FORBIDDEN,
                        ErrorCode::InsufficientScope,
                        format!("insufficient token scope: needs {}", needed.as_str()),
                    );
                }
                if let Some(record) = &maybe_record {
                    let now = chrono::Utc::now().timestamp();
                    if state.token_touch.claim_write(&record.id, now) {
                        let _ = state.db.touch_api_token(&record.id, now);
                    }
                }
                let scope = match &maybe_record {
                    Some(record) => record.scope,
                    None => AuthScope::Admin,
                };
                let kind = match &maybe_record {
                    Some(_) => CredentialKind::Token,
                    None => CredentialKind::Bootstrap,
                };
                let expires_at = maybe_record.as_ref().and_then(|record| record.expires_at);
                req.extensions_mut().insert(AuthUser {
                    username: "api-token".to_string(),
                    kind,
                    token_name: maybe_record.map(|record| record.name),
                    scope,
                    expires_at,
                });
                next.run(req).await
            }
            Err(code) => coded_response(
                StatusCode::UNAUTHORIZED,
                code,
                "invalid or expired API token",
            ),
        };
    }

    let session = session_token_from_headers(req.headers());
    let user = session
        .as_deref()
        .and_then(|token| user_for_token(&state.db, token));
    match user {
        Some(mut user) => {
            user.kind = CredentialKind::Session;
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None if req.uri().path().starts_with("/api/") => coded_response(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthenticated,
            "unauthorized",
        ),
        None => Redirect::to("/login").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_should_round_trip_and_verify() {
        let (record, raw) = new_api_token_record("test", AuthScope::Read, None);
        assert!(raw.starts_with(API_TOKEN_PREFIX));
        assert_eq!(raw.len(), API_TOKEN_PREFIX.len() + 43);
        assert!(verify_api_token(&record, &raw));
        assert!(!verify_api_token(&record, "pomb_bogus"));

        // Tampered hash must not verify
        let mut tampered = record.clone();
        tampered.token_hash = "00".repeat(32);
        assert!(!verify_api_token(&tampered, &raw));
    }

    #[test]
    fn token_last4_should_match_tail() {
        let (record, raw) = new_api_token_record("test", AuthScope::Work, None);
        assert!(raw.ends_with(&record.last4));
        assert_eq!(record.last4.len(), 4);
    }

    #[test]
    fn legacy_cf_token_should_still_authenticate() {
        let raw = "cf_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
        let salt_hex = "00112233445566778899aabbccddeeff";
        let record = ApiTokenRecord {
            id: "legacy".into(),
            name: "legacy-bot".into(),
            prefix: "cf_".into(),
            last4: "DEFG".into(),
            token_hash: hash_api_token(raw, salt_hex).expect("hex salt is valid"),
            salt: salt_hex.into(),
            scope: AuthScope::Read,
            created_at: 0,
            last_used_at: None,
            expires_at: None,
        };
        assert!(!raw.starts_with(API_TOKEN_PREFIX));
        assert!(verify_api_token(&record, raw));

        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::connect(dir.path().join("legacy.redb").to_str().expect("utf8 path"))
            .expect("connect");
        db.create_api_token(&record).expect("store token");
        let found = db
            .find_api_token(raw)
            .expect("lookup")
            .expect("token found");
        assert_eq!(found.id, "legacy");
    }

    fn record_with(scope: AuthScope) -> ApiTokenRecord {
        ApiTokenRecord {
            id: "x".into(),
            name: "r".into(),
            prefix: "pomb_".into(),
            last4: "abcd".into(),
            token_hash: "h".into(),
            salt: "s".into(),
            scope,
            created_at: 0,
            last_used_at: None,
            expires_at: None,
        }
    }

    #[test]
    fn scope_should_include_the_scopes_below_it_and_not_those_above() {
        for (held, grants) in [
            (AuthScope::Read, [true, false, false]),
            (AuthScope::Work, [true, true, false]),
            (AuthScope::Admin, [true, true, true]),
        ] {
            let record = record_with(held);
            for (scope, grant) in AuthScope::ALL.into_iter().zip(grants) {
                assert_eq!(record.has_scope(scope), grant, "{held:?} vs {scope:?}");
            }
        }
    }

    #[test]
    fn highest_should_pick_the_top_scope_and_refuse_unknown_names() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            AuthScope::highest(&names(&["read", "work", "read"])),
            Some(AuthScope::Work)
        );
        assert_eq!(AuthScope::highest(&names(&[])), None);
        assert_eq!(AuthScope::highest(&names(&["read", "write"])), None);
    }

    #[test]
    fn required_scope_should_default_gets_to_read_and_the_rest_to_admin() {
        let scope = |method: Method, path| required_scope(&method, path);
        assert_eq!(scope(Method::GET, "/api/tasks/:id"), AuthScope::Read);
        assert_eq!(scope(Method::HEAD, "/api/tasks/:id"), AuthScope::Read);
        assert_eq!(scope(Method::POST, "/api/tasks"), AuthScope::Work);
        assert_eq!(scope(Method::POST, "/api/not-classified"), AuthScope::Admin);
        assert_eq!(scope(Method::DELETE, "/api/boards/:id"), AuthScope::Admin);
        assert_eq!(
            scope(Method::GET, "/api/boards/:id/calendar-feed"),
            AuthScope::Admin
        );
        assert_eq!(
            scope(Method::HEAD, "/api/boards/{id}/calendar-feed"),
            AuthScope::Admin
        );
    }

    /// Every (method, path) the router registers behind the auth layer, read from its source.
    fn registered_routes() -> Vec<(String, String)> {
        let source = include_str!("routes.rs");
        let start = source.find("pub fn router(").expect("router fn");
        let end = start + source[start..].find(".route_layer(").expect("route_layer");
        let body = &source[start..end];
        let mut found = Vec::new();
        for call in body.split(".route(").skip(1) {
            let path = call.split('"').nth(1).expect("route path");
            let handlers = &call[call.find('"').unwrap() + path.len() + 2..];
            for method in ["get", "post", "put", "patch", "delete"] {
                let needle = format!("{method}(");
                let listed = handlers.match_indices(&needle).any(|(at, _)| {
                    handlers[..at]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
                });
                if listed {
                    found.push((method.to_uppercase(), path.to_string()));
                }
            }
        }
        found
    }

    #[test]
    fn route_scopes_should_cover_every_route_behind_the_auth_layer() {
        let routes = registered_routes();
        assert!(routes.len() > 100, "parsed only {} routes", routes.len());
        for (method, path) in &routes {
            let listed = ROUTE_SCOPES
                .iter()
                .any(|(m, pattern, _)| m.as_str() == method && pattern == path);
            assert!(
                listed || method == "GET",
                "{method} {path} has no ROUTE_SCOPES entry; unlisted routes fall back to admin"
            );
        }
        for (method, pattern, _) in ROUTE_SCOPES {
            assert!(
                routes
                    .iter()
                    .any(|(m, p)| m.as_str() == method.as_str() && p == pattern),
                "ROUTE_SCOPES lists {method} {pattern}, which the router does not register"
            );
        }
    }

    #[test]
    fn route_scopes_should_put_the_work_routes_at_work() {
        let work: Vec<_> = ROUTE_SCOPES
            .iter()
            .filter(|(_, _, scope)| *scope == AuthScope::Work)
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect();
        assert_eq!(
            work,
            [
                "POST /api/tasks",
                "PATCH /api/tasks/:id",
                "POST /api/tasks/:id/move",
                "POST /api/tasks/:id/watch",
                "POST /api/tasks/:id/time",
                "POST /api/tasks/:id/subtasks",
                "PATCH /api/tasks/:id/subtasks/:sub_id",
                "POST /api/tasks/:id/links",
                "POST /api/tasks/:id/claim",
                "POST /api/tasks/:id/claim/heartbeat",
                "POST /api/tasks/:id/claim/release",
                "POST /api/tasks/:id/comments",
                "POST /api/tasks/:id/attachments",
                "POST /api/time/manual",
                "POST /api/timer/start",
                "POST /api/timer/stop",
                "POST /api/timer/retarget",
            ]
        );
    }

    #[test]
    fn token_should_expire_only_after_its_expires_at() {
        let now = 1_700_000_000i64;
        let mut record = ApiTokenRecord {
            id: "x".into(),
            name: "r".into(),
            prefix: "pomb_".into(),
            last4: "abcd".into(),
            token_hash: "h".into(),
            salt: "s".into(),
            scope: AuthScope::Read,
            created_at: 0,
            last_used_at: None,
            expires_at: None,
        };
        assert!(!record.is_expired(now));
        record.expires_at = Some(now + 60);
        assert!(!record.is_expired(now));
        record.expires_at = Some(now - 1);
        assert!(record.is_expired(now));
    }

    #[test]
    fn bearer_should_parse_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer pomb_abc123"),
        );
        assert_eq!(
            bearer_token_from_headers(&headers).as_deref(),
            Some("pomb_abc123")
        );
        let empty = HeaderMap::new();
        assert_eq!(bearer_token_from_headers(&empty), None);
        let mut bad = HeaderMap::new();
        bad.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_token_from_headers(&bad), None);
    }

    #[test]
    fn should_accept_bearer_scheme_in_any_case() {
        for scheme in ["bearer", "BEARER", "BeArEr"] {
            let mut headers = HeaderMap::new();
            let value = format!("{scheme} pomb_abc123");
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&value).unwrap());
            assert_eq!(
                bearer_token_from_headers(&headers).as_deref(),
                Some("pomb_abc123"),
                "{scheme}"
            );
        }
    }

    #[test]
    fn should_reject_malformed_authorization_headers() {
        for value in [
            "bearer",
            "BEARER ",
            "Basic pomb_abc123",
            "Bearercf_abc123",
            "é",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(bearer_token_from_headers(&headers), None, "{value}");
        }
    }

    /// `Secure` is only present when asked for, because a `Secure` cookie sent over plain HTTP is
    /// dropped by the browser.
    #[test]
    fn session_cookie_should_set_secure_flag_only_when_asked() {
        let plain = session_cookie_secure("tok", false);
        assert!(!plain.contains("Secure"));
        assert!(plain.contains("HttpOnly"));
        let https = session_cookie_secure("tok", true);
        assert!(https.contains("; Secure"));
    }

    /// HTTPS detection honors the TLS-terminating proxy headers.
    #[test]
    fn request_should_be_https_from_proxy_headers() {
        assert!(!request_is_https(&HeaderMap::new()));
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!request_is_https(&headers));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(request_is_https(&headers));

        // First entry wins on a comma list (standard proxy behavior)
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(request_is_https(&headers));
        let mut forwarded = HeaderMap::new();
        forwarded.insert(
            "forwarded",
            HeaderValue::from_static("for=1.2.3.4;proto=https;by=9.9.9.9"),
        );
        assert!(request_is_https(&forwarded));
    }
}
