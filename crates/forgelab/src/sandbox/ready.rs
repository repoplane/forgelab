//! Waiting for a forge to reflect a push. Health is not readiness: a push returns before the
//! forge has finished reflecting it, and for a short window afterwards a branch listing answers
//! 200 with a null body -- not an error, just silently empty. Read naively that is "this
//! repository has no branches".

use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::fleet::LockRepo;
use crate::forge::{Forge, ForgeError, Ref};
use crate::util::{go_duration, short};

use super::Env;
use super::pool::for_each_collect;
use super::report::{CommandError, aggregate};

/// How long one repository has to reflect its push. It is spent per repository and not per
/// fleet: how slow one repository was says nothing about the next, and a budget shared across
/// the fleet fails whichever repository happens to be holding it when the time runs out --
/// naming that repository in the error, though it may have been answering perfectly.
pub const READY_PER_REPO: Duration = Duration::from_secs(60);

/// Stops a fleet from waiting on a forge that is not coming back. A per-repository bound
/// multiplies: at eight workers, a hundred repositories against a dead forge would otherwise
/// sit for the better part of a quarter of an hour.
pub const READY_OVERALL: Duration = Duration::from_secs(10 * 60);

/// The only thing the wait asks of a forge. It is named here so the wait can be tested without
/// standing up everything else a forge does.
#[async_trait]
pub trait BranchLister: Send + Sync {
    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError>;
}

#[async_trait]
impl BranchLister for dyn Forge {
    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError> {
        Forge::branches(self, name).await
    }
}

impl Env {
    /// Blocks until each repository reports its default branch at the baseline.
    ///
    /// Where the forge looks several repositories up at once and says where their default
    /// branches point, one batched look settles most of them; only those it has not caught up
    /// with yet are then waited for one at a time.
    pub(crate) async fn wait_ready(&self, repos: &[LockRepo]) -> Result<(), CommandError> {
        let overall = Instant::now() + READY_OVERALL;
        let unreadable = self.forge.caps().archived_unreadable;
        let mut pending: Vec<LockRepo> = repos.to_vec();
        if self.forge.batch_size() > 1 {
            let names: Vec<String> = pending.iter().map(|r| r.name.clone()).collect();
            // Only a shortcut: if the batched look fails, every repository is waited for.
            if let Ok(found) = self.lookup(&names).await {
                pending = pending
                    .into_iter()
                    .zip(found)
                    .filter(|(want, live)| {
                        !live.as_ref().is_some_and(|l| {
                            l.default_branch == want.default_branch
                                && l.head.as_deref() == Some(want.baseline.as_str())
                        })
                    })
                    .map(|(want, _)| want)
                    .collect();
            }
        }
        self.progress.phase("wait", pending.len());
        let results =
            for_each_collect(pending, self.concurrency, &self.cancel, |want| async move {
                if want.empty || (want.archived && unreadable) {
                    self.progress.add(1);
                    return (want, Ok(()));
                }
                let forge: &dyn Forge = self.forge.as_ref();
                let r = wait_one(forge, &want, READY_PER_REPO, overall, &self.cancel).await;
                self.progress.add(1);
                (want, r)
            })
            .await;
        aggregate(results, |w| &w.name).map(drop)
    }
}

impl Env {
    /// Waits until `name` reports `branch` at `sha`: before writing settings to a repository
    /// this run has just pushed to. Forgejo takes a push in at once and applies it in the
    /// background -- that is when a first push clears `is_empty` -- while every settings write
    /// (`PATCH /repos/{owner}/{repo}`, archiving included) saves the whole repository row as it
    /// was read when that request began. A settings write that lands in between puts
    /// `is_empty` back: the repository then reads as empty for good, and no wait for its
    /// branch ever ends.
    pub(crate) async fn wait_pushed(
        &self,
        name: &str,
        branch: &str,
        sha: &str,
    ) -> Result<(), CommandError> {
        let want = LockRepo {
            name: name.to_string(),
            default_branch: branch.to_string(),
            baseline: sha.to_string(),
            ..LockRepo::default()
        };
        let forge: &dyn Forge = self.forge.as_ref();
        wait_one(
            forge,
            &want,
            READY_PER_REPO,
            Instant::now() + READY_OVERALL,
            &self.cancel,
        )
        .await
    }
}

