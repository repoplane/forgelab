//! The rules: what to fault, when, and how. Loadable from YAML, or built in code by a test.

use std::collections::BTreeMap;

use http::Method;
use serde::{Deserialize, Serialize};

/// A rule file: an optional seed and the rules, evaluated in order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// One rule. A rule with `inject` fires a fault; one with `stale` answers from the stale store
/// while a window is open. Both may carry `when`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    #[serde(rename = "match", default)]
    pub match_: Match,
    #[serde(default)]
    pub when: Option<When>,
    #[serde(default)]
    pub inject: Option<Inject>,
    #[serde(default)]
    pub stale: Option<Stale>,
}

/// Which requests a rule looks at.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// HTTP methods, upper case. None means any.
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    /// A glob on the path: `*` is one path segment (or part of one), `**` any number of
    /// segments. None means any path within the scope.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub scope: Scope,
}

/// The kind of traffic a rule applies to.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Paths under `/api/`.
    #[default]
    Api,
    /// git smart-HTTP: `*.git/...`, `/info/refs`, `/git-upload-pack`, `/git-receive-pack`.
    Git,
    Any,
}

impl Scope {
    pub fn covers(self, path: &str) -> bool {
        match self {
            Scope::Api => path.starts_with("/api/"),
            Scope::Git => is_git_path(path),
            Scope::Any => true,
        }
    }
}

/// Whether a path is git smart-HTTP traffic.
pub fn is_git_path(path: &str) -> bool {
    path.contains(".git/")
        || path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
}

/// How often a matching request is faulted. Every condition present must hold.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// Fire with this probability, decided by the seeded generator.
    #[serde(default)]
    pub probability: Option<f64>,
    /// Fire on every n-th matching request of this rule.
    #[serde(default)]
    pub every_nth: Option<u64>,
    /// Fire on the first n matching requests of this rule.
    #[serde(default)]
    pub first_n: Option<u64>,
    /// Never fire more than this many times in all.
    #[serde(default)]
    pub max_total: Option<u64>,
    /// Never fire more than this many times on one `METHOD path`.
    #[serde(default)]
    pub max_per_path: Option<u64>,
}

/// The fault: a synthetic answer, a dropped connection, or added latency (which composes with
/// whatever fires next).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub reset: bool,
    /// Added latency, milliseconds, drawn uniformly from the range.
    #[serde(default)]
    pub latency_ms: Option<(u64, u64)>,
}

/// A stale read: after a mutation, answer reads with what was there before it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stale {
    pub after_mutation_on: StaleTrigger,
    pub for_secs: f64,
    #[serde(default)]
    pub missing_as: MissingAs,
    /// Replaces the body while keeping a 200: Forgejo answers a branch listing with `null` for
    /// a moment after a push.
    #[serde(default)]
    pub body: Option<String>,
}

/// What opens a stale window for a read.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StaleTrigger {
    /// Any successful mutation on any API path of the same repository.
    SameRepo,
    /// A successful `git-receive-pack` (push) to the same repository.
    GitReceivePackSameRepo,
    /// A successful mutation on exactly this path.
    SamePath,
}

/// What to answer during a window when no snapshot of the path exists yet.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissingAs {
    #[default]
    NotFound,
    Pass,
}

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("parse rules: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    #[error("rule {rule:?}: {msg}")]
    Invalid { rule: String, msg: String },
}

impl Rules {
    pub fn from_yaml(text: &str) -> Result<Rules, RulesError> {
        let rules: Rules = serde_yaml_ng::from_str(text)?;
        rules.validate()?;
        Ok(rules)
    }

    pub fn builder() -> RulesBuilder {
        RulesBuilder::default()
    }

