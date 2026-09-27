//! One HTTP layer under every forge client: a `Transport` that moves bytes, and an
//! `HttpClient` that decides what to do with the answer -- classify it, wait out a rate limit,
//! retry a transient failure, serialise and pace writes where a forge asks for that -- so that
//! no client has its own retry loop, and a test can stand in for the network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::classify::{Classified, Classifier};
use super::error::{Class, ForgeError, TransportError, reason};

/// Moves one request to a server and its answer back. The production one is reqwest; tests
/// script answers in-process; the fault layer wraps either.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(
        &self,
        req: Request<Bytes>,
        timeout: Duration,
    ) -> Result<Response<Bytes>, TransportError>;
}

/// The real thing. Follows redirects, as Go's default client did: that is how a renamed
/// repository is caught answering under another name.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, ForgeError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(concat!("forgelab/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ForgeError::msg(format!("build HTTP client: {e}")))?;
        Ok(ReqwestTransport { client })
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(
        &self,
        req: Request<Bytes>,
        timeout: Duration,
    ) -> Result<Response<Bytes>, TransportError> {
        let (parts, body) = req.into_parts();
        let url = parts.uri.to_string();
        let mut builder = self.client.request(parts.method, url).timeout(timeout);
        builder = builder.headers(parts.headers);
        if !body.is_empty() {
            builder = builder.body(body);
        }
        let resp = builder.send().await.map_err(|e| TransportError {
            message: describe(&e),
            before_send: e.is_connect() || e.is_request() && !e.is_timeout(),
            timeout: e.is_timeout(),
        })?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(|e| TransportError {
            message: describe(&e),
            before_send: false,
            timeout: e.is_timeout(),
        })?;
        let mut out = Response::builder()
            .status(status)
            .body(bytes)
            .expect("a status is a response");
        *out.headers_mut() = headers;
        Ok(out)
    }
}

/// reqwest's Display of an error includes the URL, which may carry nothing secret in this
/// crate (credentials travel in headers) but is long; the source chain says what happened.
fn describe(e: &reqwest::Error) -> String {
    let mut msg = if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connect".to_string()
    } else {
        "request failed".to_string()
    };
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

/// A transport that answers from a closure, the way Go's `httptest` handlers did, and
/// remembers every request it saw as `METHOD /path`.
/// A scripted answer to one request.
pub type Handler = dyn Fn(&Request<Bytes>) -> Response<Bytes> + Send + Sync;

pub struct ScriptedTransport {
    handler: Arc<Handler>,
    seen: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    pub fn new(
        handler: impl Fn(&Request<Bytes>) -> Response<Bytes> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(ScriptedTransport {
            handler: Arc::new(handler),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// Every request so far, as `METHOD /path` (the path as sent, percent-encoding kept).
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn reset(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// A response with a status and a body, for handlers.
    pub fn reply(status: u16, body: &str) -> Response<Bytes> {
        Response::builder()
            .status(status)
            .body(Bytes::from(body.to_string()))
            .unwrap()
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn send(
        &self,
        req: Request<Bytes>,
        _timeout: Duration,
    ) -> Result<Response<Bytes>, TransportError> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method(), req.uri().path()));
        Ok((self.handler)(&req))
    }
}

/// How long a logical request may take, all pauses and retries included, and how retries are
/// spaced.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total wall-clock budget for one logical request. A rate limit whose pause would end
    /// after it is an error to report, not a pause to sit through: a secondary limit asks for
    /// a minute, an exhausted hourly one for most of an hour.
    pub budget: Duration,
    /// First pause after a transient failure; doubled each time, with full jitter, up to `cap`.
    pub base: Duration,
    pub cap: Duration,
    /// The pause when a rate limit says nothing about how long.
    pub default_rate_limit_wait: Duration,
    /// Per-request network timeout.
    pub timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            budget: Duration::from_secs(10 * 60),
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
            default_rate_limit_wait: Duration::from_secs(60),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Serialises mutations and spaces them out. GitHub asks that mutating requests be made
/// serially, not concurrently; bursts of them are what trips the secondary rate limit.
pub struct WriteLane {
    next: tokio::sync::Mutex<Instant>,
    min_interval: Duration,
}

impl WriteLane {
    pub fn new(min_interval: Duration) -> Arc<Self> {
        Arc::new(WriteLane {
            next: tokio::sync::Mutex::new(Instant::now()),
            min_interval,
        })
    }

    /// Waits for the lane and for the pacing interval; the guard holds the lane until dropped.
    pub async fn acquire(&self) -> tokio::sync::MutexGuard<'_, Instant> {
        let mut g = self.next.lock().await;
        tokio::time::sleep_until(*g).await;
        *g = Instant::now() + self.min_interval;
        g
    }
}

/// Per-request knobs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestOpts {
    /// A request that may be repeated after a transient failure without doing its work
    /// twice. Defaults to true for GET, HEAD, PUT, PATCH and DELETE; false for POST.
    pub idempotent: Option<bool>,
    /// Overrides the policy's budget.
    pub budget: Option<Duration>,
}

