//! Sign-in modes, session cookies, API tokens, and the auth middleware.
//!
//! Single-owner model with two browser sign-in modes. Under `--auth none` the
//! server listens on loopback only and every browser request is the owner's.
//! Under `--auth passkey` the owner signs in with a passkey, and the browser
//! carries a random session token in an HttpOnly cookie that is looked up in
//! the `sessions` table on each request. In both modes agents and scripts
//! authenticate with a named API token instead: `Authorization: Bearer
//! pomb_...`, validated against salted hashes in the `api_tokens` table (or
//! the `POMB_BOOTSTRAP_TOKEN` env var for headless bootstrap).

use std::fmt;
use std::net::IpAddr;

use axum::extract::{MatchedPath, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::{RngCore, thread_rng};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::api_error::{ErrorCode, coded_response};
use crate::db::PublicDb;
use crate::limit::{CredentialKey, rate_limited_response};
use crate::models::{AccountId, AccountRole, Actor, ApiTokenRecord, AuthScope, UserRow};
use crate::passkey::RelyingParty;

/// How a browser signs in, chosen with `--auth`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AuthMode {
    /// No sign-in: every browser request on loopback is the owner's.
    None,
    /// The owner signs in with a passkey.
    Passkey,
}

/// Whether the operator chose the sign-in mode or left it at its default.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AuthChoice {
    /// `--auth` or `POMB_AUTH` named this mode.
    Chosen(AuthMode),
    /// Neither was given, so the mode is `none`.
    Default,
}

impl fmt::Display for AuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "no sign-in",
            Self::Passkey => "passkey sign-in",
        })
    }
}

/// Returns the authority of a normalized `scheme://authority` URL.
fn url_authority(url: &str) -> &str {
    url.split_once("://")
        .map_or(url, |(_, authority)| authority)
}

/// The running sign-in mode, with the passkey relying party when the public URL gives one.
#[derive(Clone, Debug)]
pub enum SignIn {
    /// `--auth none`. A public URL still lets the owner add passkeys for a later passkey start.
    None(Option<RelyingParty>),
    /// `--auth passkey`.
    Passkey(RelyingParty),
}

/// Why the server cannot start in the chosen sign-in mode.
#[derive(Debug, PartialEq, Eq)]
pub enum SignInProblem {
    /// `--auth none` asked to listen on this address, which is not loopback.
    NotLoopback(String),
    /// `--auth passkey` without a public URL.
    MissingPublicUrl,
    /// The public URL cannot be the passkey relying party.
    RelyingParty(String),
    /// No `--auth` was given while this public URL, whose host is not loopback, was set.
    ImplicitNone(String),
}

impl fmt::Display for SignInProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback(host) => write!(
                f,
                "--auth none needs a loopback --host (127.0.0.1, ::1 or localhost), not {host}; \
                 use --auth passkey to listen on other addresses"
            ),
            Self::MissingPublicUrl => f.write_str(
                "--auth passkey needs POMB_PUBLIC_URL (or --public-url), whose host is the \
                 passkey relying party",
            ),
            Self::RelyingParty(problem) => write!(f, "the public URL {problem}"),
            Self::ImplicitNone(url) => write!(
                f,
                "POMB_PUBLIC_URL is {url}, which is not loopback, but no sign-in mode was \
                 chosen: set --auth passkey (POMB_AUTH=passkey), or --auth none \
                 (POMB_AUTH=none) if everyone who can reach that URL is trusted"
            ),
        }
    }
}

