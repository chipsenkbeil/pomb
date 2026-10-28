//! Stable error codes and the JSON error body that Bearer clients receive.
//!
//! Handlers keep returning plain-text errors, tagged with an [`ErrorCode`] in the response
//! extensions. [`json_errors`] wraps the whole router and rewrites those responses into an
//! [`ErrorBody`] for Bearer requests and for `/api/*` requests with no cookie. Requests that carry
//! the session cookie keep the plain text bodies, because the browser UI shows them as they are.

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, COOKIE, LINK, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth;

const SPEC_PATH: &str = "/api/v1/openapi.json";
const HELP_PATH: &str = "/llms.txt";
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

/// Stable machine-readable reason for an error response.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is malformed or fails validation (400).
    InvalidRequest,
    /// A column or swimlane create names no board (400).
    BoardIdRequired,
    /// The device code is unknown or already used (400).
    InvalidGrant,
    /// The handle, or the recovery code sent with it, matches no account (400).
    InvalidRecovery,
    /// No credential was sent (401).
    Unauthenticated,
    /// The API token is unknown or revoked (401).
    TokenInvalid,
    /// The API token is past its expiry (401).
    TokenExpired,
    /// The admin suspended the account, so it cannot sign in and its tokens stopped working (401).
    AccountSuspended,
    /// The credential is valid but not allowed this action (403).
    Forbidden,
    /// The token's scope does not cover the request method (403).
    InsufficientScope,
    /// The endpoint needs the browser session, not an API token (403).
    SessionRequired,
    /// The action needs a passkey check within the last 5 minutes (403).
    ReauthRequired,
    /// The device pairing was denied (403).
    AccessDenied,
    /// The account waits for the admin to approve it, and reaches nothing until then (403).
    AccountPending,
    /// The instance takes no requests to join: only an invite link registers an account (403).
    RegistrationClosed,
    /// The resource does not exist (404).
    NotFound,
    /// The path matches no API route (404).
    RouteNotFound,
    /// The path exists but not for this HTTP method (405).
    MethodNotAllowed,
    /// The request conflicts with current state (409).
    Conflict,
    /// A live claim by another holder blocks the request (409).
    ClaimHeld,
    /// The claim fence is stale, mismatched or expired (409).
    StaleFence,
    /// The target column is at its WIP limit (409).
    WipLimitReached,
    /// The board already has a field with that key (409).
    FieldKeyExists,
    /// Another account already has that handle (409).
    HandleTaken,
    /// The admin cannot delete its account while other accounts exist (409).
    AdminHasAccounts,
    /// The write would take the account past one of the quotas the admin set (409).
    QuotaExceeded,
    /// The resource is gone for good (410).
    Gone,
    /// The cursor is older than the board events kept, or newer than any written (410).
    CursorExpired,
    /// The device code expired (410).
    DeviceCodeExpired,
    /// The request body is too large (413).
    PayloadTooLarge,
    /// The Content-Type is not supported (415).
    UnsupportedMediaType,
    /// The body parsed but its content is unacceptable (422).
    UnprocessableEntity,
    /// The request needs a precondition it did not send (428).
    PreconditionRequired,
    /// The device pairing is not approved yet (428).
    AuthorizationPending,
    /// Too many requests (429).
    RateLimited,
    /// As many accounts as the instance's cap already wait for approval, so a request to join is
    /// refused (429).
    PendingFull,
    /// The device is polling too fast (429).
    SlowDown,
    /// The API token or the account already holds as many open waits for board events as it may
    /// (429).
    TooManyWaits,
    /// The server holds as many of these requests as it takes at once: passkey ceremonies (429)
    /// or attachment uploads (503). Retry later.
    Busy,
    /// The request took longer than the server allows (408).
    RequestTimeout,
    /// The database or its disk is full, so writes that grow it stop until the admin makes room
    /// (507).
    StorageFull,
    /// The server failed (5xx).
    InternalError,
}

impl ErrorCode {
    /// Picks the code for a status when the handler named none.
    #[must_use]
    pub fn for_status(status: StatusCode) -> Self {
        match status {
            StatusCode::BAD_REQUEST => Self::InvalidRequest,
            StatusCode::UNAUTHORIZED => Self::Unauthenticated,
            StatusCode::FORBIDDEN => Self::Forbidden,
            StatusCode::NOT_FOUND => Self::NotFound,
            StatusCode::METHOD_NOT_ALLOWED => Self::MethodNotAllowed,
            StatusCode::CONFLICT => Self::Conflict,
            StatusCode::GONE => Self::Gone,
            StatusCode::PAYLOAD_TOO_LARGE => Self::PayloadTooLarge,
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Self::UnsupportedMediaType,
            StatusCode::UNPROCESSABLE_ENTITY => Self::UnprocessableEntity,
            StatusCode::PRECONDITION_REQUIRED => Self::PreconditionRequired,
            StatusCode::TOO_MANY_REQUESTS => Self::RateLimited,
            StatusCode::REQUEST_TIMEOUT => Self::RequestTimeout,
            status if status.is_server_error() => Self::InternalError,
            _ => Self::InvalidRequest,
        }
    }
}

