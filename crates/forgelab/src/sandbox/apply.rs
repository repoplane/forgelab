//! plan and apply: resolve the fleet into a lock, diff it against the forge, converge.

use std::collections::HashMap;

use crate::fleet::{self, Lock};
use crate::forge::{self, ForgeError, Settings};
use crate::seed::{self, Built};
use crate::util::go_slice;

use super::pool::for_each_collect;
use super::report::{CommandError, aggregate};
use super::{Env, with_marker, without_marker};

/// What apply would do to one declared repository.
pub(crate) struct Change {
    pub repo: fleet::Repo,
    /// None for an empty repository.
    pub built: Option<Built>,
    pub live: Option<forge::Repo>,

    pub create: bool,
    /// Content, a declared tag or the default branch differs from the lock.
    pub push: bool,
    /// The repository is unreadable while archived and is to be un-archived: whether it needs
    /// a push can only be decided once it is readable again.
    pub recheck: bool,
    /// Refs an earlier apply created and the fleet no longer declares: `refs/tags/v1`,
    /// `refs/heads/master`. Removed after the settings that may depend on them are changed.
    pub stale_refs: Vec<String>,
    /// Human-readable settings differences.
    pub notes: Vec<String>,
}

impl Change {
    pub(crate) fn needed(&self) -> bool {
        self.create || self.push || self.recheck || !self.notes.is_empty()
    }
}

impl Env {
    /// Prints what apply would do. No writes, to the forge or to disk.
    pub async fn plan(&self) -> Result<(), CommandError> {
        let (_, changes) = self.plan_changes().await?;
        self.print_plan("PLAN", &changes);
        Ok(())
    }

    /// Converges the sandbox on the fleet and writes the lock. It creates and updates; it
    /// never deletes a repository, and it never adopts a repository it did not create.
    ///
    /// It is idempotent, so a run interrupted for any reason resumes by being run again.
    pub async fn apply(&self) -> Result<(), CommandError> {
        let (lock, changes) = self.plan_changes().await?;
        let (todo, _rest): (Vec<Change>, Vec<Change>) =
            changes.into_iter().partition(Change::needed);
        let todo_names: Vec<String> = todo.iter().map(|c| c.repo.name.clone()).collect();
        // The plan is printed from what is needed plus what is not; both halves are shown.
        let mut all: Vec<&Change> = todo.iter().chain(_rest.iter()).collect();
        all.sort_by(|a, b| a.repo.name.cmp(&b.repo.name));
        self.print_plan_refs("APPLY", &all);
        drop(all);
        // No prompt: apply never deletes, touches only declared repositories that carry the
        // marker, and has just printed exactly what it is about to do.
        if !todo.is_empty() {
            self.forge
                .ensure_org()
                .await
                .map_err(|e| CommandError::Other(format!("org {}: {e}", self.sandbox.org)))?;
            let results =
                for_each_collect(todo, self.concurrency, &self.cancel, |mut c| async move {
                    let r = self.apply_one(&mut c).await;
                    if r.is_ok() {
                        self.debugf(format!("  applied {}\n", c.repo.name));
                    }
                    (c, r)
                })
                .await;
            aggregate(results, |c| &c.repo.name)?;
            let _ = todo_names;
        }

        lock.write_file(&self.lock_path())?;
        self.wait_ready(&lock.repos).await?;
        let rep = self.compare(&lock).await?;
        rep.into_error(self)?;
        self.printf(format!(
            "ok: {} repositories match {}\n",
            lock.repos.len(),
            fleet::LOCK_FILE
        ));
        Ok(())
    }

    /// Resolves the fleet into a lock -- building every commit locally, which needs git and
    /// nothing else -- and diffs it against the forge.
    async fn plan_changes(&self) -> Result<(Lock, Vec<Change>), CommandError> {
        let spec = fleet::load_spec(self.root())?;
        let digest = fleet::digest(self.root())?;
        let previous = self.previous_lock();

        let mut changes: Vec<Change> = Vec::with_capacity(spec.repos.len());
        for r in &spec.repos {
            // The marker belongs to the sandbox. A fleet that declared it too would make the
            // topic comparison unable to tell the two apart.
            if r.topics.iter().any(|t| t == &self.sandbox.marker_topic) {
                return Err(CommandError::Other(format!(
                    "{}: topic {:?} is the sandbox marker and may not be declared",
                    r.name, self.sandbox.marker_topic
                )));
            }
            changes.push(Change {
                repo: r.clone(),
                built: None,
                live: None,
                create: false,
                push: false,
                recheck: false,
                stale_refs: vec![],
                notes: vec![],
            });
        }

        let builders = self
            .concurrency
            .min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            )
            .max(1);
        let git = &spec.git;
        let results = for_each_collect(changes, builders, &self.cancel, |mut c| async move {
            if c.repo.empty {
                return (c, Ok(()));
            }
            let r = seed::build(self.root(), &c.repo, git).await;
            match r {
                Ok(b) => {
                    c.built = Some(b);
                    (c, Ok(()))
                }
                Err(e) => (c, Err(CommandError::Other(e.to_string()))),
            }
        })
        .await;
        let changes = aggregate(results, |c| &c.repo.name)?;

