//! reset: put every drifted repository back to its baseline.

use crate::forge::Settings;
use crate::seed;

use super::Env;
use super::pool::for_each_collect;
use super::report::{CommandError, aggregate};
use super::verify::State;

impl Env {
    /// Puts every drifted repository back to its baseline.
    ///
    /// It cannot create or delete a repository -- not "does not", cannot: it has no code path
    /// that does either. Reset runs constantly and unattended, and a missing repository means
    /// the fleet drifted or the tool is pointed at the wrong place. Both want a human, so that
    /// is a guard failure telling them to run apply.
    ///
    /// It writes only to repositories that differ from the lock. Every step is declarative --
    /// "make it equal X", never "apply this delta" -- so a reset that dies halfway is safely
    /// re-runnable.
    pub async fn reset(&self) -> Result<(), CommandError> {
        let lock = self.load_lock()?;
        let rep = self.compare(&lock).await?;
        let g = rep.guards();
        if !g.is_empty() {
            return Err(CommandError::Guard(format!(
                "guard failure, nothing was reset:\n  {}",
                g.join("\n  ")
            )));
        }

        let touched = rep.drifted();
        if touched.is_empty() {
            self.printf("ok: nothing to reset\n");
            return Ok(());
        }
        self.progress.phase("reset", touched.len());
        let results = for_each_collect(touched, self.concurrency, &self.cancel, |s| async move {
            let r = self.reset_one(&s).await;
            self.progress.add(1);
            if r.is_ok() {
                self.printf(format!("reset {} ({})\n", s.want.name, s.drift.join("; ")));
            }
            (s, r)
        })
        .await;
        let touched = aggregate(results, |s| &s.want.name)?;

        let wants: Vec<_> = touched.iter().map(|s| s.want.clone()).collect();
        self.wait_ready(&wants).await?;
        let rep = self.compare(&lock).await?;
        rep.into_error(self)?;
        self.printf(format!(
            "ok: {} of {} repositories reset\n",
            touched.len(),
            lock.repos.len()
        ));
        Ok(())
    }

    /// The order is load-bearing: an archived repository refuses every write, the default
    /// branch must exist before it can be the default, and a branch cannot be deleted while it
    /// is the default or while an open request still points at it.
    async fn reset_one(&self, s: &State) -> Result<(), CommandError> {
        let want = &s.want;
        let name = want.name.as_str();

        if s.live.archived {
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

        if s.refs_dirty {
            // Not assumed from apply: the branch may have been protected since, by the forge or
            // by the test that just ran. A refused push is what says so.
            let remote = self.git_remote(name)?;
            self.force_push(name, &want.default_branch, || async {
                Ok(seed::reset_to_baseline(&remote, &want.default_branch, &want.tags).await?)
            })
            .await?;
        }

        let mut settings = Settings {
            visibility: Some(want.visibility.clone()),
            ..Settings::default()
        };
        if !want.empty {
            settings.default_branch = Some(want.default_branch.clone());
        }
        self.forge
            .update_settings(name, settings)
            .await
            .map_err(|e| CommandError::Other(format!("settings: {e}")))?;

        for r in &s.requests {
            self.forge
                .close_request(name, r.number)
                .await
                .map_err(|e| CommandError::Other(format!("close request #{}: {e}", r.number)))?;
        }
        for b in &s.extra_branches {
            self.forge
                .delete_branch(name, b)
                .await
                .map_err(|e| CommandError::Other(format!("delete branch {b}: {e}")))?;
        }
        for t in &s.extra_tags {
            self.forge
                .delete_tag(name, t)
                .await
                .map_err(|e| CommandError::Other(format!("delete tag {t}: {e}")))?;
        }

        self.forge
            .set_topics(name, &want.topics)
            .await
            .map_err(|e| CommandError::Other(format!("set topics: {e}")))?;
        if want.archived {
            self.forge
                .update_settings(
                    name,
                    Settings {
                        archived: Some(true),
                        ..Settings::default()
                    },
                )
                .await
                .map_err(|e| CommandError::Other(format!("archive: {e}")))?;
        }
        Ok(())
    }
}