/// Carries the JSON body of an error response sent to a Bearer client.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable reason. Match on this, not on `message`.
    pub code: ErrorCode,
    /// Human-readable text.
    pub message: String,
    /// Path of the document that explains how to recover, on 401, unknown routes and wrong methods.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// On 410 `cursor_expired`, the oldest cursor of the board's events that still resumes with no
    /// event missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_cursor: Option<String>,
}

/// Builds a plain-text error response that carries its [`ErrorCode`].
pub fn coded_response(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Response {
    let mut response = (status, message.into()).into_response();
    response.extensions_mut().insert(code);
    response
}

/// Answers a request that matches no route with an empty body tagged as an unknown route.
pub async fn route_not_found(uri: Uri) -> Response {
    let code = if uri.path().starts_with("/api/") {
        ErrorCode::RouteNotFound
    } else {
        ErrorCode::NotFound
    };
    empty_response(StatusCode::NOT_FOUND, code)
}

fn empty_response(status: StatusCode, code: ErrorCode) -> Response {
    let mut response = status.into_response();
    response.extensions_mut().insert(code);
    response
}

/// What `json_errors` does with an error response.
enum Rewrite {
    Keep,
    Json,
}

/// Who sent the request, as far as error formatting is concerned.
struct Caller {
    bearer: bool,
    cookie: bool,
    api: bool,
}

impl Caller {
    fn of(req: &Request) -> Self {
        Self {
            bearer: auth::bearer_token_from_headers(req.headers()).is_some(),
            cookie: req.headers().contains_key(COOKIE),
            api: req.uri().path().starts_with("/api/"),
        }
    }

    /// Decides the format of an error: Bearer callers always get JSON, and so does an `/api/*`
    /// caller with no cookie on any 4xx.
    fn rewrite(&self, status: StatusCode) -> Rewrite {
        let anonymous_api = self.api && !self.cookie;
        if self.bearer || (anonymous_api && status.is_client_error()) {
            Rewrite::Json
        } else {
            Rewrite::Keep
        }
    }
}

/// Rewrites error responses into [`ErrorBody`] JSON for Bearer requests, and gives every 401 on
/// `/api/*` its discovery headers.
pub async fn json_errors(req: Request, next: Next) -> Response {
    let caller = Caller::of(&req);
    let mut response = next.run(req).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    if status == StatusCode::UNAUTHORIZED && caller.api {
        let headers = response.headers_mut();
        headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        headers.insert(LINK, help_link());
    }
    let code = response.extensions().get::<ErrorCode>().copied();
    let plain_text = response
        .headers()
        .get(CONTENT_TYPE)
        .is_none_or(|value| value.as_bytes().starts_with(b"text/plain"));
    if !plain_text || matches!(caller.rewrite(status), Rewrite::Keep) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_ERROR_BODY_BYTES).await else {
        return Response::from_parts(parts, Body::empty());
    };
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_TYPE);
    let mut json = Json(error_body(status, code, &bytes)).into_response();
    *json.status_mut() = status;
    json.headers_mut().extend(parts.headers);
    json
}

fn help_link() -> HeaderValue {
    HeaderValue::from_str(&format!("<{HELP_PATH}>; rel=\"help\"")).expect("static header value")
}

fn error_body(status: StatusCode, tagged: Option<ErrorCode>, bytes: &[u8]) -> ErrorBody {
    let code = tagged.unwrap_or_else(|| ErrorCode::for_status(status));
    let text = String::from_utf8_lossy(bytes).into_owned();
    let (message, help) = match code {
        _ if !text.is_empty() => {
            let help = (status == StatusCode::UNAUTHORIZED).then(|| HELP_PATH.to_string());
            (text, help)
        }
        ErrorCode::RouteNotFound => (
            format!("no such API route, the route list is at {SPEC_PATH}"),
            Some(SPEC_PATH.to_string()),
        ),
        ErrorCode::MethodNotAllowed => (
            format!("method not allowed on this route, the route list is at {SPEC_PATH}"),
            Some(SPEC_PATH.to_string()),
        ),
        _ => (
            status
                .canonical_reason()
                .unwrap_or("error")
                .to_ascii_lowercase(),
            None,
        ),
    };
    ErrorBody {
        code,
        message,
        help,
        oldest_cursor: None,
    }
}
