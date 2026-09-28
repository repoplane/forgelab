//! Decides, per request, what to do to it -- and remembers what it saw, so that a read can be
//! answered with what was there before the last write.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Response};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tokio::time::Instant;

use crate::rules::{MissingAs, Rules, StaleTrigger};

/// What to do with one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Added before anything else happens.
    pub latency: Duration,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Forward to the forge.
    Pass,
    /// Answer without asking the forge.
    Inject {
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
    },
    /// Drop the connection.
    Reset,
    /// Answer with what the path looked like before the last mutation, or with a synthetic
    /// answer standing in for a lagging cache.
    ServeStale {
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
    },
}

/// One rule that fired, for the log a failing test prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub rule: String,
    pub method: String,
    pub path: String,
    /// How many requests this exact `METHOD path` had seen before this one.
    pub counter: u64,
}

#[derive(Default)]
struct RuleState {
    matched: u64,
    fired_total: u64,
    fired_per_path: HashMap<String, u64>,
}

#[derive(Clone)]
struct Snapshot {
    status: u16,
    headers: Vec<(String, String)>,
    body: Bytes,
}

#[derive(Default)]
struct StaleStore {
    /// The last good answer per path, taken while no window covers the path.
    snapshots: HashMap<String, Snapshot>,
    /// When a key was last mutated, per trigger kind. A window is open while the rule's
    /// `for_secs` has not elapsed since then.
    mutated: HashMap<(StaleTrigger, String), Instant>,
}

/// The fault engine: rules, counters, the stale store and the fired log.
pub struct FaultEngine {
    rules: Rules,
    seed: u64,
    per_path: Mutex<HashMap<String, u64>>,
    states: Vec<Mutex<RuleState>>,
    stale: Mutex<StaleStore>,
    fired: Mutex<Vec<Fired>>,
}