    pub fn validate(&self) -> Result<(), RulesError> {
        for r in &self.rules {
            let invalid = |msg: &str| RulesError::Invalid {
                rule: r.name.clone(),
                msg: msg.to_string(),
            };
            if r.name.is_empty() {
                return Err(RulesError::Invalid {
                    rule: String::new(),
                    msg: "a rule needs a name".into(),
                });
            }
            if r.inject.is_none() && r.stale.is_none() {
                return Err(invalid("needs inject or stale"));
            }
            if r.inject.is_some() && r.stale.is_some() {
                return Err(invalid("inject and stale are exclusive"));
            }
            if let Some(w) = &r.when {
                if let Some(p) = w.probability
                    && !(0.0..=1.0).contains(&p)
                {
                    return Err(invalid("probability must be within 0 and 1"));
                }
                if w.every_nth == Some(0) {
                    return Err(invalid("every_nth must be at least 1"));
                }
            }
            if let Some(i) = &r.inject {
                if i.status.is_none() && !i.reset && i.latency_ms.is_none() {
                    return Err(invalid("inject needs a status, reset or latency_ms"));
                }
                if let Some((lo, hi)) = i.latency_ms
                    && lo > hi
                {
                    return Err(invalid("latency_ms range is reversed"));
                }
                if let Some(s) = i.status
                    && !(100..600).contains(&s)
                {
                    return Err(invalid("status must be an HTTP status code"));
                }
            }
            if let Some(s) = &r.stale
                && s.for_secs <= 0.0
            {
                return Err(invalid("for_secs must be positive"));
            }
            if let Some(m) = &r.match_.methods {
                for m in m {
                    if m.parse::<Method>().is_err() {
                        return Err(invalid(&format!("unknown method {m:?}")));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Builds rules in code.
#[derive(Debug, Default)]
pub struct RulesBuilder {
    rules: Rules,
}

impl RulesBuilder {
    pub fn seed(mut self, seed: u64) -> Self {
        self.rules.seed = Some(seed);
        self
    }

    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.rules.push(rule);
        self
    }

    pub fn build(self) -> Rules {
        self.rules
    }
}

impl Rule {
    /// Whether the rule looks at this request at all (scope, method, path).
    pub fn matches(&self, method: &Method, path: &str) -> bool {
        if !self.match_.scope.covers(path) {
            return false;
        }
        if let Some(methods) = &self.match_.methods
            && !methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method.as_str()))
        {
            return false;
        }
        match &self.match_.path {
            Some(glob) => glob_matches(glob, path),
            None => true,
        }
    }
}

/// Path globbing: `**` matches any number of segments, `*` any run of characters within one
/// segment. Both sides are split on `/`.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let g: Vec<&str> = glob.trim_start_matches('/').split('/').collect();
    let p: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    segments_match(&g, &p)
}

fn segments_match(g: &[&str], p: &[&str]) -> bool {
    match g.split_first() {
        None => p.is_empty(),
        Some((&"**", rest)) => (0..=p.len()).any(|i| segments_match(rest, &p[i..])),
        Some((first, rest)) => match p.split_first() {
            Some((seg, prest)) => segment_matches(first, seg) && segments_match(rest, prest),
            None => false,
        },
    }
}

fn segment_matches(pattern: &str, seg: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == seg;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = seg;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = r#"
seed: 42
rules:
  - name: gh-secondary
    match: { methods: [POST, PATCH, PUT, DELETE], path: "/api/**" }
    when:  { probability: 0.03, max_per_path: 3 }
    inject: { status: 403, headers: { X-RateLimit-Remaining: "4264" }, body: '{"message":"You have exceeded a secondary rate limit ..."}' }
  - { name: 429-bare,  match: { path: "/api/**" }, when: { probability: 0.02 }, inject: { status: 429 } }
  - { name: 429-ra,    match: { path: "/api/**" }, when: { probability: 0.02 }, inject: { status: 429, headers: { Retry-After: "2" } } }
  - { name: flaky-5xx, match: { path: "/api/**" }, when: { probability: 0.05 }, inject: { status: 503 } }
  - { name: resets,    match: { scope: any },      when: { probability: 0.01 }, inject: { reset: true } }
  - { name: slow,      match: { scope: any },      when: { probability: 0.3 },  inject: { latency_ms: [50, 800] } }
  - name: stale-repo-after-write
    match: { methods: [GET], path: "/api/v1/repos/*/*" }
    stale: { after_mutation_on: same_repo, for_secs: 3, missing_as: not_found }
  - name: null-branches-after-push
    match: { methods: [GET], path: "/api/v1/repos/*/*/branches" }
    stale: { after_mutation_on: git_receive_pack_same_repo, for_secs: 2, body: "null" }
  - { name: ado-signin, match: { scope: api }, when: { every_nth: 50 }, inject: { status: 203, body: "<html>Sign in</html>" } }
"#;

    #[test]
    fn parses_the_example() {
        let r = Rules::from_yaml(EXAMPLE).unwrap();
        assert_eq!(r.seed, Some(42));
        assert_eq!(r.rules.len(), 9);
        assert_eq!(r.rules[0].match_.methods.as_ref().unwrap().len(), 4);
        assert_eq!(
            r.rules[0].inject.as_ref().unwrap().headers["X-RateLimit-Remaining"],
            "4264"
        );
        assert_eq!(
            r.rules[5].inject.as_ref().unwrap().latency_ms,
            Some((50, 800))
        );
        assert_eq!(
            r.rules[6].stale.as_ref().unwrap().after_mutation_on,
            StaleTrigger::SameRepo
        );
        assert_eq!(
            r.rules[7].stale.as_ref().unwrap().body.as_deref(),
            Some("null")
        );
        assert_eq!(r.rules[4].match_.scope, Scope::Any);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(
            Rules::from_yaml("rules:\n  - { name: x, matches: {}, inject: { status: 500 } }\n")
                .is_err()
        );
        assert!(
            Rules::from_yaml(
                "rules:\n  - { name: x, when: { probability: 1.5 }, inject: { status: 500 } }\n"
            )
            .is_err()
        );
        assert!(Rules::from_yaml("rules:\n  - { name: x, inject: {} }\n").is_err());
        assert!(Rules::from_yaml("rules:\n  - { name: x }\n").is_err());
        assert!(
            Rules::from_yaml(
                "rules:\n  - { name: x, when: { every_nth: 0 }, inject: { status: 500 } }\n"
            )
            .is_err()
        );
    }

    #[test]
    fn globs() {
        assert!(glob_matches("/api/**", "/api/v1/repos/o/r"));
        assert!(glob_matches("/api/**", "/api/"));
        assert!(glob_matches("/api/v1/repos/*/*", "/api/v1/repos/o/r"));
        assert!(!glob_matches(
            "/api/v1/repos/*/*",
            "/api/v1/repos/o/r/branches"
        ));
        assert!(glob_matches(
            "/api/v1/repos/*/*/branches",
            "/api/v1/repos/o/r/branches"
        ));
        assert!(glob_matches("**/*.git/**", "/o/r.git/git-receive-pack"));
        assert!(glob_matches("**/info/refs", "/o/r.git/info/refs"));
        assert!(!glob_matches("/repos/*", "/repos/a/b"));
    }

    #[test]
    fn scopes_and_matching() {
        assert!(Scope::Git.covers("/o/r.git/info/refs"));
        assert!(Scope::Git.covers("/o/r.git/git-receive-pack"));
        assert!(!Scope::Git.covers("/api/v1/repos/o/r"));
        assert!(Scope::Api.covers("/api/v1/repos/o/r"));
        assert!(!Scope::Api.covers("/o/r.git/info/refs"));
        let r = Rule {
            name: "x".into(),
            match_: Match {
                methods: Some(vec!["post".into()]),
                path: None,
                scope: Scope::Api,
            },
            when: None,
            inject: Some(Inject {
                status: Some(500),
                ..Default::default()
            }),
            stale: None,
        };
        assert!(r.matches(&Method::POST, "/api/v1/x"));
        assert!(!r.matches(&Method::GET, "/api/v1/x"));
        assert!(!r.matches(&Method::POST, "/o/r.git/info/refs"));
    }
}