/// The client every forge shares.
pub struct HttpClient {
    pub forge: &'static str,
    transport: Arc<dyn Transport>,
    classify: Classifier,
    policy: RetryPolicy,
    default_headers: HeaderMap,
    write_lane: Option<Arc<WriteLane>>,
    cancel: CancellationToken,
}

/// A successful answer.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Answer {
    pub fn json<T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
    ) -> Result<Option<T>, ForgeError> {
        if self.body.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&self.body)
            .map(Some)
            .map_err(|e| ForgeError::Decode {
                method: method.to_string(),
                path: path.to_string(),
                message: e.to_string(),
            })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl HttpClient {
    pub fn new(
        forge: &'static str,
        transport: Arc<dyn Transport>,
        classify: Classifier,
        default_headers: HeaderMap,
    ) -> Self {
        HttpClient {
            forge,
            transport,
            classify,
            policy: RetryPolicy::default(),
            default_headers,
            write_lane: None,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_write_lane(mut self, lane: Arc<WriteLane>) -> Self {
        self.write_lane = Some(lane);
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Sends a JSON request and decodes a JSON answer, if any.
    pub async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOpts,
    ) -> Result<(HeaderMap, Option<T>), ForgeError> {
        let path = path_of(url);
        let answer = self.send(method.clone(), url, body, opts).await?;
        let decoded = answer.json(&method, &path)?;
        Ok((answer.headers, decoded))
    }

    /// Sends a request whose answer body does not matter.
    pub async fn call(
        &self,
        method: Method,
        url: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOpts,
    ) -> Result<Answer, ForgeError> {
        self.send(method, url, body, opts).await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOpts,
    ) -> Result<Answer, ForgeError> {
        let path = path_of(url);
        let raw = match body {
            Some(b) => Bytes::from(
                serde_json::to_vec(b)
                    .map_err(|e| ForgeError::msg(format!("encode request: {e}")))?,
            ),
            None => Bytes::new(),
        };
        let has_body = body.is_some();
        let idempotent = opts.idempotent.unwrap_or(method != Method::POST);
        let deadline = Instant::now() + opts.budget.unwrap_or(self.policy.budget);

        // Held for the whole logical request, pauses included: a mutation waiting out a rate
        // limit must not let another one through.
        let _lane = match (
            &self.write_lane,
            method == Method::GET || method == Method::HEAD,
        ) {
            (Some(lane), false) => Some(lane.acquire().await),
            _ => None,
        };

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut req = Request::builder().method(method.clone()).uri(url);
            for (k, v) in &self.default_headers {
                req = req.header(k, v);
            }
            if has_body {
                req = req.header(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            let req = req
                .body(raw.clone())
                .map_err(|e| ForgeError::msg(format!("build request: {e}")))?;

            let sent = tokio::select! {
                r = self.transport.send(req, self.policy.timeout) => r,
                _ = self.cancel.cancelled() => return Err(ForgeError::Cancelled),
            };
            let resp = match sent {
                Ok(r) => r,
                Err(te) => {
                    let can_retry = idempotent || te.before_send;
                    let pause = self.backoff(attempt);
                    if can_retry && Instant::now() + pause < deadline {
                        tracing::debug!(forge = self.forge, %method, path, attempt, "transport failure, retrying: {}", te.message);
                        self.sleep(pause).await?;
                        continue;
                    }
                    return Err(ForgeError::Transport {
                        method: method.to_string(),
                        path,
                        source: te,
                    });
                }
            };

            let status = resp.status().as_u16();
            // A classifier may veto a 2xx: Azure DevOps answers a bad token with a 203 and a
            // sign-in page, which is not success.
            if (200..300).contains(&status) && (self.classify)(&resp).class != Class::Auth {
                let (parts, body) = resp.into_parts();
                return Ok(Answer {
                    status,
                    headers: parts.headers,
                    body,
                });
            }
            let Classified { class, retry_after } = (self.classify)(&resp);
            match class {
                Class::RateLimited => {
                    let wait = retry_after.unwrap_or(self.policy.default_rate_limit_wait);
                    if Instant::now() + wait > deadline {
                        return Err(ForgeError::RateLimitExceeded {
                            forge: self.forge,
                            method: method.to_string(),
                            path,
                            wait,
                        });
                    }
                    tracing::debug!(forge = self.forge, %method, path, wait_secs = wait.as_secs(), "rate limited, waiting");
                    self.sleep(wait).await?;
                    continue;
                }
                Class::Transient if idempotent => {
                    let pause = self.backoff(attempt);
                    if Instant::now() + pause < deadline {
                        tracing::debug!(forge = self.forge, %method, path, status, attempt, "transient failure, retrying");
                        self.sleep(pause).await?;
                        continue;
                    }
                }
                _ => {}
            }
            let body = String::from_utf8_lossy(resp.body()).trim().to_string();
            let body = if body.len() > 2048 {
                format!("{}…", &body[..body.floor_char_boundary(2048)])
            } else {
                body
            };
            return Err(ForgeError::Status {
                class,
                status,
                reason: reason(status),
                method: method.to_string(),
                path,
                body,
                retry_after,
            });
        }
    }

    /// Exponential backoff with full jitter, capped.
    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .policy
            .base
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
        let cap = exp.min(self.policy.cap);
        let jitter: f64 = rand::random::<f64>();
        cap.mul_f64(jitter.max(0.1))
    }

    async fn sleep(&self, d: Duration) -> Result<(), ForgeError> {
        tokio::select! {
            _ = tokio::time::sleep(d) => Ok(()),
            _ = self.cancel.cancelled() => Err(ForgeError::Cancelled),
        }
    }
}

/// The path of a URL, for messages: no host, no query.
pub fn path_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => u.path().to_string(),
        Err(_) => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client(t: Arc<ScriptedTransport>, classify: Classifier) -> HttpClient {
        HttpClient::new("Test", t, classify, HeaderMap::new()).with_policy(RetryPolicy {
            budget: Duration::from_secs(600),
            base: Duration::from_millis(100),
            cap: Duration::from_secs(2),
            default_rate_limit_wait: Duration::from_secs(60),
            timeout: Duration::from_secs(30),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limits_are_waited_out_within_the_budget() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let t = ScriptedTransport::new(move |_| {
            // Six secondary limits in a row: more than the four attempts the Go client allowed.
            if n2.fetch_add(1, Ordering::SeqCst) < 6 {
                ScriptedTransport::reply(
                    403,
                    r#"{"message":"You have exceeded a secondary rate limit"}"#,
                )
            } else {
                ScriptedTransport::reply(200, r#"{"ok":true}"#)
            }
        });
        let c = client(t.clone(), super::super::classify::github);
        let started = Instant::now();
        let (_, v): (_, Option<serde_json::Value>) = c
            .json(
                Method::POST,
                "http://x/api/thing",
                Some(&serde_json::json!({})),
                RequestOpts::default(),
            )
            .await
            .unwrap();
        assert_eq!(v.unwrap()["ok"], true);
        assert_eq!(t.seen().len(), 7);
        assert!(started.elapsed() >= Duration::from_secs(360));
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_hourly_limit_is_an_error_not_an_hours_sleep() {
        let reset = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 2400)
            .to_string();
        let t = ScriptedTransport::new(move |_| {
            Response::builder()
                .status(403)
                .header("X-RateLimit-Remaining", "0")
                .header("X-RateLimit-Reset", &reset)
                .body(Bytes::new())
                .unwrap()
        });
        let c = client(t.clone(), super::super::classify::github);
        let err = c
            .call(
                Method::GET,
                "http://x/api/thing",
                None::<&()>,
                RequestOpts::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::RateLimitExceeded { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("rate limited by Test; retry in 40m"),
            "{err}"
        );
        assert_eq!(t.seen().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_retry_only_when_safe() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let t = ScriptedTransport::new(move |_| {
            if n2.fetch_add(1, Ordering::SeqCst) < 2 {
                ScriptedTransport::reply(503, "")
            } else {
                ScriptedTransport::reply(200, "")
            }
        });
        let c = client(t.clone(), super::super::classify::generic);
        c.call(
            Method::GET,
            "http://x/a",
            None::<&()>,
            RequestOpts::default(),
        )
        .await
        .unwrap();
        assert_eq!(t.seen().len(), 3);

        n.store(0, Ordering::SeqCst);
        t.reset();
        let err = c
            .call(
                Method::POST,
                "http://x/a",
                None::<&()>,
                RequestOpts::default(),
            )
            .await
            .unwrap_err();
        assert!(err.is_status(&[503]), "{err}");
        assert_eq!(t.seen().len(), 1, "a POST is not repeated on a 5xx");
    }

    #[tokio::test(start_paused = true)]
    async fn errors_carry_class_and_text() {
        let t =
            ScriptedTransport::new(|_| ScriptedTransport::reply(404, r#"{"message":"Not Found"}"#));
        let c = client(t, super::super::classify::generic);
        let err = c
            .call(
                Method::GET,
                "http://x/api/v1/repos/o/r",
                None::<&()>,
                RequestOpts::default(),
            )
            .await
            .unwrap_err();
        assert!(err.is_not_found());
        assert_eq!(
            err.to_string(),
            r#"GET /api/v1/repos/o/r: 404 Not Found: {"message":"Not Found"}"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn write_lane_serialises_and_paces() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let (i2, m2) = (inflight.clone(), max.clone());
        let t = ScriptedTransport::new(move |_| {
            let now = i2.fetch_add(1, Ordering::SeqCst) + 1;
            m2.fetch_max(now, Ordering::SeqCst);
            i2.fetch_sub(1, Ordering::SeqCst);
            ScriptedTransport::reply(200, "")
        });
        let c = Arc::new(
            client(t.clone(), super::super::classify::generic)
                .with_write_lane(WriteLane::new(Duration::from_secs(1))),
        );
        let started = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..5 {
            let c = c.clone();
            tasks.spawn(async move {
                c.call(
                    Method::PATCH,
                    "http://x/a",
                    Some(&serde_json::json!({})),
                    RequestOpts::default(),
                )
                .await
                .unwrap()
            });
        }
        while tasks.join_next().await.is_some() {}
        assert_eq!(max.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "five writes are spaced a second apart"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_wait() {
        let t = ScriptedTransport::new(|_| ScriptedTransport::reply(429, ""));
        let cancel = CancellationToken::new();
        let c = client(t, super::super::classify::generic).with_cancel(cancel.clone());
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            c2.cancel();
        });
        let err = c
            .call(
                Method::GET,
                "http://x/a",
                None::<&()>,
                RequestOpts::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ForgeError::Cancelled));
    }
}