        let mut baselines: HashMap<String, String> = HashMap::new();
        for c in &changes {
            if let Some(b) = &c.built {
                baselines.insert(c.repo.name.clone(), b.sha.clone());
            }
        }
        let lock = Lock::new(&spec, &digest, &baselines);

        let previous = &previous;
        let results = for_each_collect(
            changes,
            self.concurrency,
            &self.cancel,
            |mut c| async move {
                let r = self.diff_one(&mut c, previous.as_ref()).await;
                (c, r)
            },
        )
        .await;
        let changes = aggregate(results, |c| &c.repo.name)?;
        Ok((lock, changes))
    }

    async fn diff_one(&self, c: &mut Change, previous: Option<&Lock>) -> Result<(), CommandError> {
        let want = c.repo.clone();
        let Some(live) = self.forge.get(&want.name).await? else {
            c.create = true;
            return Ok(());
        };
        let caps = self.forge.caps();
        // A repository with this name that forgelab did not create is somebody else's.
        if caps.topics && !live.topics.iter().any(|t| t == &self.sandbox.marker_topic) {
            return Err(CommandError::Guard(format!(
                "{}/{} already exists without the {:?} topic: it is not forgelab's, and apply never adopts. Rename the fixture or remove that repository",
                self.sandbox.org, want.name, self.sandbox.marker_topic
            )));
        }

        if caps.visibility && live.visibility != want.visibility {
            c.notes.push(format!(
                "visibility: {} → {}",
                live.visibility, want.visibility
            ));
        }
        if live.archived != want.archived {
            c.notes
                .push(format!("archived: {} → {}", live.archived, want.archived));
        }
        let got = without_marker(&live.topics, &self.sandbox.marker_topic);
        if caps.topics && got != want.topics {
            c.notes.push(format!(
                "topics: {} → {}",
                go_slice(&got),
                go_slice(&want.topics)
            ));
        }
        // Unreadable while archived: its refs cannot be compared. If it is to stay archived it
        // is taken as seeded -- to re-seed one after editing its fixture, destroy it first. If
        // it is to be un-archived, the question is asked again once it can be read.
        if caps.archived_unreadable && live.archived {
            c.recheck = !want.archived && !want.empty;
            c.live = Some(live);
            return Ok(());
        }
        if want.empty {
            if !live.empty {
                return Err(CommandError::Guard(format!(
                    "{} is no longer empty: delete it on the forge, then run apply again",
                    want.name
                )));
            }
            c.live = Some(live);
            return Ok(());
        }
        if live.default_branch != want.default_branch {
            c.notes.push(format!(
                "default branch: {} → {}",
                live.default_branch, want.default_branch
            ));
        }
        if live.empty {
            c.push = true; // created by an interrupted apply, never seeded
            c.live = Some(live);
            return Ok(());
        }
        let remote = self.git_remote(&want.name)?;
        let (branches, tags) = seed::ls_remote(&remote).await?;
        let sha = c.built.as_ref().map(|b| b.sha.as_str()).unwrap_or("");
        c.push = tags.get(seed::BASELINE_TAG).map(String::as_str) != Some(sha)
            // Every declared tag must sit at the seed commit, and the default branch must exist
            // before it can be made the default.
            || want.tags.iter().any(|t| tags.get(t).map(String::as_str) != Some(sha))
            || !branches.contains_key(&want.default_branch);
        if !c.push
            && (want
                .tags
                .iter()
                .any(|t| tags.get(t).map(String::as_str) != Some(sha))
                || !branches.contains_key(&want.default_branch))
        {
            c.push = true;
        }
        // What an earlier apply created and this fleet no longer declares: a tag that was in
        // the previous lock, the previous default branch. Only those -- a ref a test created is
        // drift, and reset's business.
        if let Some(prev) = previous.and_then(|p| p.repo(&want.name)) {
            for t in &prev.tags {
                if !want.tags.contains(t) && tags.contains_key(t) {
                    c.stale_refs.push(format!("refs/tags/{t}"));
                }
            }
            if prev.default_branch != want.default_branch
                && branches.contains_key(&prev.default_branch)
            {
                c.stale_refs
                    .push(format!("refs/heads/{}", prev.default_branch));
            }
            if !c.stale_refs.is_empty() {
                c.notes.push(format!("remove: {}", c.stale_refs.join(", ")));
            }
        }
        c.live = Some(live);
        Ok(())
    }

    async fn apply_one(&self, c: &mut Change) -> Result<(), CommandError> {
        let want = c.repo.clone();
        let name = want.name.as_str();
        let marker = self.sandbox.marker_topic.as_str();

        if c.create {
            // The marker goes on with the creation, before any content: an interrupted apply
            // leaves repositories behind, and the re-run has to recognise them as its own.
            self.create_verified(
                name,
                &want.visibility,
                &want.default_branch,
                &with_marker(&want.topics, marker),
            )
            .await
            .map_err(|e| CommandError::Other(format!("create: {e}")))?;
        }

        // An archived repository rejects every write, so it is lifted for the duration and
        // restored last.
        let mut live_archived = c.live.as_ref().is_some_and(|l| l.archived);
        if live_archived {
            self.forge
                .update_settings(
                    name,
                    Settings {
                        archived: Some(false),
                        ..Settings::default()
                    },
                )
                .await
                .map_err(|e| CommandError::Other(format!("unarchive: {e}")))?;
            live_archived = false;
        }

        if c.recheck
            && let Some(b) = &c.built
        {
            // Readable now: does it hold the seed?
            let remote = self.git_remote(name)?;
            let (_, tags) = seed::ls_remote(&remote).await?;
            c.push = tags.get(seed::BASELINE_TAG) != Some(&b.sha);
        }

        if (c.create || c.push)
            && let Some(b) = &c.built
        {
            // Every seed is a force-push, the first one included, so the lift is unconditional:
            // the branch this is about to write can already be protected even on a repository
            // created moments ago, because a forge may protect a default branch the instant it
            // names one.
            self.forge
                .allow_force_push(name, &want.default_branch)
                .await
                .map_err(|e| CommandError::Other(format!("allow force-push: {e}")))?;
            let remote = self.git_remote(name)?;
            b.push(&remote).await?;
        }

        // Archived goes last, on its own: once set, nothing else can be.
        let mut settings = Settings {
            visibility: Some(want.visibility.clone()),
            ..Settings::default()
        };
        if !want.empty {
            // A repository created empty adopts the instance default branch, so a fixture that
            // wants `master` needs it set after the push has created the ref.
            settings.default_branch = Some(want.default_branch.clone());
        }
        self.forge
            .update_settings(name, settings)
            .await
            .map_err(|e| CommandError::Other(format!("settings: {e}")))?;
        if !c.create {
            self.forge
                .set_topics(name, &with_marker(&want.topics, marker))
                .await
                .map_err(|e| CommandError::Other(format!("set topics: {e}")))?;
        }
        if !c.stale_refs.is_empty() {
            let remote = self.git_remote(name)?;
            seed::delete_remote_refs(&remote, &c.stale_refs)
                .await
                .map_err(|e| CommandError::Other(format!("remove stale refs: {e}")))?;
        }
        if want.archived != live_archived {
            self.forge
                .update_settings(
                    name,
                    Settings {
                        archived: Some(want.archived),
                        ..Settings::default()
                    },
                )
                .await
                .map_err(|e| CommandError::Other(format!("archive: {e}")))?;
        }
        Ok(())
    }

    /// Creates a repository, and when the forge answered a create with a transient failure --
    /// a 5xx, a dropped connection -- asks it whether the repository is there before trying
    /// again. A create is not idempotent, so it is never repeated blindly; but a 503 from a
    /// forge under load, which is what a fleet of a hundred provokes, must not strand the
    /// apply either. One that did land is finished the way `Forge::create` would have: with
    /// its topics set, so the marker is on it.
    async fn create_verified(
        &self,
        name: &str,
        visibility: &str,
        default_branch: &str,
        topics: &[String],
    ) -> Result<(), ForgeError> {
        let mut wait = std::time::Duration::from_secs(1);
        for attempt in 1..=5 {
            match self
                .forge
                .create(name, visibility, default_branch, topics)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if e.class() == forge::Class::Transient && attempt < 5 => {
                    tracing::debug!(
                        name,
                        attempt,
                        "create answered a transient failure, checking whether it landed: {e}"
                    );
                    match self.forge.get(name).await? {
                        Some(_) => return self.forge.set_topics(name, topics).await,
                        None => {
                            tokio::select! {
                                _ = tokio::time::sleep(wait) => {}
                                _ = self.cancel.cancelled() => return Err(ForgeError::Cancelled),
                            }
                            wait *= 2;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    fn print_plan(&self, verb: &str, changes: &[Change]) {
        let refs: Vec<&Change> = changes.iter().collect();
        self.print_plan_refs(verb, &refs);
    }

    fn print_plan_refs(&self, verb: &str, changes: &[&Change]) {
        self.header(verb, changes.len());
        let caps = self.forge.caps();
        if !caps.topics || !caps.visibility {
            self.printf(format!(
                "  note: {} has no repository topics or per-repository visibility. Those fleet\n        settings are ignored here, and with no topic to carry it so is the marker guard:\n        a repository with a declared name is treated as forgelab's.\n\n",
                self.sandbox.forge
            ));
        }
        let mut n = 0;
        for c in changes {
            if c.create {
                self.printf(format!("  + {:<28} create\n", c.repo.name));
            } else if c.needed() {
                let mut notes = c.notes.clone();
                if c.push {
                    notes.insert(0, "content: re-seed".to_string());
                } else if c.recheck {
                    notes.insert(0, "content: check once readable".to_string());
                }
                self.printf(format!("  ~ {:<28} {}\n", c.repo.name, notes.join(", ")));
            } else {
                continue;
            }
            n += 1;
        }
        if n == 0 {
            self.printf("  nothing to do\n");
        }
        self.printf("\n");
    }
}