impl FaultEngine {
    /// `seed` overrides the one in the rules; with neither, a random seed is drawn. The seed
    /// in use is logged so that a run can be repeated.
    pub fn new(rules: Rules, seed: Option<u64>) -> Arc<FaultEngine> {
        let seed = seed.or(rules.seed).unwrap_or_else(|| rand::rng().random());
        tracing::info!("faultproxy: seed={seed}");
        let states = rules
            .rules
            .iter()
            .map(|_| Mutex::new(RuleState::default()))
            .collect();
        Arc::new(FaultEngine {
            rules,
            seed,
            per_path: Mutex::new(HashMap::new()),
            states,
            stale: Mutex::new(StaleStore::default()),
            fired: Mutex::new(Vec::new()),
        })
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// Every rule that fired so far, in order.
    pub fn fired(&self) -> Vec<Fired> {
        self.fired.lock().unwrap().clone()
    }

    /// Decides what happens to this request. Rules are evaluated in order: latency
    /// accumulates, the first status, reset or stale answer that fires wins.
    pub fn decide(&self, method: &Method, path: &str) -> Decision {
        let key = format!("{method} {path}");
        let counter = {
            let mut m = self.per_path.lock().unwrap();
            let c = m.entry(key.clone()).or_insert(0);
            let before = *c;
            *c += 1;
            before
        };
        let mut latency = Duration::ZERO;
        for (i, rule) in self.rules.rules.iter().enumerate() {
            if !rule.matches(method, path) {
                continue;
            }
            let mut rng = ChaCha8Rng::seed_from_u64(fnv(&[
                &self.seed.to_le_bytes(),
                rule.name.as_bytes(),
                method.as_str().as_bytes(),
                path.as_bytes(),
                &counter.to_le_bytes(),
            ]));
            let mut state = self.states[i].lock().unwrap();
            state.matched += 1;
            if !self.when_holds(rule, &state, &key, &mut rng) {
                continue;
            }
            if let Some(stale) = &rule.stale {
                let Some(answer) = self.stale_answer(path, stale) else {
                    continue;
                };
                self.record(&mut state, rule, method, path, counter, &key);
                return Decision {
                    latency,
                    action: answer,
                };
            }
            if let Some(inject) = &rule.inject {
                if let Some((lo, hi)) = inject.latency_ms {
                    latency += Duration::from_millis(rng.random_range(lo..=hi));
                }
                if inject.reset {
                    self.record(&mut state, rule, method, path, counter, &key);
                    return Decision {
                        latency,
                        action: Action::Reset,
                    };
                }
                if let Some(status) = inject.status {
                    self.record(&mut state, rule, method, path, counter, &key);
                    let headers = inject
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let body = Bytes::from(inject.body.clone().unwrap_or_default());
                    return Decision {
                        latency,
                        action: Action::Inject {
                            status,
                            headers,
                            body,
                        },
                    };
                }
                // Latency alone: it composes with what fires next.
                self.record(&mut state, rule, method, path, counter, &key);
            }
        }
        Decision {
            latency,
            action: Action::Pass,
        }
    }

    fn when_holds(
        &self,
        rule: &crate::rules::Rule,
        state: &RuleState,
        key: &str,
        rng: &mut ChaCha8Rng,
    ) -> bool {
        let Some(w) = &rule.when else { return true };
        if let Some(max) = w.max_total
            && state.fired_total >= max
        {
            return false;
        }
        if let Some(max) = w.max_per_path
            && state.fired_per_path.get(key).copied().unwrap_or(0) >= max
        {
            return false;
        }
        if let Some(n) = w.first_n
            && state.matched > n
        {
            return false;
        }
        if let Some(n) = w.every_nth
            && !state.matched.is_multiple_of(n)
        {
            return false;
        }
        if let Some(p) = w.probability
            && rng.random::<f64>() >= p
        {
            return false;
        }
        true
    }

    fn record(
        &self,
        state: &mut RuleState,
        rule: &crate::rules::Rule,
        method: &Method,
        path: &str,
        counter: u64,
        key: &str,
    ) {
        state.fired_total += 1;
        *state.fired_per_path.entry(key.to_string()).or_insert(0) += 1;
        tracing::debug!(rule = %rule.name, %method, path, counter, "faultproxy: rule fired");
        self.fired.lock().unwrap().push(Fired {
            rule: rule.name.clone(),
            method: method.to_string(),
            path: path.to_string(),
            counter,
        });
    }

    /// The stale answer for a read, if a window covers it.
    fn stale_answer(&self, path: &str, stale: &crate::rules::Stale) -> Option<Action> {
        let store = self.stale.lock().unwrap();
        let key = match stale.after_mutation_on {
            StaleTrigger::SamePath => path.to_string(),
            StaleTrigger::SameRepo | StaleTrigger::GitReceivePackSameRepo => repo_prefix(path)?,
        };
        let mutated = *store.mutated.get(&(stale.after_mutation_on, key))?;
        if mutated.elapsed().as_secs_f64() >= stale.for_secs {
            return None;
        }
        if let Some(body) = &stale.body {
            return Some(Action::ServeStale {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: Bytes::from(body.clone()),
            });
        }
        match store.snapshots.get(path) {
            Some(s) => Some(Action::ServeStale {
                status: s.status,
                headers: s.headers.clone(),
                body: s.body.clone(),
            }),
            None => match stale.missing_as {
                MissingAs::NotFound => Some(Action::ServeStale {
                    status: 404,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Bytes::from_static(br#"{"message":"not found (stale)"}"#),
                }),
                MissingAs::Pass => None,
            },
        }
    }

    /// Records a forwarded answer: a 2xx GET is snapshotted (unless a window already covers
    /// the path, in which case the stale value stays, as a cache's would); a successful
    /// mutation opens windows for its path and its repository.
    pub fn observe(&self, method: &Method, path: &str, resp: &Response<Bytes>) {
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return;
        }
        if method == Method::GET || method == Method::HEAD {
            let mut store = self.stale.lock().unwrap();
            if self.window_open(&store, path) {
                return;
            }
            let headers = resp
                .headers()
                .iter()
                .filter(|(k, _)| !is_hop_by_hop(k.as_str()) && k.as_str() != "content-length")
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
                .collect();
            store.snapshots.insert(
                path.to_string(),
                Snapshot {
                    status,
                    headers,
                    body: resp.body().clone(),
                },
            );
            return;
        }
        self.observe_mutation(method, path, status);
    }

    /// The lightweight half of `observe`, for traffic whose body is streamed rather than
    /// buffered: opens the windows a successful mutation opens.
    pub fn observe_mutation(&self, method: &Method, path: &str, status: u16) {
        if method == Method::GET || method == Method::HEAD || !(200..300).contains(&status) {
            return;
        }
        let now = Instant::now();
        let mut store = self.stale.lock().unwrap();
        store
            .mutated
            .insert((StaleTrigger::SamePath, path.to_string()), now);
        if let Some(prefix) = repo_prefix(path) {
            store
                .mutated
                .insert((StaleTrigger::SameRepo, prefix.clone()), now);
            if path.ends_with("/git-receive-pack") {
                store
                    .mutated
                    .insert((StaleTrigger::GitReceivePackSameRepo, prefix), now);
            }
        }
    }

    fn window_open(&self, store: &StaleStore, path: &str) -> bool {
        let prefix = repo_prefix(path);
        self.rules
            .rules
            .iter()
            .filter_map(|r| r.stale.as_ref())
            .any(|s| {
                let key = match s.after_mutation_on {
                    StaleTrigger::SamePath => Some(path.to_string()),
                    _ => prefix.clone(),
                };
                key.and_then(|k| store.mutated.get(&(s.after_mutation_on, k)))
                    .is_some_and(|t| t.elapsed().as_secs_f64() < s.for_secs)
            })
    }
}

/// The API path that names a repository, for any path about it: `/api/v1/repos/{o}/{r}`
/// (Forgejo, Gitea), `/repos/{o}/{r}` (GitHub), `/api/v4/projects/{id}` (GitLab),
/// `/{org}/{project}/_apis/git/repositories/{repo}` (Azure DevOps). A git smart-HTTP path
/// `/{o}/{r}.git/...` is mapped to the Forgejo layout, the forge this layer runs in front of.
pub fn repo_prefix(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let join = |n: usize| Some(format!("/{}", segs[..n].join("/")));
    match segs.as_slice() {
        ["api", "v1", "repos", _o, _r, ..] => join(5),
        ["repos", _o, _r, ..] => join(3),
        ["api", "v4", "projects", _id, ..] => join(4),
        [_org, _project, "_apis", "git", "repositories", _repo, ..] => join(6),
        [o, r, ..] if r.ends_with(".git") => {
            Some(format!("/api/v1/repos/{o}/{}", r.trim_end_matches(".git")))
        }
        _ => None,
    }
}

/// Headers that belong to one connection and must not be copied to another.
pub fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// FNV-1a over the parts, with a separator so that ("ab","c") and ("a","bc") differ. Chosen
/// over the standard library's hasher because it is the same in every Rust version.
fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.iter().chain(std::iter::once(&0xffu8)) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Inject, Match, Rule, Scope, Stale, When};

    fn rule(name: &str, when: Option<When>, inject: Inject) -> Rule {
        Rule {
            name: name.into(),
            match_: Match {
                methods: None,
                path: Some("/api/**".into()),
                scope: Scope::Api,
            },
            when,
            inject: Some(inject),
            stale: None,
        }
    }

    fn status(s: u16) -> Inject {
        Inject {
            status: Some(s),
            ..Default::default()
        }
    }

    fn sequence(engine: &FaultEngine) -> Vec<Action> {
        (0..40)
            .map(|i| {
                engine
                    .decide(&Method::GET, &format!("/api/v1/repos/o/r{}", i % 5))
                    .action
            })
            .collect()
    }

    #[test]
    fn same_seed_same_decisions() {
        let rules = || {
            Rules::builder()
                .rule(rule(
                    "p",
                    Some(When {
                        probability: Some(0.4),
                        ..Default::default()
                    }),
                    status(503),
                ))
                .build()
        };
        let a = sequence(&FaultEngine::new(rules(), Some(7)));
        let b = sequence(&FaultEngine::new(rules(), Some(7)));
        let c = sequence(&FaultEngine::new(rules(), Some(8)));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.iter().any(|x| *x != Action::Pass) && a.contains(&Action::Pass));
        assert_eq!(
            FaultEngine::new(Rules::builder().seed(3).build(), None).seed(),
            3
        );
    }

    #[test]
    fn decisions_depend_on_the_paths_own_history_not_arrival_order() {
        let rules = || {
            Rules::builder()
                .rule(rule(
                    "p",
                    Some(When {
                        probability: Some(0.5),
                        ..Default::default()
                    }),
                    status(503),
                ))
                .build()
        };
        let e1 = FaultEngine::new(rules(), Some(1));
        let e2 = FaultEngine::new(rules(), Some(1));
        let a: Vec<_> = ["/api/a", "/api/b", "/api/a", "/api/b"]
            .iter()
            .map(|p| (p, e1.decide(&Method::GET, p).action))
            .collect();
        let b: Vec<_> = ["/api/b", "/api/a", "/api/b", "/api/a"]
            .iter()
            .map(|p| (p, e2.decide(&Method::GET, p).action))
            .collect();
        // Per path, the n-th decision is the same whatever else came between.
        let pick = |v: &Vec<(&&str, Action)>, p: &str| -> Vec<Action> {
            v.iter()
                .filter(|(q, _)| **q == p)
                .map(|(_, a)| a.clone())
                .collect()
        };
        assert_eq!(pick(&a, "/api/a"), pick(&b, "/api/a"));
        assert_eq!(pick(&a, "/api/b"), pick(&b, "/api/b"));
    }

    #[test]
    fn counters() {
        let e = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "first",
                    Some(When {
                        first_n: Some(2),
                        ..Default::default()
                    }),
                    status(500),
                ))
                .build(),
            Some(1),
        );
        let got: Vec<bool> = (0..5)
            .map(|_| e.decide(&Method::GET, "/api/x").action != Action::Pass)
            .collect();
        assert_eq!(got, [true, true, false, false, false]);

        let e = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "nth",
                    Some(When {
                        every_nth: Some(3),
                        ..Default::default()
                    }),
                    status(500),
                ))
                .build(),
            Some(1),
        );
        let got: Vec<bool> = (0..7)
            .map(|_| e.decide(&Method::GET, "/api/x").action != Action::Pass)
            .collect();
        assert_eq!(got, [false, false, true, false, false, true, false]);

        let e = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "cap",
                    Some(When {
                        max_per_path: Some(2),
                        max_total: Some(3),
                        ..Default::default()
                    }),
                    status(500),
                ))
                .build(),
            Some(1),
        );
        let fired = |p: &str| e.decide(&Method::GET, p).action != Action::Pass;
        assert_eq!(
            [fired("/api/a"), fired("/api/a"), fired("/api/a")],
            [true, true, false]
        );
        assert_eq!(
            [fired("/api/b"), fired("/api/b")],
            [true, false],
            "max_total reached after three"
        );
        assert_eq!(e.fired().len(), 3);
        assert_eq!(
            e.fired()[0],
            Fired {
                rule: "cap".into(),
                method: "GET".into(),
                path: "/api/a".into(),
                counter: 0
            }
        );
    }

    #[test]
    fn latency_composes_and_injects_carry_headers_and_body() {
        let mut inj = status(403);
        inj.headers.insert("Retry-After".into(), "2".into());
        inj.body = Some("limited".into());
        let e = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "slow",
                    None,
                    Inject {
                        latency_ms: Some((100, 100)),
                        ..Default::default()
                    },
                ))
                .rule(rule("limit", None, inj))
                .build(),
            Some(1),
        );
        let d = e.decide(&Method::POST, "/api/x");
        assert_eq!(d.latency, Duration::from_millis(100));
        assert_eq!(
            d.action,
            Action::Inject {
                status: 403,
                headers: vec![("Retry-After".into(), "2".into())],
                body: Bytes::from_static(b"limited")
            }
        );
        let e = FaultEngine::new(
            Rules::builder()
                .rule(rule(
                    "reset",
                    None,
                    Inject {
                        reset: true,
                        ..Default::default()
                    },
                ))
                .build(),
            Some(1),
        );
        assert_eq!(e.decide(&Method::GET, "/api/x").action, Action::Reset);
        assert_eq!(
            e.decide(&Method::GET, "/o/r.git/info/refs").action,
            Action::Pass,
            "api scope leaves git alone"
        );
    }

    fn ok(body: &str) -> Response<Bytes> {
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Bytes::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn stale_windows_open_on_mutation_and_close_after_for_secs() {
        let e = FaultEngine::new(
            Rules::builder()
                .rule(Rule {
                    name: "stale".into(),
                    match_: Match {
                        methods: Some(vec!["GET".into()]),
                        path: Some("/api/v1/repos/*/*".into()),
                        scope: Scope::Api,
                    },
                    when: None,
                    inject: None,
                    stale: Some(Stale {
                        after_mutation_on: StaleTrigger::SameRepo,
                        for_secs: 3.0,
                        missing_as: MissingAs::NotFound,
                        body: None,
                    }),
                })
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
                        for_secs: 2.0,
                        missing_as: MissingAs::Pass,
                        body: Some("null".into()),
                    }),
                })
                .build(),
            Some(1),
        );
        let repo = "/api/v1/repos/o/r";
        // Nothing mutated yet: reads pass.
        assert_eq!(e.decide(&Method::GET, repo).action, Action::Pass);
        e.observe(&Method::GET, repo, &ok(r#"{"archived":false}"#));
        // A mutation on the repository opens the window; the read is the old answer.
        e.observe(&Method::PATCH, repo, &ok(r#"{"archived":true}"#));
        match e.decide(&Method::GET, repo).action {
            Action::ServeStale {
                status: 200, body, ..
            } => assert_eq!(body, r#"{"archived":false}"#),
            other => panic!("{other:?}"),
        }
        // A fresh answer forwarded during the window does not replace the stale snapshot.
        e.observe(&Method::GET, repo, &ok(r#"{"archived":true}"#));
        assert!(matches!(
            e.decide(&Method::GET, repo).action,
            Action::ServeStale { .. }
        ));
        // A path never seen answers 404 during the window.
        assert!(
            matches!(
                e.decide(&Method::GET, "/api/v1/repos/o/r2").action,
                Action::Pass
            ),
            "another repo is unaffected"
        );
        e.observe(&Method::POST, "/api/v1/repos/o/new/topics", &ok("{}"));
        assert!(matches!(
            e.decide(&Method::GET, "/api/v1/repos/o/new").action,
            Action::ServeStale { status: 404, .. }
        ));
        // The window closes.
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(e.decide(&Method::GET, repo).action, Action::Pass);
        assert_eq!(
            e.decide(&Method::GET, "/api/v1/repos/o/new").action,
            Action::Pass
        );

        // A push opens the receive-pack window: the branch listing answers null for 2s.
        e.observe_mutation(&Method::POST, "/o/r.git/git-receive-pack", 200);
        match e.decide(&Method::GET, "/api/v1/repos/o/r/branches").action {
            Action::ServeStale {
                status: 200, body, ..
            } => assert_eq!(body, "null"),
            other => panic!("{other:?}"),
        }
        tokio::time::advance(Duration::from_millis(2100)).await;
        assert_eq!(
            e.decide(&Method::GET, "/api/v1/repos/o/r/branches").action,
            Action::Pass
        );
        assert!(e.fired().iter().any(|f| f.rule == "null-branches"));
    }

    #[test]
    fn repo_prefixes() {
        assert_eq!(
            repo_prefix("/api/v1/repos/o/r/branches").as_deref(),
            Some("/api/v1/repos/o/r")
        );
        assert_eq!(
            repo_prefix("/repos/o/r/pulls").as_deref(),
            Some("/repos/o/r")
        );
        assert_eq!(
            repo_prefix("/api/v4/projects/a%2Fb/repository/branches").as_deref(),
            Some("/api/v4/projects/a%2Fb")
        );
        assert_eq!(
            repo_prefix("/org/proj/_apis/git/repositories/x/refs").as_deref(),
            Some("/org/proj/_apis/git/repositories/x")
        );
        assert_eq!(
            repo_prefix("/o/r.git/git-receive-pack").as_deref(),
            Some("/api/v1/repos/o/r")
        );
        assert_eq!(repo_prefix("/api/v1/orgs/o"), None);
    }
}