impl SignIn {
    /// Checks `mode` against the listen `host` and the normalized `public_url`.
    ///
    /// Under `--auth none` a public URL that cannot be a relying party only leaves passkeys
    /// unavailable, since the URL also serves the device-pairing link.
    ///
    /// The default mode also refuses a public URL whose host is not loopback, since a reverse
    /// proxy in front would hand the owner's powers to everyone it serves.
    pub fn new(
        choice: AuthChoice,
        host: &str,
        public_url: Option<&str>,
    ) -> Result<Self, SignInProblem> {
        let mode = match choice {
            AuthChoice::Chosen(mode) => mode,
            AuthChoice::Default => {
                if let Some(url) = public_url
                    && !is_loopback_host(url_authority(url))
                {
                    return Err(SignInProblem::ImplicitNone(url.to_string()));
                }
                AuthMode::None
            }
        };
        match mode {
            AuthMode::None if !is_loopback_host(host) => {
                Err(SignInProblem::NotLoopback(host.to_string()))
            }
            AuthMode::None => Ok(Self::None(
                public_url.and_then(|url| RelyingParty::new(url).ok()),
            )),
            AuthMode::Passkey => {
                let url = public_url.ok_or(SignInProblem::MissingPublicUrl)?;
                RelyingParty::new(url)
                    .map(Self::Passkey)
                    .map_err(SignInProblem::RelyingParty)
            }
        }
    }

    /// Returns the mode this sign-in runs.
    pub fn mode(&self) -> AuthMode {
        match self {
            Self::None(_) => AuthMode::None,
            Self::Passkey(_) => AuthMode::Passkey,
        }
    }

    /// Returns the passkey relying party, absent under `--auth none` without a public URL.
    pub fn relying_party(&self) -> Option<&RelyingParty> {
        match self {
            Self::None(relying_party) => relying_party.as_ref(),
            Self::Passkey(relying_party) => Some(relying_party),
        }
    }

    /// Returns whether `--auth none` serves a request naming this `Host`: a loopback name, or the
    /// public URL's host. Anything else is a page on another site that resolved its own name to
    /// this loopback address.
    fn serves_host(&self, host: &str) -> bool {
        let name = host_name(host);
        is_loopback_host(name)
            || self
                .relying_party()
                .is_some_and(|relying_party| relying_party.host().eq_ignore_ascii_case(name))
    }
}

/// Seconds a browser keeps the session cookie.
pub const SESSION_MAX_AGE_SECONDS: u32 = 30 * 24 * 60 * 60;

/// Username of the owner `--auth none` creates on an instance that has no user yet.
pub const DEFAULT_OWNER: &str = "owner";

/// Returns the host of a `Host` header value or listen address, without its port or brackets.
fn host_name(authority: &str) -> &str {
    let authority = authority.trim();
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    match authority.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => authority,
    }
}

/// Returns whether `host`, with or without a port, names this machine's loopback interface.
pub fn is_loopback_host(host: &str) -> bool {
    let name = host_name(host);
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Authenticated user, inserted into request extensions by [`auth_middleware`].
#[derive(Clone, Debug)]
pub struct AuthUser {
    /// The account the request acts for, which the data handles bind to.
    account_id: AccountId,
    /// The account's handle.
    pub handle: String,
    /// What the account may do beyond its own boards.
    pub role: AccountRole,
    /// The name history events record: the handle for the browser, `api-token` for a token.
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
    /// Returns the id of the account the request acts for.
    #[must_use]
    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

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

/// Builds the [`ApiTokenRecord`] for a fresh token of `account_id` (hashing it). The raw
/// token itself is returned alongside and must be shown to the user once —
/// it is never stored.
pub fn new_api_token_record(
    account_id: &AccountId,
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
        account_id: account_id.as_str().to_string(),
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

/// A validated Bearer credential and the account it acts for.
struct Bearer {
    /// The token's record, absent for the env bootstrap token, which carries full access.
    record: Option<ApiTokenRecord>,
    /// The account: the token's, or the instance's admin for the bootstrap token. Before the first
    /// account exists, the bootstrap token and ownerless tokens act for the ownerless rows.
    account: Option<UserRow>,
}

/// Why a Bearer token gets no user.
#[derive(Debug)]
enum BearerRefusal {
    /// The token is malformed, unknown, revoked, expired or for an account that does not exist.
    Invalid(ErrorCode),
    /// The database could not be read.
    Internal(String),
}

impl From<Box<dyn std::error::Error>> for BearerRefusal {
    fn from(error: Box<dyn std::error::Error>) -> Self {
        Self::Internal(error.to_string())
    }
}

impl IntoResponse for BearerRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::Invalid(code) => coded_response(
                StatusCode::UNAUTHORIZED,
                code,
                "invalid or expired API token",
            ),
            Self::Internal(message) => coded_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                message,
            ),
        }
    }
}

