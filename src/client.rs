use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Longest JSON body the client reads, in bytes.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Longest connect wait, in seconds.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Longest whole request, in seconds.
const TOTAL_TIMEOUT_SECS: u64 = 30;

/// Smallest status code that counts as the server refusing the request.
const FIRST_ERROR_STATUS: u16 = 400;

/// Most characters of a non-JSON error body the error text keeps.
const MAX_ERROR_CHARS: usize = 200;

/// Content type of a request whose body is a file.
const OCTET_STREAM: &str = "application/octet-stream";

/// What replaces the API token wherever it would reach an error text.
const REDACTED: &str = "***";

/// Environment variable naming the server, such as `http://127.0.0.1:3000`.
const URL_VAR: &str = "POMB_URL";

/// Environment variable holding the API token.
const TOKEN_VAR: &str = "POMB_API_TOKEN";

/// Why a request to a running server failed, as a message with the token already redacted.
#[derive(Debug)]
pub struct Error(
    /// The message, with the token already redacted.
    String,
);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// A blocking client of a running Pomb server, signed in with an API token.
pub struct Client {
    /// The agent that holds the timeouts and the TLS setup.
    agent: ureq::Agent,
    /// Address of the server, without a trailing slash.
    base: String,
    /// The API token, sent as a Bearer header and never printed.
    token: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Builds a client from `POMB_URL` and `POMB_API_TOKEN`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] naming the variable that is unset or empty.
    pub fn from_env() -> Result<Self, Error> {
        Self::from_vars(std::env::var(URL_VAR).ok(), std::env::var(TOKEN_VAR).ok())
    }

    /// Builds a client from the values of the two variables, `None` when unset.
    fn from_vars(url: Option<String>, token: Option<String>) -> Result<Self, Error> {
        let url = url
            .filter(|url| !url.is_empty())
            .ok_or_else(|| Error(format!("{URL_VAR} is not set")))?;
        let token = token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error(format!("{TOKEN_VAR} is not set")))?;
        Ok(Self::new(&url, token))
    }

    /// Builds a client of the server at `url` that signs in with `token`.
    pub fn new(url: &str, token: String) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls_graviola::default_provider()))
            .root_certs(RootCerts::WebPki)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            .timeout_global(Some(Duration::from_secs(TOTAL_TIMEOUT_SECS)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .new_agent();
        Self {
            agent,
            base: url.trim_end_matches('/').to_string(),
            token,
        }
    }

    /// Requests `path` with GET and reads the JSON answer as a `T`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the server cannot be reached, answers with a status of 400 or
    /// more, or sends a body that is not a `T`.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let (url, request) = self.signed(path, |agent, url| agent.get(url));
        self.read(&url, request.call())
    }

    /// Requests `path` with POST and `body` as JSON and reads the JSON answer as a `T`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the server cannot be reached, answers with a status of 400 or
    /// more, or sends a body that is not a `T`.
    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, Error> {
        let (url, request) = self.signed(path, |agent, url| agent.post(url));
        self.read(&url, request.send_json(body))
    }

    /// Requests `path` with PATCH and `body` as JSON and reads the JSON answer as a `T`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the server cannot be reached, answers with a status of 400 or
    /// more, or sends a body that is not a `T`.
    pub fn patch<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, Error> {
        let (url, request) = self.signed(path, |agent, url| agent.patch(url));
        self.read(&url, request.send_json(body))
    }

    /// Requests `path` with POST, the query `query` and `bytes` as an `application/octet-stream`
    /// body, and reads the JSON answer as a `T`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the server cannot be reached, answers with a status of 400 or
    /// more, or sends a body that is not a `T`.
    pub fn post_bytes<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        bytes: &[u8],
    ) -> Result<T, Error> {
        let (url, request) = self.signed(path, |agent, url| agent.post(url));
        let request = request
            .query_pairs(query.iter().copied())
            .content_type(OCTET_STREAM);
        self.read(&url, request.send(bytes))
    }

    /// Returns the address of `path` on the server and the request that `start` makes to it, with
    /// the Bearer header set.
    fn signed<B>(
        &self,
        path: &str,
        start: impl FnOnce(&ureq::Agent, &str) -> ureq::RequestBuilder<B>,
    ) -> (String, ureq::RequestBuilder<B>) {
        let url = format!("{}{path}", self.base);
        let request =
            start(&self.agent, &url).header("Authorization", format!("Bearer {}", self.token));
        (url, request)
    }

    /// Turns the outcome of a request to `url` into a `T`, or into the server's reason.
    fn read<T: DeserializeOwned>(
        &self,
        url: &str,
        outcome: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<T, Error> {
        let mut response =
            outcome.map_err(|err| self.fail(format!("request to {url} failed: {err}")))?;
        let status = response.status().as_u16();
        if status >= FIRST_ERROR_STATUS {
            let body = response.body_mut().read_to_string().unwrap_or_default();
            return Err(self.fail(format!("pomb answered {status}: {}", error_message(&body))));
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_json()
            .map_err(|err| {
                self.fail(format!(
                    "pomb sent an answer to {url} that cannot be read: {err}"
                ))
            })
    }

    /// Wraps `text` in an [`Error`] with the token replaced.
    fn fail(&self, text: String) -> Error {
        Error(text.replace(&self.token, REDACTED))
    }
}

/// The `message` of a JSON error body, else the start of the body, else `no message`.
fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_string));
    let text = message.unwrap_or_else(|| body.trim().chars().take(MAX_ERROR_CHARS).collect());
    if text.is_empty() {
        "no message".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests;
