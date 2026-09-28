//! `fleet.lock.json`: the resolved fleet, byte-stable and committed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::FleetError;
use super::gojson;
use super::spec::{SPEC_VERSION, Spec};

/// The lock's fixed name inside a fleet directory.
pub const LOCK_FILE: &str = "fleet.lock.json";

/// The resolved fleet: every declared value after defaults are applied, plus the baseline SHAs
/// that only building the commits can reveal. It is a pure function of the fleet, so it is
/// byte-stable, committed, and the same on every forge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lock {
    pub version: i64,
    pub fleet_digest: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub repos: Vec<LockRepo>,
}

/// One repository as it is supposed to exist on a forge.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockRepo {
    pub name: String,
    pub default_branch: String,
    pub visibility: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub topics: Vec<String>,
    pub archived: bool,
    pub empty: bool,
    #[serde(
        default,
        deserialize_with = "null_as_empty",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,
    /// The seed commit. Empty for an empty repository.
    pub baseline: String,
}

fn null_as_empty<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

impl Lock {
    /// Resolves a spec into a lock. `baselines` maps repository name to seed commit SHA.
    pub fn new(
        spec: &Spec,
        digest: &str,
        baselines: &std::collections::HashMap<String, String>,
    ) -> Lock {
        Lock {
            version: SPEC_VERSION,
            fleet_digest: digest.to_string(),
            repos: spec
                .repos
                .iter()
                .map(|r| LockRepo {
                    name: r.name.clone(),
                    default_branch: r.default_branch.clone(),
                    visibility: r.visibility.clone(),
                    topics: r.topics.clone(),
                    archived: r.archived,
                    empty: r.empty,
                    tags: r.tags.clone(),
                    baseline: baselines.get(&r.name).cloned().unwrap_or_default(),
                })
                .collect(),
        }
    }

    /// The entry for a repository name.
    pub fn repo(&self, name: &str) -> Option<&LockRepo> {
        self.repos.iter().find(|r| r.name == name)
    }

    /// Renders the lock as formatted JSON with a trailing newline, byte-identical to what the
    /// Go implementation wrote, so that it is diff-friendly and stable under `git diff`.
    pub fn marshal(&self) -> Vec<u8> {
        let mut out = gojson::to_vec_pretty(self).expect("a lock always serialises");
        out.push(b'\n');
        out
    }

    /// Writes the lock atomically: to a temporary file beside it, then renamed into place, so
    /// that an interrupted run cannot leave a truncated lock behind.
    pub fn write_file(&self, path: &Path) -> Result<(), FleetError> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".fleet.lock.")
            .suffix(".tmp")
            .tempfile_in(dir)
            .map_err(|e| FleetError::io(format!("write {}", path.display()), e))?;
        std::io::Write::write_all(&mut tmp, &self.marshal())
            .and_then(|()| tmp.as_file().sync_all())
            .map_err(|e| FleetError::io(format!("write {}", path.display()), e))?;
        tmp.persist(path)
            .map_err(|e| FleetError::io(format!("write {}", path.display()), e.error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
        }
        Ok(())
    }
}

/// Reads and parses a lock file. A missing file is reported so that callers can say "run
/// apply" (see `FleetError::is_not_found`).
pub fn read_lock(path: &Path) -> Result<Lock, FleetError> {
    let raw =
        std::fs::read(path).map_err(|e| FleetError::io(format!("read {}", path.display()), e))?;
    parse_lock(&raw)
}

/// Decodes a lock from JSON.
///
/// An empty lock is rejected rather than returned. Everything downstream iterates over this
/// list, so an empty one would make every check run zero times and report success against a
/// completely unseeded sandbox.
pub fn parse_lock(raw: &[u8]) -> Result<Lock, FleetError> {
    let l: Lock =
        serde_json::from_slice(raw).map_err(|e| FleetError::invalid(format!("parse lock: {e}")))?;
    if l.version != SPEC_VERSION {
        return Err(FleetError::invalid(format!(
            "parse lock: version is {}, want {SPEC_VERSION}",
            l.version
        )));
    }
    if l.repos.is_empty() {
        return Err(FleetError::invalid(
            "parse lock: no repositories; the lock is empty or truncated",
        ));
    }
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        for raw in [
            r#"{"version":1,"repos":[]}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"repos":["#,
            r#"{"version":2,"repos":[{}]}"#,
        ] {
            assert!(parse_lock(raw.as_bytes()).is_err(), "{raw}");
        }
    }

    #[test]
    fn marshal_matches_go_layout() {
        let lock = Lock {
            version: 1,
            fleet_digest: "sha256:ab".into(),
            repos: vec![
                LockRepo {
                    name: "a".into(),
                    default_branch: "main".into(),
                    visibility: "private".into(),
                    topics: vec![],
                    archived: false,
                    empty: true,
                    tags: vec![],
                    baseline: String::new(),
                },
                LockRepo {
                    name: "b".into(),
                    default_branch: "main".into(),
                    visibility: "public".into(),
                    topics: vec!["x".into()],
                    archived: true,
                    empty: false,
                    tags: vec!["v1".into()],
                    baseline: "deadbeef".into(),
                },
            ],
        };
        let want = "{\n  \"version\": 1,\n  \"fleet_digest\": \"sha256:ab\",\n  \"repos\": [\n    {\n      \"name\": \"a\",\n      \"default_branch\": \"main\",\n      \"visibility\": \"private\",\n      \"topics\": [],\n      \"archived\": false,\n      \"empty\": true,\n      \"baseline\": \"\"\n    },\n    {\n      \"name\": \"b\",\n      \"default_branch\": \"main\",\n      \"visibility\": \"public\",\n      \"topics\": [\n        \"x\"\n      ],\n      \"archived\": true,\n      \"empty\": false,\n      \"tags\": [\n        \"v1\"\n      ],\n      \"baseline\": \"deadbeef\"\n    }\n  ]\n}\n";
        assert_eq!(String::from_utf8(lock.marshal()).unwrap(), want);
        assert_eq!(parse_lock(want.as_bytes()).unwrap(), lock);
    }

    #[test]
    fn write_is_atomic_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        let lock = parse_lock(br#"{"version":1,"fleet_digest":"d","repos":[{"name":"a","default_branch":"main","visibility":"private","topics":null,"archived":false,"empty":true,"baseline":""}]}"#).unwrap();
        lock.write_file(&path).unwrap();
        assert_eq!(read_lock(&path).unwrap(), lock);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file left behind"
        );
        assert!(
            read_lock(&dir.path().join("missing.json"))
                .unwrap_err()
                .is_not_found()
        );
    }
}