/// Validates a Bearer token: env bootstrap first, then the DB table. A token whose account does
/// not exist is invalid.
fn validate_bearer(db: &PublicDb, state: &AppState, raw: &str) -> Result<Bearer, BearerRefusal> {
    // Headless bootstrap: a token from the environment, for first setup
    // and automation that runs before any token is created in the UI.
    if let Some(env_token) = &state.bootstrap_token
        && env_token.matches(raw.as_bytes())
    {
        return Ok(Bearer {
            record: None,
            account: db.owner()?,
        });
    }
    let record = db
        .find_api_token(raw)?
        .ok_or(BearerRefusal::Invalid(ErrorCode::TokenInvalid))?;
    if record.is_expired(chrono::Utc::now().timestamp()) {
        return Err(BearerRefusal::Invalid(ErrorCode::TokenExpired));
    }
    let ownerless = record.account_id.is_empty();
    let account = match db.owner()? {
        None if ownerless => None,
        _ => Some(
            db.user(&record.account_id)?
                .ok_or(BearerRefusal::Invalid(ErrorCode::TokenInvalid))?,
        ),
    };
    Ok(Bearer {
        record: Some(record),
        account,
    })
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
    (Method::POST, "/api/v1/auth/passkeys", AuthScope::Admin),
    (
        Method::POST,
        "/api/v1/auth/passkeys/options",
        AuthScope::Admin,
    ),
    (
        Method::DELETE,
        "/api/v1/auth/passkeys/:id",
        AuthScope::Admin,
    ),
    (Method::POST, "/api/v1/backups", AuthScope::Admin),
    (Method::PUT, "/api/v1/backups/settings", AuthScope::Admin),
    (Method::POST, "/logout", AuthScope::Admin),
    (Method::POST, "/settings", AuthScope::Admin),
    (Method::POST, "/b/:board_id/settings", AuthScope::Admin),
    (Method::POST, "/device", AuthScope::Admin),
    (Method::POST, "/device/approve", AuthScope::Admin),
    (Method::POST, "/device/deny", AuthScope::Admin),
];

/// Matches `path` against `pattern`, where a `:name` or `{name}` segment matches any segment.
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
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

/// Pull the `session` cookie value out of request headers, if present.
pub fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("session=")
            .map(|value| value.to_string())
    })
}

/// Returns the browser user signed in as `account`, which holds full access.
fn browser_user(account: UserRow) -> AuthUser {
    AuthUser {
        account_id: account.account_id(),
        handle: account.username.clone(),
        role: account.role,
        username: account.username,
        kind: CredentialKind::Session,
        token_name: None,
        scope: AuthScope::Admin,
        expires_at: None,
    }
}

/// Looks up the user behind a session token.
pub fn user_for_token(db: &PublicDb, token: &str) -> Option<AuthUser> {
    db.user_for_token(token).ok()?.map(browser_user)
}

