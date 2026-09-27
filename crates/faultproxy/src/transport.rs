//! The in-process face: a `Transport` that faults what passes through it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use forgelab::forge::{Transport, TransportError};
use http::{Request, Response};

use crate::engine::{Action, FaultEngine};

/// Wraps a transport. Every API request forgelab makes is decided on by the engine before it
/// reaches the inner transport, and every answer that comes back is observed.
pub struct FaultTransport<T: Transport> {
    inner: T,
    engine: Arc<FaultEngine>,
    seen: Mutex<Vec<String>>,
}

impl<T: Transport> FaultTransport<T> {
    pub fn new(inner: T, engine: Arc<FaultEngine>) -> Arc<Self> {
        Arc::new(FaultTransport {
            inner,
            engine,
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn engine(&self) -> &Arc<FaultEngine> {
        &self.engine
    }

    /// Every request so far, as `METHOD /path`, faulted ones included.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn reset_seen(&self) {
        self.seen.lock().unwrap().clear();
    }
}

/// A shared transport, so that an `Arc<ScriptedTransport>` or an `Arc<dyn Transport>` can be
/// wrapped by `FaultTransport`, which wants a value.
pub struct Shared(pub Arc<dyn Transport>);

#[async_trait]
impl Transport for Shared {
    async fn send(
        &self,
        req: Request<Bytes>,
        timeout: Duration,
    ) -> Result<Response<Bytes>, TransportError> {
        self.0.send(req, timeout).await
    }
}

/// Builds a synthetic answer.
pub(crate) fn synthetic(status: u16, headers: &[(String, String)], body: Bytes) -> Response<Bytes> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(k.as_str(), v.as_str());
    }
    b.body(body)
        .expect("a status and valid headers make a response")
}

#[async_trait]
impl<T: Transport> Transport for FaultTransport<T> {
    async fn send(
        &self,
        req: Request<Bytes>,
        timeout: Duration,
    ) -> Result<Response<Bytes>, TransportError> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        self.seen.lock().unwrap().push(format!("{method} {path}"));
        let decision = self.engine.decide(&method, &path);
        if !decision.latency.is_zero() {
            tokio::time::sleep(decision.latency).await;
        }
        match decision.action {
            Action::Pass => {
                let resp = self.inner.send(req, timeout).await?;
                self.engine.observe(&method, &path, &resp);
                Ok(resp)
            }
            Action::Inject {
                status,
                headers,
                body,
            }
            | Action::ServeStale {
                status,
                headers,
                body,
            } => Ok(synthetic(status, &headers, body)),
            Action::Reset => Err(TransportError {
                message: "connection reset by peer (injected)".into(),
                before_send: false,
                timeout: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Inject, Match, Rule, Rules, Scope, When};
    use forgelab::forge::ScriptedTransport;

    fn rule(name: &str, when: Option<When>, inject: Inject) -> Rule {
        Rule {
            name: name.into(),
            match_: Match {
                methods: None,
                path: None,
                scope: Scope::Api,
            },
            when,
            inject: Some(inject),
            stale: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn injects_and_passes() {
        let inner = ScriptedTransport::new(|_| ScriptedTransport::reply(200, "real"));
        let mut inj = Inject {
            status: Some(429),
            ..Default::default()
        };
        inj.headers.insert("Retry-After".into(), "1".into());
        inj.body = Some("slow down".into());
        let engine = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "slow",
                    None,
                    Inject {
                        latency_ms: Some((250, 250)),
                        ..Default::default()
                    },
                ))
                .rule(rule(
                    "limit",
                    Some(When {
                        first_n: Some(1),
                        ..Default::default()
                    }),
                    inj,
                ))
                .rule(rule(
                    "reset",
                    Some(When {
                        every_nth: Some(2),
                        ..Default::default()
                    }),
                    Inject {
                        reset: true,
                        ..Default::default()
                    },
                ))
                .build(),
            Some(1),
        );
        let t = FaultTransport::new(Shared(inner.clone()), engine);
        let req = || {
            Request::builder()
                .method("GET")
                .uri("http://x/api/v1/repos/o/r")
                .body(Bytes::new())
                .unwrap()
        };

        let started = tokio::time::Instant::now();
        let r = t.send(req(), Duration::from_secs(1)).await.unwrap();
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers()["retry-after"], "1");
        assert_eq!(r.body(), "slow down");
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(
            inner.seen().is_empty(),
            "an injected answer never reaches the forge"
        );

        // The reset rule never saw the first request (the limit fired before it), so this is
        // its first match and every-2nd leaves it alone.
        let r = t.send(req(), Duration::from_secs(1)).await.unwrap();
        assert_eq!(r.body(), "real");
        assert_eq!(inner.seen().len(), 1);

        let err = t.send(req(), Duration::from_secs(1)).await.unwrap_err();
        assert!(err.message.contains("reset"));
        assert!(!err.before_send);

        let r = t.send(req(), Duration::from_secs(1)).await.unwrap();
        assert_eq!(r.body(), "real");
        assert_eq!(inner.seen().len(), 2);
        assert_eq!(t.seen().len(), 4);
        assert_eq!(
            t.engine().fired().len(),
            6,
            "four latencies, one limit, one reset"
        );
    }
}
