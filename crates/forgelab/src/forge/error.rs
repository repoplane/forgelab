//! How a forge's answer is classified, so that callers can tell "not there" from "broken"
//! from "try again later".

use std::time::Duration;

use crate::util::go_duration;

/// What an HTTP answer means for the caller. Every status code lands in exactly one class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// 404, or a forge's way of saying the same thing.
    NotFound,
    /// 409.
    Conflict,
    /// 422.
    Unprocessable,
    /// 401, or a forge's sign-in page in place of an answer.
    Auth,
    /// 403 that is not a rate limit: the token lacks a permission.
    Forbidden,
    /// 429, or a 403 that GitHub uses for the same purpose. Retried while the budget lasts.
    RateLimited,
    /// 5xx, 408, or a failure below HTTP. Retried when the request is idempotent.
    Transient,
    /// Anything else outside 2xx.
    Permanent,
}

impl Class {
    /// The class a bare status code falls in, before any forge-specific reading.
    pub fn of_status(status: u16) -> Class {
        match status {
            404 => Class::NotFound,
            409 => Class::Conflict,
            422 => Class::Unprocessable,
            401 => Class::Auth,
            403 => Class::Forbidden,
            429 => Class::RateLimited,
            408 | 500 | 502 | 503 | 504 => Class::Transient,
            _ => Class::Permanent,
        }
    }
}

/// A failure below HTTP: the connection, the resolver, a timeout.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub message: String,
    /// The request never reached the server (connect or DNS failure), so a non-idempotent
    /// request can be repeated safely.
    pub before_send: bool,
    pub timeout: bool,
}

/// What a forge call reports. `Display` is what the user reads, so it names the request and
/// never contains a credential.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ForgeError {
    /// The server answered outside 2xx.
    #[error("{method} {path}: {status} {reason}: {body}")]
    Status {
        class: Class,
        status: u16,
        reason: &'static str,
        method: String,
        path: String,
        /// The response body, trimmed and truncated for a log line.
        body: String,
        retry_after: Option<Duration>,
    },
    /// A rate limit asked for a pause that would outlast the request's time budget.
    #[error("{method} {path}: rate limited by {forge}; retry in {}", go_duration(*wait))]
    RateLimitExceeded {
        forge: &'static str,
        method: String,
        path: String,
        wait: Duration,
    },
    /// The request never got an HTTP answer.
    #[error("{method} {path}: {source}")]
    Transport {
        method: String,
        path: String,
        #[source]
        source: TransportError,
    },
    /// Anything a client says in its own words: a missing organisation, a namespace pending
    /// deletion, an operation that failed.
    #[error("{0}")]
    Message(String),
    /// A JSON body that did not parse.
    #[error("{method} {path}: decode response: {message}")]
    Decode { method: String, path: String, message: String },
    /// The run was interrupted.
    #[error("interrupted")]
    Cancelled,
}

impl ForgeError {
    pub fn msg(m: impl Into<String>) -> Self {
        ForgeError::Message(m.into())
    }

    /// The class of the failure. Messages count as permanent.
    pub fn class(&self) -> Class {
        match self {
            ForgeError::Status { class, .. } => *class,
            ForgeError::RateLimitExceeded { .. } => Class::RateLimited,
            ForgeError::Transport { .. } => Class::Transient,
            ForgeError::Message(_) | ForgeError::Decode { .. } | ForgeError::Cancelled => Class::Permanent,
        }
    }

    /// True for an HTTP answer with one of these status codes: the Go `hasStatus`.
    pub fn is_status(&self, codes: &[u16]) -> bool {
        matches!(self, ForgeError::Status { status, .. } if codes.contains(status))
    }

    pub fn is_not_found(&self) -> bool {
        self.class() == Class::NotFound
    }

    /// Prefixes the message, keeping the class: the Go `fmt.Errorf("%s: %w", ...)`.
    pub fn context(self, prefix: &str) -> Self {
        match self {
            ForgeError::Message(m) => ForgeError::Message(format!("{prefix}: {m}")),
            other => ForgeError::Message(format!("{prefix}: {other}")),
        }
    }
}

/// The canonical reason phrase Go's `resp.Status` carried, for the error text.
pub fn reason(status: u16) -> &'static str {
    http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("")
}