/// `Set-Cookie` value for a fresh login. The `Secure` attribute is set only when the login request
/// arrived over HTTPS: browsers silently drop a `Secure` cookie sent over plain HTTP, which would
/// break cookie auth on local dev servers.
pub fn session_cookie_secure(token: &str, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_MAX_AGE_SECONDS}{secure_attr}"
    )
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
/// - The browser. Under `--auth none` every request without a Bearer token is the owner's, as
///   long as its `Host` is loopback or the public URL's host. Under `--auth passkey` it needs the
///   session cookie a passkey sign-in set. Either way it has full access.
/// - `Authorization: Bearer` API token (named tokens in the DB, or the
///   `POMB_BOOTSTRAP_TOKEN` env var). Scoped: a route needs `read`, `work` or
///   `admin` as [`ROUTE_SCOPES`] says. Token management endpoints additionally
///   require the browser, so a token can never mint new tokens.
///
/// Under `--auth passkey` a browser without a session gets a plain 401 on API routes (`/api/*`), so
/// htmx/fetch can detect it, and a redirect on page routes: to `/setup` until the first passkey is
/// registered, then to `/login`. Bearer requests are validated as usual, so the
/// `POMB_BOOTSTRAP_TOKEN` bootstrap works before setup.
pub async fn auth_middleware(
    State(state): State<AppState>,
    matched: Option<MatchedPath>,
    mut req: Request,
    next: Next,
) -> Response {
    let needed = required_scope(
        req.method(),
        matched
            .as_ref()
            .map_or(req.uri().path(), MatchedPath::as_str),
    );

    // An explicit credential beats the ambient browser, and one that is not a Bearer token is no
    // credential at all
    let db = state.public();
    let now = chrono::Utc::now().timestamp();
    if req.headers().contains_key(AUTHORIZATION) {
        let validated = bearer_token_from_headers(req.headers())
            .ok_or(BearerRefusal::Invalid(ErrorCode::TokenInvalid))
            .and_then(|raw| validate_bearer(&db, &state, &raw));
        return match validated {
            Ok(Bearer {
                record: maybe_record,
                account,
            }) => {
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
                if let Some(record) = &maybe_record
                    && state.token_touch.claim_write(&record.id, now)
                {
                    let _ = db.touch_api_token(&record.id, now);
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
                let (account_id, handle, role) = match account {
                    Some(account) => (account.account_id(), account.username, account.role),
                    None => (AccountId::ownerless(), String::new(), AccountRole::Admin),
                };
                let role = match kind {
                    CredentialKind::Bootstrap => AccountRole::Admin,
                    _ => role,
                };
                req.extensions_mut().insert(AuthUser {
                    account_id,
                    handle,
                    role,
                    username: "api-token".to_string(),
                    kind,
                    token_name: maybe_record.map(|record| record.name),
                    scope,
                    expires_at,
                });
                next.run(req).await
            }
            Err(refusal) => refusal.into_response(),
        };
    }

    let user = match state.sign_in {
        SignIn::None(_) => owner_browser(&db, &state, req.headers()),
        SignIn::Passkey(_) => session_browser(&db, req.headers(), req.uri().path()),
    };
    match user {
        Ok(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        Err(refusal) => refusal.into_response(),
    }
}

/// Why a browser request without a Bearer token gets no user.
#[derive(Debug)]
enum BrowserRefusal {
    /// `--auth none` got a `Host` that is neither loopback nor the public URL's host.
    ForeignHost,
    /// The owner could not be loaded.
    Internal(String),
    /// An API route without a session.
    Unauthenticated,
    /// A page route before the first passkey exists.
    Setup,
    /// A page route without a session.
    SignIn,
}

impl IntoResponse for BrowserRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::ForeignHost => coded_response(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "without sign-in this server answers only to loopback host names",
            ),
            Self::Internal(message) => coded_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                message,
            ),
            Self::Unauthenticated => coded_response(
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthenticated,
                "unauthorized",
            ),
            Self::Setup => Redirect::to("/setup").into_response(),
            Self::SignIn => Redirect::to("/login").into_response(),
        }
    }
}

