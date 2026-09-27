//! The determinism gate: the Rust port must produce the committed locks byte for byte.
//!
//! `examples/fleet` is always checked. The `shapes` and `scale` fleets are checked when
//! `FORGELAB_FLEETS_DIR` points at a checkout of repoplane/fleets (default: `../fleets` next to
//! this repository), and skipped loudly otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forgelab::fleet::{self, Lock};
use forgelab::seed;

async fn lock_bytes(root: &Path) -> Vec<u8> {
    let spec = fleet::load_spec(root).unwrap_or_else(|e| panic!("{}: {e}", root.display()));
    let digest = fleet::digest(root).unwrap();
    let sem = Arc::new(tokio::sync::Semaphore::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for r in spec.repos.iter().filter(|r| !r.empty).cloned() {
        let root = root.to_path_buf();
        let id = spec.git.clone();
        let sem = sem.clone();
        tasks.spawn(async move {
            let _p = sem.acquire().await.unwrap();
            let b = seed::build(&root, &r, &id).await.unwrap_or_else(|e| panic!("{}: {e}", r.name));
            (r.name, b.sha)
        });
    }
    let mut baselines = HashMap::new();
    while let Some(res) = tasks.join_next().await {
        let (name, sha) = res.unwrap();
        baselines.insert(name, sha);
    }
    Lock::new(&spec, &digest, &baselines).marshal()
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    for (i, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}:\n  got:  {x}\n  want: {y}", i + 1);
        }
    }
    format!("lengths differ: got {} lines, want {}", a.lines().count(), b.lines().count())
}

async fn check(root: &Path) {
    let want = std::fs::read(root.join(fleet::LOCK_FILE)).unwrap();
    let got = lock_bytes(root).await;
    assert!(
        got == want,
        "{}/{} is not reproduced byte for byte: {}",
        root.display(),
        fleet::LOCK_FILE,
        first_difference(&got, &want)
    );
    // And the digest alone is stable across a second read.
    assert_eq!(fleet::digest(root).unwrap(), fleet::digest(root).unwrap());
}

fn examples_fleet() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/fleet")
}

fn fleets_dir() -> Option<PathBuf> {
    let dir = match std::env::var_os("FORGELAB_FLEETS_DIR") {
        Some(d) => PathBuf::from(d),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fleets"),
    };
    if dir.join("shapes").join(fleet::LOCK_FILE).exists() {
        Some(dir)
    } else {
        eprintln!(
            "golden_lock: skipping shapes and scale: no fleets checkout at {} (set FORGELAB_FLEETS_DIR)",
            dir.display()
        );
        None
    }
}

#[tokio::test]
async fn examples_fleet_lock_is_reproduced() {
    check(&examples_fleet()).await;
}

#[tokio::test]
async fn shapes_fleet_lock_is_reproduced() {
    if let Some(dir) = fleets_dir() {
        check(&dir.join("shapes")).await;
    }
}

#[tokio::test]
async fn scale_fleet_lock_is_reproduced() {
    if let Some(dir) = fleets_dir() {
        check(&dir.join("scale")).await;
    }
}

/// The committed lock, parsed and written back, is unchanged: parsing loses nothing and the
/// formatter matches Go's.
#[test]
fn committed_lock_round_trips() {
    let raw = std::fs::read(examples_fleet().join(fleet::LOCK_FILE)).unwrap();
    let lock = fleet::parse_lock(&raw).unwrap();
    assert_eq!(lock.marshal(), raw);
}
