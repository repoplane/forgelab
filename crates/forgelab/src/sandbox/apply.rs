//! plan and apply: resolve the fleet into a lock, diff it against the forge, converge.

use std::collections::HashMap;

use crate::fleet::{self, Lock};
use crate::forge::{self, Settings};
use crate::seed::{self, Built};
use crate::util::go_slice;

use super::pool::for_each_collect;
use super::report::{CommandError, aggregate};
use super::{Env, sorted};

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
            self.progress.phase("apply", todo.len());
            let results =
                for_each_collect(todo, self.concurrency, &self.cancel, |mut c| async move {
                    let r = self.apply_one(&mut c).await;
                    self.progress.add(1);
                    (c, r)
                })
                .await;
            let todo = aggregate(results, |c| &c.repo.name)?;

            // What the repositories just created look like now that they hold their seed: read
            // back, all at once, rather than written blindly. Their visibility came with the
            // create and their default branch with the first push, and on GitHub that saves one
            // paced write in three.
            let created: Vec<String> = todo
                .iter()
                .filter(|c| c.create)
                .map(|c| c.repo.name.clone())
                .collect();
            let mut fresh: HashMap<String, forge::Repo> = HashMap::new();
            for (name, r) in created.iter().zip(self.lookup(&created).await?) {
                if let Some(r) = r {
                    fresh.insert(name.clone(), r);
                }
            }
            let fresh = &fresh;
            self.progress.phase("settle", todo.len());
            let results = for_each_collect(todo, self.concurrency, &self.cancel, |c| async move {
                let current = if c.create {
                    fresh.get(&c.repo.name).cloned()
                } else {
                    c.live.clone()
                };
                let r = self.settle_one(&c, current).await;
                self.progress.add(1);
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
        self.progress.phase("build", changes.len());
        let results = for_each_collect(changes, builders, &self.cancel, |mut c| async move {
            if c.repo.empty {
                return (c, Ok(()));
            }
            let r = seed::build(self.root(), &c.repo, git).await;
            self.progress.add(1);
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

        let names: Vec<String> = changes.iter().map(|c| c.repo.name.clone()).collect();
        let found = self.lookup(&names).await?;
        let previous = &previous;
        let results = for_each_collect(
            changes.into_iter().zip(found).collect(),
            self.concurrency,
            &self.cancel,
            |(mut c, live)| async move {
                let r = self.diff_one(&mut c, live, previous.as_ref()).await;
                ((c, None), r)
            },
        )
        .await;
        let changes = aggregate(results, |(c, _)| &c.repo.name)?
            .into_iter()
            .map(|(c, _): (Change, Option<forge::Repo>)| c)
            .collect();
        Ok((lock, changes))
    }

    async fn diff_one(
        &self,
        c: &mut Change,
        live: Option<forge::Repo>,
        previous: Option<&Lock>,
    ) -> Result<(), CommandError> {
        let want = c.repo.clone();
        let Some(live) = live else {
            c.create = true;
            return Ok(());
        };
        let caps = self.forge.caps();
        // A repository with this name that forgelab did not create is somebody else's.
        if !self.is_ours(&live) {
            return Err(CommandError::Guard(self.never_adopts(&want.name)));
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
        let got = sorted(&live.topics);
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
        if live.empty {
            c.push = true; // created by an interrupted apply, never seeded
            c.live = Some(live);
            return Ok(());
        }
        // Only once there is a commit: a forge may report the default branch of an empty
        // repository as the instance default, or as nothing at all.
        if live.default_branch != want.default_branch {
            c.notes.push(format!(
                "default branch: {} → {}",
                live.default_branch, want.default_branch
            ));
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

        if c.create {
            // The marker goes on with the creation, in the same request: an interrupted apply
            // leaves repositories behind, and the re-run has to recognise them as its own.
            self.create_verified(
                name,
                &want.visibility,
                &want.default_branch,
                &want.topics,
                c.built.is_some(),
            )
            .await
            .map_err(|e| match e {
                CommandError::Guard(g) => CommandError::Guard(g),
                other => CommandError::Other(format!("create: {other}")),
            })?;
        }

        // An archived repository rejects every write, so it is lifted for the duration and
        // restored last, in `settle_one`.
        if c.live.as_ref().is_some_and(|l| l.archived) {
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
            let remote = self.git_remote(name)?;
            self.force_push(name, &want.default_branch, || {
                self.push_seed(b, &remote, name, c.create)
            })
            .await?;
        }
        Ok(())
    }

    /// The second half of applying one repository, once every repository has its content:
    /// the settings, then what depends on them, then archiving. Only what differs is written:
    /// writes are what a forge rate-limits -- GitHub paces them a second apart -- and reads are
    /// cheap. `current` is the repository as last read.
    async fn settle_one(
        &self,
        c: &Change,
        current: Option<forge::Repo>,
    ) -> Result<(), CommandError> {
        let want = &c.repo;
        let name = want.name.as_str();
        let caps = self.forge.caps();
        let mut settings = Settings::default();
        if caps.visibility
            && current
                .as_ref()
                .is_none_or(|l| l.visibility != want.visibility)
        {
            settings.visibility = Some(want.visibility.clone());
        }
        // A repository created empty adopts the instance default branch, so a fixture that
        // wants `master` needs it set after the push has created the ref.
        if !want.empty
            && current
                .as_ref()
                .is_none_or(|l| l.default_branch != want.default_branch)
        {
            settings.default_branch = Some(want.default_branch.clone());
        }
        if settings != Settings::default() {
            self.forge
                .update_settings(name, settings)
                .await
                .map_err(|e| CommandError::Other(format!("settings: {e}")))?;
        }
        // Set when they differ -- a create sets them, and so normally leaves nothing to do.
        let topics_differ = current
            .as_ref()
            .is_none_or(|l| sorted(&l.topics) != want.topics);
        if caps.topics && topics_differ {
            self.forge
                .set_topics(name, &want.topics)
                .await
                .map_err(|e| CommandError::Other(format!("set topics: {e}")))?;
        }
        if !c.stale_refs.is_empty() {
            let remote = self.git_remote(name)?;
            seed::delete_remote_refs(&remote, &c.stale_refs)
                .await
                .map_err(|e| CommandError::Other(format!("remove stale refs: {e}")))?;
        }
        // Unarchived by `apply_one` if it was archived, so archived now only if it should be.
        if want.archived {
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

    /// Force-pushes, and lifts whatever protects `branch` only if the forge refused the push.
    /// Every seed and every reset is a force-push, and on a repository nothing protects -- a
    /// sandbox's, normally -- asking first costs two reads per push for nothing. A refused push
    /// is lifted and made again: by the forge itself (GitLab protects a default branch on first
    /// push), by an interrupted apply, or by the test that just ran.
    pub(crate) async fn force_push<F, Fut>(
        &self,
        name: &str,
        branch: &str,
        push: F,
    ) -> Result<(), CommandError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), CommandError>>,
    {
        let refused = match push().await {
            Ok(()) => return Ok(()),
            Err(e) if self.cancel.is_cancelled() => return Err(e),
            Err(e) => e,
        };
        tracing::debug!(
            name,
            "push refused, lifting what protects {branch}: {refused}"
        );
        if let Err(e) = self.forge.allow_force_push(name, branch).await {
            return Err(CommandError::Other(format!(
                "{refused} (and lifting what protects {branch} failed: {e})"
            )));
        }
        push().await
    }

    /// Pushes the seed. A repository created a moment ago can be known to the forge's API and
    /// not yet to its git endpoint: GitLab answered "project not found" to the push of one of
    /// 108 new projects in a real apply, after its API had already answered for it. So for a
    /// repository this apply just created, and only then, a "not found" from git is waited
    /// out, up to 90s. Anywhere else a missing repository is a real answer.
    async fn push_seed(
        &self,
        b: &Built,
        remote: &crate::forge::GitRemote,
        name: &str,
        just_created: bool,
    ) -> Result<(), CommandError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut pause = std::time::Duration::from_secs(1);
        loop {
            match b.push(remote).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    let not_found = msg.contains("could not be found") || msg.contains("not found");
                    if !(just_created && not_found)
                        || tokio::time::Instant::now() + pause >= deadline
                    {
                        return Err(e.into());
                    }
                    tracing::debug!(name, "created, git does not know it yet; pushing again");
                    tokio::select! {
                        _ = tokio::time::sleep(pause) => {}
                        _ = self.cancel.cancelled() => return Err(CommandError::Other("interrupted".into())),
                    }
                    pause = (pause * 2).min(std::time::Duration::from_secs(10));
                }
            }
        }
    }

    /// Waits until a repository just created answers. GitLab has answered a create with
    /// success and then 404 to the next few calls on the project -- the push, the branch
    /// protection -- for three projects of a hundred and eight in a subgroup made a moment
    /// before. Everything after a create assumes the repository is there, so it is waited for
    /// here, once, rather than in every step that follows.
    async fn wait_created(&self, name: &str) -> Result<(), CommandError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut pause = std::time::Duration::from_millis(200);
        loop {
            if self.forge.get(name).await?.is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CommandError::Other(format!(
                    "{name} was created but does not answer after {}",
                    crate::util::go_duration(std::time::Duration::from_secs(90))
                )));
            }
            tracing::debug!(name, "created, not answering yet");
            tokio::select! {
                _ = tokio::time::sleep(pause) => {}
                _ = self.cancel.cancelled() => return Err(CommandError::Other("interrupted".into())),
            }
            pause = (pause * 2).min(std::time::Duration::from_secs(3));
        }
    }

    /// The guard failure for a declared name that somebody else's repository already holds.
    fn never_adopts(&self, name: &str) -> String {
        format!(
            "{}/{name} already exists without the {:?} marker in its description: it is not forgelab's, and apply never adopts. Rename the fixture or remove that repository",
            self.sandbox.org, self.sandbox.marker
        )
    }

    /// Creates a repository, and does not take the forge's first word for a failure that
    /// may not be one:
    ///
    /// - a transient failure -- a 5xx, a dropped connection -- may or may not have landed. A
    ///   create is not idempotent, so it is never repeated blindly: the forge is asked whether
    ///   the repository is there, and only if it is not is the create tried again;
    /// - a conflict means the repository exists after all: the lookup that planned the create
    ///   was answered from a cache that had not caught up. It is then treated exactly as the
    ///   plan would have: forgelab's own if it carries the marker, somebody else's otherwise.
    ///
    /// One that did land carries the marker -- it went on with the create -- and is finished
    /// the way `Forge::create` would have: with its topics set.
    ///
    /// When a push follows, the push is what waits for a repository the forge has not caught
    /// up with yet (`push_seed`); only an empty one is waited for here.
    async fn create_verified(
        &self,
        name: &str,
        visibility: &str,
        default_branch: &str,
        topics: &[String],
        push_follows: bool,
    ) -> Result<(), CommandError> {
        let mut wait = std::time::Duration::from_secs(1);
        for attempt in 1..=5 {
            let err = match self
                .forge
                .create(
                    name,
                    visibility,
                    default_branch,
                    topics,
                    &self.sandbox.marker,
                )
                .await
            {
                Ok(()) if push_follows => return Ok(()),
                Ok(()) => return self.wait_created(name).await,
                Err(e) => e,
            };
            match err.class() {
                forge::Class::Transient if attempt < 5 => {
                    tracing::debug!(
                        name,
                        attempt,
                        "create answered a transient failure, checking whether it landed: {err}"
                    );
                    if let Some(live) = self.forge.get(name).await? {
                        if !self.is_ours(&live) {
                            return Err(CommandError::Guard(self.never_adopts(name)));
                        }
                        return Ok(self.forge.set_topics(name, topics).await?);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.cancel.cancelled() => return Err(CommandError::Other("interrupted".into())),
                    }
                    wait *= 2;
                }
                forge::Class::Conflict => {
                    let Some(live) = self.get_confirmed(name).await? else {
                        return Err(err.into());
                    };
                    if !self.is_ours(&live) {
                        return Err(CommandError::Guard(self.never_adopts(name)));
                    }
                    tracing::debug!(
                        name,
                        "create answered a conflict: the repository is forgelab's after all"
                    );
                    return Ok(self.forge.set_topics(name, topics).await?);
                }
                _ => return Err(err.into()),
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
        if !caps.topics || !caps.visibility || !caps.marker {
            self.printf(format!(
                "  note: {} has no repository topics, descriptions or per-repository visibility.\n        Those fleet settings are ignored here, and with no description to carry it so is\n        the marker guard: a repository with a declared name is treated as forgelab's.\n\n",
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