/// Returns the owner, for a browser request under `--auth none`.
fn owner_browser(
    db: &PublicDb,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthUser, BrowserRefusal> {
    if let Some(host) = headers.get(HOST)
        && !host
            .to_str()
            .is_ok_and(|host| state.sign_in.serves_host(host))
    {
        return Err(BrowserRefusal::ForeignHost);
    }
    let owner = db
        .ensure_owner(DEFAULT_OWNER)
        .map_err(|error| BrowserRefusal::Internal(error.to_string()))?;
    Ok(browser_user(owner))
}

/// Returns the signed-in user behind the session cookie, for a browser request under
/// `--auth passkey`.
fn session_browser(
    db: &PublicDb,
    headers: &HeaderMap,
    path: &str,
) -> Result<AuthUser, BrowserRefusal> {
    let session = session_token_from_headers(headers);
    match session
        .as_deref()
        .and_then(|token| user_for_token(db, token))
    {
        Some(user) => Ok(user),
        None if path.starts_with("/api/") => Err(BrowserRefusal::Unauthenticated),

        // Nobody can sign in before the first passkey exists, so setup comes first
        None if db.passkey_count().unwrap_or(1) == 0 => Err(BrowserRefusal::Setup),
        None => Err(BrowserRefusal::SignIn),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn token_should_round_trip_and_verify() {
        let (record, raw) =
            new_api_token_record(&AccountId::ownerless(), "test", AuthScope::Read, None);
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
        let (record, raw) =
            new_api_token_record(&AccountId::ownerless(), "test", AuthScope::Work, None);
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
            account_id: String::new(),
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
            account_id: String::new(),
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
            account_id: String::new(),
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

    #[test]
    fn none_mode_should_refuse_a_listen_address_that_is_not_loopback() {
        for host in ["0.0.0.0", "192.168.1.5", "::", "pomb.example.com"] {
            assert_eq!(
                SignIn::new(AuthChoice::Chosen(AuthMode::None), host, None)
                    .map(|sign_in| sign_in.mode()),
                Err(SignInProblem::NotLoopback(host.to_string())),
                "{host}"
            );
        }
        for host in [
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "[::1]",
            "localhost",
            "LocalHost",
        ] {
            assert_eq!(
                SignIn::new(AuthChoice::Chosen(AuthMode::None), host, None)
                    .map(|sign_in| sign_in.mode()),
                Ok(AuthMode::None),
                "{host}"
            );
        }
        let message = SignInProblem::NotLoopback("0.0.0.0".to_string()).to_string();
        assert!(message.contains("loopback"), "{message}");
    }

    #[test]
    fn passkey_mode_should_refuse_to_start_without_a_public_url() {
        assert_eq!(
            SignIn::new(AuthChoice::Chosen(AuthMode::Passkey), "0.0.0.0", None)
                .map(|sign_in| sign_in.mode()),
            Err(SignInProblem::MissingPublicUrl)
        );
        assert!(matches!(
            SignIn::new(
                AuthChoice::Chosen(AuthMode::Passkey),
                "0.0.0.0",
                Some("http://192.168.1.10:3000")
            ),
            Err(SignInProblem::RelyingParty(_))
        ));
        let sign_in = SignIn::new(
            AuthChoice::Chosen(AuthMode::Passkey),
            "0.0.0.0",
            Some("https://pomb.example.com"),
        )
        .expect("passkey mode");
        assert_eq!(sign_in.mode(), AuthMode::Passkey);
        assert!(
            SignInProblem::MissingPublicUrl
                .to_string()
                .contains("POMB_PUBLIC_URL")
        );
    }

    #[test]
    fn default_mode_should_refuse_a_public_url_beyond_loopback() {
        let start = |choice, url| SignIn::new(choice, "127.0.0.1", Some(url)).map(|s| s.mode());
        assert_eq!(
            start(AuthChoice::Default, "https://pomb.example.com"),
            Err(SignInProblem::ImplicitNone(
                "https://pomb.example.com".to_string()
            ))
        );
        assert_eq!(
            start(AuthChoice::Default, "http://127.0.0.1:3000"),
            Ok(AuthMode::None)
        );
        assert_eq!(
            start(AuthChoice::Default, "http://localhost:3000"),
            Ok(AuthMode::None)
        );
        assert_eq!(
            start(
                AuthChoice::Chosen(AuthMode::None),
                "https://pomb.example.com"
            ),
            Ok(AuthMode::None)
        );
        let message =
            SignInProblem::ImplicitNone("https://pomb.example.com".to_string()).to_string();
        assert!(message.contains("--auth passkey"), "{message}");
        assert!(message.contains("--auth none"), "{message}");
    }

    #[test]
    fn none_mode_should_keep_running_when_the_public_url_cannot_bind_passkeys() {
        let sign_in = SignIn::new(
            AuthChoice::Chosen(AuthMode::None),
            "127.0.0.1",
            Some("http://192.168.1.10:3000"),
        )
        .expect("none mode");
        assert!(sign_in.relying_party().is_none());
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
