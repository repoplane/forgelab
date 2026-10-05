//! verify: the comparison against the lock that every other command builds on.

use crate::fleet::{self, Lock, LockRepo};
use crate::forge::{self, Request};
use crate::seed;
use crate::util::{go_slice, short};

use super::pool::for_each_collect;
use super::report::{CommandError, aggregate};
use super::{Env, sorted};

/// One declared repository compared against the lock.
#[derive(Default)]
pub(crate) struct State {
    pub want: LockRepo,
    pub live: forge::Repo,

    /// Reasons forgelab must not touch this repository. Exit 2.
    pub guards: Vec<String>,
    /// Everything reset can repair. Exit 1.
    pub drift: Vec<String>,

    // What reset needs in order to repair it.
    pub refs_dirty: bool,
    pub extra_branches: Vec<String>,
    pub extra_tags: Vec<String>,
    pub requests: Vec<Request>,
}

/// The whole fleet compared against the lock.
pub(crate) struct Report(pub Vec<State>);

impl Report {
    pub fn guards(&self) -> Vec<String> {
        self.0
            .iter()
            .flat_map(|s| {
                s.guards
                    .iter()
                    .map(move |g| format!("{}: {g}", s.want.name))
            })
            .collect()
    }

    pub fn drifted(self) -> Vec<State> {
        self.0.into_iter().filter(|s| !s.drift.is_empty()).collect()
    }

    /// Turns the report into the error a command should return.
    pub fn into_error(self, e: &Env) -> Result<(), CommandError> {
        let g = self.guards();
        if !g.is_empty() {
            return Err(CommandError::Guard(format!(
                "guard failure:\n  {}",
                g.join("\n  ")
            )));
        }
        let d = self.drifted();
        if !d.is_empty() {
            let lines: Vec<String> = d
                .iter()
                .map(|s| format!("{}: {}", s.want.name, s.drift.join("; ")))
                .collect();
            return Err(CommandError::Drift(format!(
                "drift (run `forgelab reset --sandbox {}`):\n  {}",
                e.sandbox.name,
                lines.join("\n  ")
            )));
        }
        Ok(())
    }
}

impl Env {
    /// Asserts the sandbox matches the lock. Read-only.
    pub async fn verify(&self) -> Result<(), CommandError> {
        let lock = self.load_lock()?;
        let rep = self.compare(&lock).await?;
        rep.into_error(self)?;
        self.printf(format!(
            "ok: {} repositories match {}\n",
            lock.repos.len(),
            fleet::LOCK_FILE
        ));
        Ok(())
    }

    /// Looks every declared repository up by name -- in batches where the forge allows, one
    /// at a time where it does not -- then reads each one's refs with git ls-remote. No writes.
    pub(crate) async fn compare(&self, lock: &Lock) -> Result<Report, CommandError> {
        let names: Vec<String> = lock.repos.iter().map(|r| r.name.clone()).collect();
        let found = self.lookup_confirmed(&names).await?;
        self.progress.phase("check", names.len());
        let states: Vec<(State, Option<forge::Repo>)> = lock
            .repos
            .iter()
            .zip(found)
            .map(|(want, live)| {
                (
                    State {
                        want: want.clone(),
                        ..State::default()
                    },
                    live,
                )
            })
            .collect();
        let results = for_each_collect(
            states,
            self.concurrency,
            &self.cancel,
            |(mut s, live)| async move {
                let r = self.compare_one(&mut s, live).await;
                self.progress.add(1);
                ((s, None), r)
            },
        )
        .await;
        Ok(Report(
            aggregate(results, |(s, _)| &s.want.name)?
                .into_iter()
                .map(|(s, _)| s)
                .collect(),
        ))
    }

