//! forgelab-faultproxy: a fault-injecting reverse proxy in front of a forge.
//!
//!   forgelab-faultproxy --upstream http://localhost:3000 --listen 127.0.0.1:3001 \
//!       --rules faults/mixed.yaml [--seed N]
//!
//! Point a sandbox's `base_url` at the listen address. The seed is printed at start and can
//! also come from FORGELAB_FAULT_SEED.

use forgelab_faultproxy::{FaultEngine, Proxy, Rules};

const USAGE: &str =
    "usage: forgelab-faultproxy --upstream URL --listen HOST:PORT --rules FILE [--seed N]";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    std::process::exit(run());
}

fn run() -> i32 {
    let mut upstream = None;
    let mut listen = "127.0.0.1:0".to_string();
    let mut rules = None;
    let mut seed = std::env::var("FORGELAB_FAULT_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let (flag, value) = match a.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (a.clone(), None),
        };
        let mut value = || value.clone().or_else(|| args.next());
        match flag.as_str() {
            "--upstream" => upstream = value(),
            "--listen" => listen = value().unwrap_or(listen.clone()),
            "--rules" => rules = value(),
            "--seed" => seed = value().and_then(|v| v.parse().ok()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => {
                eprintln!("forgelab-faultproxy: unknown flag {other}\n{USAGE}");
                return 2;
            }
        }
    }
    let (Some(upstream), Some(rules_path)) = (upstream, rules) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let upstream: url::Url = match upstream.parse() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("forgelab-faultproxy: --upstream {upstream:?}: {e}");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(&rules_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("forgelab-faultproxy: read {rules_path}: {e}");
            return 2;
        }
    };
    let rules = match Rules::from_yaml(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("forgelab-faultproxy: {rules_path}: {e}");
            return 2;
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let engine = FaultEngine::new(rules, seed);
        let proxy = match Proxy::start_on(&listen, upstream, engine.clone()).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("forgelab-faultproxy: {e}");
                return 2;
            }
        };
        println!(
            "faultproxy: seed={} listening on {}",
            engine.seed(),
            proxy.base_url()
        );
        let _ = tokio::signal::ctrl_c().await;
        let fired = engine.fired();
        eprintln!("faultproxy: {} rules fired", fired.len());
        proxy.shutdown().await;
        0
    })
}
