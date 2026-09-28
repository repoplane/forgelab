//! The out-of-process face: a reverse proxy in front of the forge, so that git smart-HTTP is
//! faulted along with the API, the way a load balancer between forgelab and a real forge
//! would fault both.
//!
//! Bodies are streamed both ways. A `git-receive-pack` upload is chunked and may be large;
//! the proxy never buffers or decodes it. Only API reads (`GET /api/...`) are buffered, so
//! that the engine can snapshot them for stale answers.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{Action, FaultEngine, is_hop_by_hop};
use crate::transport::synthetic;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;
type OutBody = BoxBody<Bytes, BoxErr>;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("upstream {0} has no host")]
    Upstream(Url),
}

struct Ctx {
    upstream: Url,
    authority: String,
    engine: Arc<FaultEngine>,
    client: Client<HttpConnector, Incoming>,
}

/// A running proxy. Dropping it stops it.
pub struct Proxy {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    engine: Arc<FaultEngine>,
}

impl Proxy {
    /// Starts on a free loopback port.
    pub async fn start(upstream: Url, engine: Arc<FaultEngine>) -> Result<Proxy, ProxyError> {
        Self::start_on("127.0.0.1:0", upstream, engine).await
    }

    /// Starts on `listen` (`host:port`; port 0 picks a free one).
    pub async fn start_on(
        listen: &str,
        upstream: Url,
        engine: Arc<FaultEngine>,
    ) -> Result<Proxy, ProxyError> {
        let host = upstream
            .host_str()
            .ok_or_else(|| ProxyError::Upstream(upstream.clone()))?;
        let authority = match upstream.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| ProxyError::Bind {
                addr: listen.to_string(),
                source: e,
            })?;
        let addr = listener.local_addr().map_err(|e| ProxyError::Bind {
            addr: listen.to_string(),
            source: e,
        })?;
        let client = Client::builder(TokioExecutor::new()).build_http::<Incoming>();
        let ctx = Arc::new(Ctx {
            upstream,
            authority,
            engine: engine.clone(),
            client,
        });
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    _ = stop.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("faultproxy: accept: {e}");
                            continue;
                        }
                    },
                };
                let ctx = ctx.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let conn = http1::Builder::new()
                        .preserve_header_case(true)
                        .serve_connection(io, service_fn(move |req| handle(ctx.clone(), req)));
                    tokio::select! {
                        _ = stop.cancelled() => {}
                        r = conn => {
                            if let Err(e) = r {
                                tracing::debug!("faultproxy: connection ended: {e}");
                            }
                        }
                    }
                });
            }
        });
        tracing::info!(
            "faultproxy: listening on http://{addr}, seed={}",
            engine.seed()
        );
        Ok(Proxy {
            addr,
            cancel,
            task: Some(task),
            engine,
        })
    }

    /// `http://127.0.0.1:PORT`, to put into `sandboxes.yaml` as `base_url`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn engine(&self) -> &Arc<FaultEngine> {
        &self.engine
    }

    /// Stops accepting and closes open connections.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Removes the headers `drop` names. `HeaderMap` has no `retain`.
fn strip_headers(headers: &mut http::HeaderMap, drop: impl Fn(&str) -> bool) {
    let doomed: Vec<http::HeaderName> = headers
        .keys()
        .filter(|k| drop(k.as_str()))
        .cloned()
        .collect();
    for k in doomed {
        headers.remove(k);
    }
}

fn full(body: Bytes) -> OutBody {
    Full::new(body).map_err(|never| match never {}).boxed()
}

fn to_out(resp: Response<Bytes>) -> Response<OutBody> {
    resp.map(full)
}

async fn handle(ctx: Arc<Ctx>, req: Request<Incoming>) -> Result<Response<OutBody>, BoxErr> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let decision = ctx.engine.decide(&method, &path);
    if !decision.latency.is_zero() {
        tokio::time::sleep(decision.latency).await;
    }
    match decision.action {
        Action::Inject {
            status,
            headers,
            body,
        }
        | Action::ServeStale {
            status,
            headers,
            body,
        } => {
            return Ok(to_out(synthetic(status, &headers, body)));
        }
        // An error out of the service makes hyper drop the connection without an answer,
        // which is what a reset looks like from the client's side.
        Action::Reset => return Err("connection reset (injected)".into()),
        Action::Pass => {}
    }

    // Forward. The path and query are the client's; scheme and authority are the upstream's.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let uri: Uri = format!(
        "{}://{}{}",
        ctx.upstream.scheme(),
        ctx.authority,
        path_and_query
    )
    .parse()?;
    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    strip_headers(&mut parts.headers, |k| is_hop_by_hop(k) || k == "expect");
    parts
        .headers
        .insert(http::header::HOST, HeaderValue::from_str(&ctx.authority)?);
    let upstream_req = Request::from_parts(parts, body);

    let resp = ctx.client.request(upstream_req).await?;
    let status = resp.status().as_u16();
    let buffer = (method == Method::GET || method == Method::HEAD) && path.starts_with("/api/");
    let (mut rparts, rbody) = resp.into_parts();
    strip_headers(&mut rparts.headers, is_hop_by_hop);
    if buffer {
        let bytes = rbody.collect().await?.to_bytes();
        let snapshot = Response::from_parts(rparts.clone(), bytes.clone());
        ctx.engine.observe(&method, &path, &snapshot);
        return Ok(Response::from_parts(rparts, full(bytes)));
    }
    ctx.engine.observe_mutation(&method, &path, status);
    Ok(Response::from_parts(
        rparts,
        rbody.map_err(|e| Box::new(e) as BoxErr).boxed(),
    ))
}