    async fn compare_one(
        &self,
        s: &mut State,
        live: Option<forge::Repo>,
    ) -> Result<(), CommandError> {
        let want = s.want.clone();
        let Some(mut live) = live else {
            s.guards.push(format!(
                "missing from {}: run `forgelab apply --sandbox {}`",
                self.sandbox.org, self.sandbox.name
            ));
            return Ok(());
        };
        s.live = live.clone();
        let caps = self.forge.caps();
        if !self.is_ours(&live) {
            s.guards.push(self.not_ours());
            return Ok(());
        }

        if caps.visibility && live.visibility != want.visibility {
            s.drift.push(format!(
                "visibility is {}, want {}",
                live.visibility, want.visibility
            ));
        }
        if live.archived != want.archived {
            s.drift.push(format!(
                "archived is {}, want {}",
                live.archived, want.archived
            ));
        }
        let got = sorted(&live.topics);
        if caps.topics && got != want.topics {
            s.drift.push(format!(
                "topics are {}, want {}",
                go_slice(&got),
                go_slice(&want.topics)
            ));
        }
        // On a forge where archived means unreadable there is nothing further to look at. If it
        // is supposed to be archived, nothing a test could have changed either. If it is not,
        // its refs cannot be checked, so reset rewinds them once it has made it readable again.
        if caps.archived_unreadable && live.archived {
            s.refs_dirty = !want.archived && !want.empty;
            return Ok(());
        }

        if want.empty {
            // A forge will not delete a repository's last branch, so there is no way back to
            // "no commits" short of deleting the repository -- which reset may not do.
            if !live.empty {
                s.guards.push(
                    "is no longer empty: delete it on the forge, then run `forgelab apply`"
                        .to_string(),
                );
            }
            return Ok(());
        }
        if live.empty {
            s.guards.push(format!(
                "was never seeded: run `forgelab apply --sandbox {}`",
                self.sandbox.name
            ));
            return Ok(());
        }

        if live.default_branch != want.default_branch {
            s.drift.push(format!(
                "default branch is {}, want {}",
                live.default_branch, want.default_branch
            ));
        }

        // Refs come from git, not from the forge's listings, which can lag behind a write.
        let remote = self.git_remote(&want.name)?;
        let (branches, tags) = seed::ls_remote(&remote).await?;

        if tags.get(seed::BASELINE_TAG) != Some(&want.baseline) {
            s.guards.push(format!(
                "tag {} is missing or is not {}: run `forgelab apply --sandbox {}`",
                seed::BASELINE_TAG,
                short(&want.baseline),
                self.sandbox.name
            ));
            return Ok(());
        }
        for (t, sha) in &tags {
            if t == seed::BASELINE_TAG {
                continue;
            }
            if !want.tags.contains(t) {
                s.extra_tags.push(t.clone());
                s.drift.push(format!("extra tag {t}"));
            } else if sha != &want.baseline {
                s.refs_dirty = true;
                s.drift.push(format!("tag {t} moved"));
            }
        }
        for t in &want.tags {
            if !tags.contains_key(t) {
                s.refs_dirty = true;
                s.drift.push(format!("tag {t} is missing"));
            }
        }

        for b in branches.keys() {
            if b != &want.default_branch {
                s.extra_branches.push(b.clone());
                s.drift.push(format!("extra branch {b}"));
            }
        }
        match branches.get(&want.default_branch) {
            None => {
                s.refs_dirty = true;
                s.drift
                    .push(format!("branch {} is missing", want.default_branch));
            }
            Some(head) if head != &want.baseline => {
                s.refs_dirty = true;
                s.drift.push(format!(
                    "{} is at {}, want {}",
                    want.default_branch,
                    short(head),
                    short(&want.baseline)
                ));
            }
            Some(_) => {}
        }

        s.requests = match live.open_requests.take() {
            Some(r) => r,
            None => self.forge.open_requests(&want.name).await?,
        };
        // Open, never "how many exist": request numbers are monotonic and closed requests are
        // permanent, so a count can never be reset.
        if !s.requests.is_empty() {
            s.drift
                .push(format!("{} open request(s)", s.requests.len()));
        }
        Ok(())
    }
}
