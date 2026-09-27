//! The 108-repository scale fleet, through the fault layer, against the local Forgejo. This
//! is what a cloud forge does to forgelab at size -- rate limits, 5xx under load, dropped
//! connections, reads served from caches that lag a write -- reproduced by seeded rule, so a
//! failure is reproduced by its seed alone.
//!
//! Gated twice: `FORGELAB_E2E=1` and `FORGELAB_SCALE=1`, and the scale fleet must be found
//! (`FORGELAB_FLEETS_DIR`, default `../fleets` next to this repository).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forgelab::fleet;
use forgelab_faultproxy::{FaultEngine, Proxy, Rules};

use crate::lab::{Lab, e2e_enabled, exit_kind, forgejo};

fn scale_fleet() -> Option<PathBuf> {
    if !e2e_enabled() {
        return None;
    }
    if std::env::var("FORGELAB_SCALE")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        let dir = match std::env::var_os("FORGELAB_FLEETS_DIR") {
            Some(d) => PathBuf::from(d),
            None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fleets"),
        };
        let scale = dir.join("scale");
        if scale.join(fleet::LOCK_FILE).exists() {
            return Some(scale);
        }
        eprintln!(
            "scale: no fleets checkout at {} (set FORGELAB_FLEETS_DIR)",
            dir.display()
        );
        return None;
    }
    eprintln!("skipping the scale scenarios: set FORGELAB_SCALE=1");
    None
}

fn rules(name: &str) -> Rules {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../faults")
        .join(format!("{name}.yaml"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Rules::from_yaml(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The seed to reproduce this run with, from the environment or fresh.
fn seed() -> Option<u64> {
    std::env::var("FORGELAB_FAULT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// The whole lifecycle at size, ending with the committed lock reproduced byte for byte.
async fn full_loop(l: &Lab, engine: &Arc<FaultEngine>, committed: &[u8]) {
    let run = async {
        let spec = fleet::load_spec(&l.dir()).map_err(|e| e.to_string())?;
        let names: Vec<&str> = spec.repos.iter().map(|r| r.name.as_str()).collect();
        let (a, b, c) = (names[0], names[names.len() / 2], names[names.len() - 1]);

        l.env().plan().await.map_err(|e| format!("plan: {e}"))?;
        let out = l.out();
        if out.matches("  + ").count() != spec.repos.len() {
            return Err(format!("plan: want {} creates:\n{out}", spec.repos.len()));
        }
        l.env()
            .apply()
            .await
            .map_err(|e| format!("apply: {e}\n{}", l.out()))?;
        l.env()
            .verify()
            .await
            .map_err(|e| format!("verify after apply: {e}"))?;

        // A test run's mess on three repositories: a direct commit, a branch with an open pull
        // request, a stray tag.
        l.drift2(a).await;
        l.must_api(
            "POST",
            &format!("{}/contents/NEW.md", l.repo(b)),
            r#"{"content":"aGVsbG8K","message":"change","new_branch":"feature/run-42"}"#,
        )
        .await;
        l.must_api(
            "POST",
            &format!("{}/pulls", l.repo(b)),
            r#"{"head":"feature/run-42","base":"main","title":"run 42"}"#,
        )
        .await;
        l.must_api(
            "POST",
            &format!("{}/tags", l.repo(c)),
            r#"{"tag_name":"stray","target":"main"}"#,
        )
        .await;
        let v = l.env().verify().await;
        if exit_kind(&v) != "drift" {
            return Err(format!("verify after drift: want drift, got {v:?}"));
        }
        l.env().reset().await.map_err(|e| format!("reset: {e}"))?;
        if !l
            .out()
            .contains(&format!("3 of {} repositories reset", spec.repos.len()))
        {
            return Err(format!("reset output:\n{}", l.out()));
        }
        l.env()
            .verify()
            .await
            .map_err(|e| format!("verify after reset: {e}"))?;

        l.env()
            .destroy()
            .await
            .map_err(|e| format!("destroy: {e}"))?;
        for name in [a, b, c] {
            if l.api("GET", &l.repo(name), "").await != 404 {
                return Err(format!("{name} survived destroy"));
            }
        }
        l.env()
            .apply()
            .await
            .map_err(|e| format!("apply after destroy: {e}"))?;
        l.env()
            .verify()
            .await
            .map_err(|e| format!("verify after re-apply: {e}"))?;
        let lock = std::fs::read(l.dir().join(fleet::LOCK_FILE)).map_err(|e| e.to_string())?;
        if lock != committed {
            return Err("the lock is not byte-identical to the committed one".to_string());
        }
        Ok::<(), String>(())
    };
    if let Err(e) = run.await {
        let fired = engine.fired();
        let mut log = String::new();
        for f in fired
            .iter()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            log.push_str(&format!(
                "\n  {} {} {} #{}",
                f.rule, f.method, f.path, f.counter
            ));
        }
        panic!(
            "scale scenario failed (reproduce with FORGELAB_FAULT_SEED={}): {e}\n{} rules fired; the last ones:{log}",
            engine.seed(),
            fired.len()
        );
    }
    eprintln!(
        "scale: ok, {} rules fired (seed {})",
        engine.fired().len(),
        engine.seed()
    );
}

async fn scenario(name: &str, through_proxy: bool) {
    let Some(scale) = scale_fleet() else { return };
    let committed = std::fs::read(scale.join(fleet::LOCK_FILE)).unwrap();
    let rules = if name == "clean" {
        Rules::default()
    } else {
        rules(name)
    };
    if through_proxy {
        let engine = FaultEngine::new(rules, seed());
        let upstream = forgejo().base_url.parse().unwrap();
        let proxy = Proxy::start(upstream, engine.clone())
            .await
            .expect("start proxy");
        eprintln!(
            "scale/{name}: proxy at {} (FORGELAB_FAULT_SEED={})",
            proxy.base_url(),
            engine.seed()
        );
        let l = Lab::with_rules(&scale, Rules::default(), Some(proxy.base_url())).await;
        full_loop(&l, &engine, &committed).await;
        proxy.shutdown().await;
    } else {
        let l = Lab::with_rules(&scale, rules, None).await;
        let engine = l.engine().clone();
        full_loop(&l, &engine, &committed).await;
    }
}

/// S0: the loop with no faults at all.
#[tokio::test]
async fn s0_clean() {
    scenario("clean", false).await;
}

/// S1: secondary rate limits and 429s, with and without Retry-After.
#[tokio::test]
async fn s1_ratelimits() {
    scenario("ratelimits", false).await;
}

/// S2: 5xx, dropped connections and latency.
#[tokio::test]
async fn s2_transport() {
    scenario("transport", false).await;
}

/// S3: reads served stale after a write, and a null branch listing right after a push.
#[tokio::test]
async fn s3_stale() {
    scenario("stale", false).await;
}

/// S4: everything at half probability, through the proxy, so git traffic is faulted too.
#[tokio::test]
async fn s4_mixed_through_proxy() {
    scenario("mixed", true).await;
}
