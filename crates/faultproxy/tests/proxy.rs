//! The proxy end to end: a tiny upstream, the proxy in front of it, reqwest as the client.

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use forgelab_faultproxy::{
    FaultEngine, Inject, Match, Proxy, Rule, Rules, Scope, Stale, StaleTrigger, When,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// An upstream that answers `METHOD path` for a GET and echoes the body of anything else.
async fn upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|req: hyper::Request<Incoming>| async move {
                    let method = req.method().clone();
                    let path = req.uri().path().to_string();
                    let host = req
                        .headers()
                        .get("host")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let body = req.into_body().collect().await.unwrap().to_bytes();
                    let out = if method == http::Method::GET {
                        Bytes::from(format!("{method} {path} host={host}"))
                    } else {
                        body
                    };
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .header("x-upstream", "1")
                            .header("content-type", "text/plain")
                            .body(Full::new(out))
                            .unwrap(),
                    )
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

fn rule(name: &str, scope: Scope, when: Option<When>, inject: Inject) -> Rule {
    Rule {
        name: name.into(),
        match_: Match {
            methods: None,
            path: None,
            scope,
        },
        when,
        inject: Some(inject),
        stale: None,
    }
}

#[tokio::test]
async fn injects_passes_and_streams() {
    let up = upstream().await;
    let rules = Rules::builder()
        .rule(rule(
            "nth",
            Scope::Api,
            Some(When {
                every_nth: Some(2),
                ..Default::default()
            }),
            Inject {
                status: Some(503),
                body: Some("injected".into()),
                ..Default::default()
            },
        ))
        .build();
    let engine = FaultEngine::new(rules, Some(1));
    let proxy = Proxy::start(format!("http://{up}").parse().unwrap(), engine.clone())
        .await
        .unwrap();
    let base = proxy.base_url();
    let client = reqwest::Client::new();

    let r = client.get(format!("{base}/api/v1/x")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-upstream"], "1");
    let body = r.text().await.unwrap();
    assert_eq!(
        body,
        format!("GET /api/v1/x host={up}"),
        "the Host header names the upstream"
    );

    let r = client.get(format!("{base}/api/v1/x")).send().await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.text().await.unwrap(), "injected");

    let r = client.get(format!("{base}/api/v1/x")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Outside the api scope nothing fires, and a large body streams through byte for byte.
    let payload: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
    for _ in 0..3 {
        let r = client
            .post(format!("{base}/o/r.git/git-receive-pack"))
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), payload.as_slice());
    }
    assert_eq!(engine.fired().len(), 1);
    proxy.shutdown().await;
}

#[tokio::test]
async fn reset_drops_the_connection() {
    let up = upstream().await;
    let rules = Rules::builder()
        .rule(rule(
            "reset",
            Scope::Any,
            Some(When {
                first_n: Some(1),
                ..Default::default()
            }),
            Inject {
                reset: true,
                ..Default::default()
            },
        ))
        .build();
    let proxy = Proxy::start(
        format!("http://{up}").parse().unwrap(),
        FaultEngine::new(rules, Some(1)),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let err = client
        .get(format!("{}/anything", proxy.base_url()))
        .send()
        .await
        .expect_err("the first request is reset");
    assert!(
        err.is_request() || err.is_connect() || err.is_body(),
        "{err}"
    );
    let r = client
        .get(format!("{}/anything", proxy.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn stale_reads_after_a_push_through_the_proxy() {
    let up = upstream().await;
    let rules = Rules::builder()
        .rule(Rule {
            name: "null-branches".into(),
            match_: Match {
                methods: Some(vec!["GET".into()]),
                path: Some("/api/v1/repos/*/*/branches".into()),
                scope: Scope::Api,
            },
            when: None,
            inject: None,
            stale: Some(Stale {
                after_mutation_on: StaleTrigger::GitReceivePackSameRepo,
                for_secs: 60.0,
                missing_as: Default::default(),
                body: Some("null".into()),
            }),
        })
        .build();
    let proxy = Proxy::start(
        format!("http://{up}").parse().unwrap(),
        FaultEngine::new(rules, Some(1)),
    )
    .await
    .unwrap();
    let base = proxy.base_url();
    let client = reqwest::Client::new();
    let r = client
        .get(format!("{base}/api/v1/repos/o/r/branches"))
        .send()
        .await
        .unwrap();
    assert!(
        r.text().await.unwrap().starts_with("GET"),
        "fresh before any push"
    );
    client
        .post(format!("{base}/o/r.git/git-receive-pack"))
        .body("pack")
        .send()
        .await
        .unwrap();
    let r = client
        .get(format!("{base}/api/v1/repos/o/r/branches"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "null", "the listing lags the push");
    let r = client
        .get(format!("{base}/api/v1/repos/o/other/branches"))
        .send()
        .await
        .unwrap();
    assert!(
        r.text().await.unwrap().starts_with("GET"),
        "another repository is unaffected"
    );
}
