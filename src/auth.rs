//! Password hashing (argon2), session cookies, API tokens, and the auth
//! middleware.
//!
//! Single-admin model: the admin account is created from `ADMIN_USER` /
//! `ADMIN_PASS` on first run (see [`crate::db`]). Browser requests carry a
//! random session token in an HttpOnly cookie; the token is looked up in the
//! `sessions` table on each request. Agents and scripts authenticate with a
//! named API token instead: `Authorization: Bearer cf_...`, validated
//! against salted hashes in the `api_tokens` table (or the
//! `CHIPFLOW_API_TOKEN` env var for headless bootstrap).

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::State;
use axum::{
    extract::Request,
    http::{
        header::{AUTHORIZATION, COOKIE, SET_COOKIE},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{thread_rng, RngCore};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::models::ApiTokenRecord;
use crate::AppState;

/// Authenticated user, inserted into request extensions by [`auth_middleware`].
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    /// True when this request authenticated with an API token rather than
    /// the browser session cookie. Token management endpoints require the
    /// cookie (a token must never be able to mint new tokens).
    pub via_api_token: bool,
}

/// Prefix identifying ChipFlow API tokens.
pub const API_TOKEN_PREFIX: &str = "cf_";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Generate a fresh API token: `cf_` + 43 chars of base64url (32 random bytes).
pub fn generate_api_token() -> String {
    let mut raw = [0u8; 32];
    thread_rng().fill_bytes(&mut raw);
    format!("{API_TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
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
    scopes: Vec<String>,
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
        scopes,
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
pub fn bearer_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Validate a Bearer token: env bootstrap first, then the DB table.
/// Returns the record (None for the env bootstrap token, which carries
/// full read+write scopes implicitly).
fn validate_bearer(db: &Db, raw: &str) -> Option<Option<ApiTokenRecord>> {
    // Headless bootstrap: a token from the environment, for first setup
    // and automation that runs before any token is created in the UI.
    if let Ok(env_token) = std::env::var("CHIPFLOW_API_TOKEN") {
        let env_token = env_token.trim();
        if !env_token.is_empty() && constant_time_eq(raw.as_bytes(), env_token.as_bytes()) {
            return Some(None);
        }
    }
    let record = db.find_api_token(raw).ok()??;
    if record.is_expired(chrono::Utc::now().timestamp()) {
        return None;
    }
    Some(Some(record))
}

/// Scope a request needs: reads for GET/HEAD/OPTIONS, writes otherwise.
fn required_scope(method: &Method) -> &'static str {
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => "read",
        _ => "write",
    }
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
        .map(|(id, username)| AuthUser {
            id,
            username,
            via_api_token: false,
        })
}

/// Create a session row and return the new token.
pub fn create_session(db: &Db, user_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    db.create_session(user_id)
}

/// `Set-Cookie` value for a fresh login.
pub fn session_cookie(token: &str) -> String {
    format!("session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000")
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
///   `CHIPFLOW_API_TOKEN` env var). Scoped: GET/HEAD/OPTIONS need `read`,
///   everything else needs `write`. Token management endpoints additionally
///   require the cookie — a token can never mint new tokens.
///
/// On first run (no users yet) everything redirects to `/setup` so the
/// admin account can be created in the browser instead of via env vars.
/// API routes (`/api/*`) get a plain 401/403 so htmx/fetch can detect it;
/// page routes redirect to `/login`.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // First run: no admin account exists yet. Send every request to the
    // setup page. (/setup itself is registered after this layer, so this
    // can't loop.) On DB error, assume setup is done and fall through to
    // the normal login flow.
    if state.db.user_count().unwrap_or(1) == 0 {
        return Redirect::to("/setup").into_response();
    }

    // API tokens first: explicit credential beats ambient cookie.
    if let Some(raw) = bearer_token_from_headers(req.headers()) {
        return match validate_bearer(&state.db, &raw) {
            Some(maybe_record) => {
                let allowed = match &maybe_record {
                    None => true, // env bootstrap token: full access
                    Some(record) => record.has_scope(required_scope(req.method())),
                };
                if !allowed {
                    return (StatusCode::FORBIDDEN, "insufficient token scope").into_response();
                }
                if let Some(record) = &maybe_record {
                    let _ = state
                        .db
                        .touch_api_token(&record.id, chrono::Utc::now().timestamp());
                }
                req.extensions_mut().insert(AuthUser {
                    id: "api-token".to_string(),
                    username: "api-token".to_string(),
                    via_api_token: true,
                });
                next.run(req).await
            }
            None => (StatusCode::UNAUTHORIZED, "invalid or expired API token").into_response(),
        };
    }

    let user = match session_token_from_headers(req.headers()) {
        Some(token) => user_for_token(&state.db, &token),
        None => None,
    };
    match user {
        Some(mut user) => {
            user.via_api_token = false;
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None if req.uri().path().starts_with("/api/") => {
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
        }
        None => Redirect::to("/login").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip_verifies() {
        let (record, raw) = new_api_token_record("test", vec!["read".into()], None);
        assert!(raw.starts_with(API_TOKEN_PREFIX));
        assert_eq!(raw.len(), API_TOKEN_PREFIX.len() + 43);
        assert!(verify_api_token(&record, &raw));
        assert!(!verify_api_token(&record, "cf_bogus"));
        // Tampered hash must not verify.
        let mut tampered = record.clone();
        tampered.token_hash = "00".repeat(32);
        assert!(!verify_api_token(&tampered, &raw));
    }

    #[test]
    fn token_last4_matches_tail() {
        let (record, raw) = new_api_token_record("test", vec!["write".into()], None);
        assert!(raw.ends_with(&record.last4));
        assert_eq!(record.last4.len(), 4);
    }

    #[test]
    fn scope_rules() {
        let read_only = ApiTokenRecord {
            id: "x".into(),
            name: "r".into(),
            prefix: "cf_".into(),
            last4: "abcd".into(),
            token_hash: "h".into(),
            salt: "s".into(),
            scopes: vec!["read".into()],
            created_at: 0,
            last_used_at: None,
            expires_at: None,
        };
        assert!(read_only.has_scope("read"));
        assert!(!read_only.has_scope("write"));
        let mut writer = read_only.clone();
        writer.scopes = vec!["write".into()];
        assert!(writer.has_scope("read")); // write implies read
        assert!(writer.has_scope("write"));
    }

    #[test]
    fn expiry_rules() {
        let now = 1_700_000_000i64;
        let mut record = ApiTokenRecord {
            id: "x".into(),
            name: "r".into(),
            prefix: "cf_".into(),
            last4: "abcd".into(),
            token_hash: "h".into(),
            salt: "s".into(),
            scopes: vec!["read".into()],
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
    fn bearer_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer cf_abc123"));
        assert_eq!(
            bearer_token_from_headers(&headers).as_deref(),
            Some("cf_abc123")
        );
        let empty = HeaderMap::new();
        assert_eq!(bearer_token_from_headers(&empty), None);
        let mut bad = HeaderMap::new();
        bad.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_token_from_headers(&bad), None);
    }
}