/// Blocks until `want` reports its baseline. It gives up on its own clock, or on the fleet's,
/// and says which -- the two are different failures and want different answers.
pub(crate) async fn wait_one<L: BranchLister + ?Sized>(
    f: &L,
    want: &LockRepo,
    per_repo: Duration,
    overall: Instant,
    cancel: &CancellationToken,
) -> Result<(), CommandError> {
    let deadline = Instant::now() + per_repo;
    // Quick first looks for a local forge, backing off to a second for a hosted one.
    let mut pause = Duration::from_millis(25);
    loop {
        let answer = f.branches(&want.name).await;
        if let Ok(branches) = &answer
            && branches
                .iter()
                .any(|r| r.name == want.default_branch && r.sha == want.baseline)
        {
            return Ok(());
        }
        let now = Instant::now();
        if now >= overall {
            return Err(CommandError::Other(format!(
                "gave up on the fleet after {}, waiting for {}",
                go_duration(READY_OVERALL),
                want.name
            )));
        }
        if now >= deadline {
            return Err(match answer {
                Err(e) => CommandError::Other(format!("{} not ready: {e}", want.name)),
                Ok(_) => CommandError::Other(format!(
                    "{} never reported {} at {}",
                    want.name,
                    want.default_branch,
                    short(&want.baseline)
                )),
            });
        }
        tokio::select! {
            _ = tokio::time::sleep(pause) => {}
            _ = cancel.cancelled() => return Err(CommandError::Other("interrupted".into())),
        }
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Answers with the baseline only from the nth call onwards, which is the shape of a forge
    /// that has taken the push but not yet reflected it.
    struct Branches {
        ready: i32,
        calls: AtomicI32,
        err: Option<String>,
    }

    impl Branches {
        fn after(ready: i32) -> Self {
            Branches {
                ready,
                calls: AtomicI32::new(0),
                err: None,
            }
        }
    }

    #[async_trait]
    impl BranchLister for Branches {
        async fn branches(&self, _: &str) -> Result<Vec<Ref>, ForgeError> {
            if let Some(e) = &self.err {
                return Err(ForgeError::msg(e.clone()));
            }
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.ready {
                return Ok(vec![]); // 200 with nothing in it: not an error, just not ready
            }
            Ok(vec![Ref {
                name: "main".into(),
                sha: "abc123".into(),
            }])
        }
    }

    fn repo() -> LockRepo {
        LockRepo {
            name: "svc".into(),
            default_branch: "main".into(),
            visibility: "private".into(),
            topics: vec![],
            archived: false,
            empty: false,
            tags: vec![],
            baseline: "abc123".into(),
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[tokio::test(start_paused = true)]
    async fn returns_once_reflected() {
        wait_one(
            &Branches::after(3),
            &repo(),
            Duration::from_secs(60),
            far(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn names_the_repository_it_waited_for() {
        let err = wait_one(
            &Branches::after(1 << 30),
            &repo(),
            Duration::from_millis(40),
            far(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("svc never reported main at abc123"),
            "{err}"
        );
    }

    /// An error from the forge is reported as itself rather than as "never reported": the two
    /// send a reader to different places.
    #[tokio::test(start_paused = true)]
    async fn surfaces_the_forge_error() {
        let b = Branches {
            ready: 0,
            calls: AtomicI32::new(0),
            err: Some("boom".into()),
        };
        let err = wait_one(
            &b,
            &repo(),
            Duration::from_millis(40),
            far(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("svc not ready: boom"), "{err}");
    }

    /// The fleet's ceiling and a repository's own clock are different failures.
    #[tokio::test(start_paused = true)]
    async fn distinguishes_the_fleet_ceiling() {
        let err = wait_one(
            &Branches::after(1 << 30),
            &repo(),
            Duration::from_secs(3600),
            Instant::now(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("gave up on the fleet after 10m0s"),
            "{err}"
        );
        assert!(!err.to_string().contains("never reported"));
    }

    /// Every call starts its own clock, so a call after an exhausted one is unaffected.
    #[tokio::test(start_paused = true)]
    async fn budget_is_not_shared_between_repositories() {
        let (overall, per_repo) = (far(), Duration::from_millis(200));
        assert!(
            wait_one(
                &Branches::after(1 << 30),
                &repo(),
                per_repo,
                overall,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        wait_one(
            &Branches::after(1),
            &repo(),
            per_repo,
            overall,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stops_on_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = wait_one(
            &Branches::after(1 << 30),
            &repo(),
            Duration::from_secs(3600),
            far(),
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "interrupted");
    }
}
