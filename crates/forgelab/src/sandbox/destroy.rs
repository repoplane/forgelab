//! destroy: delete the declared repositories that carry the marker, and nothing else.

use crate::fleet::{self, Spec};
use crate::forge::{NamespaceDepth, Removal};

use super::Env;
use super::pool::for_each_collect;
use super::report::{CommandError, FailureReport, RepoFailure};

struct Target {
    name: String,
    delete: bool,
    skip: String,
}

impl Env {
    /// Deletes the declared repositories that carry the marker, and nothing else: not
    /// undeclared repositories, not a same-named repository forgelab did not create, and not
    /// the organisation. The namespaces they sat in go too, with whatever else is in them by
    /// then, but only those forgelab made: the marker means the same on a namespace as on a
    /// repository.
    ///
    /// One repository that cannot be deleted does not stop the others, nor the namespaces:
    /// everything that can go goes, and every failure is reported at the end.
    pub async fn destroy(&self) -> Result<(), CommandError> {
        let spec = fleet::load_spec(self.root())?;
        let caps = self.forge.caps();

        let names: Vec<String> = spec.repos.iter().map(|r| r.name.clone()).collect();
        let found = self.lookup_confirmed(&names).await?;
        let targets: Vec<Target> = names
            .into_iter()
            .zip(found)
            .map(|(name, live)| {
                let mut t = Target {
                    name,
                    delete: false,
                    skip: String::new(),
                };
                if let Some(live) = live {
                    if !self.is_ours(&live) {
                        t.skip = format!(
                            "has no {:?} marker in its description, so it is not forgelab's",
                            self.sandbox.marker
                        );
                    } else {
                        t.delete = true;
                    }
                }
                t
            })
            .collect();

        self.header("DESTROY", targets.len());
        let mut doomed: Vec<Target> = Vec::new();
        for t in targets {
            if t.delete {
                self.printf(format!("  - {}\n", t.name));
                doomed.push(t);
            } else if !t.skip.is_empty() {
                self.printf(format!("  ! {:<28} left alone: {}\n", t.name, t.skip));
            }
        }
        let namespaces =
            doomed_namespaces(&spec, caps.namespace_depth, &self.sandbox.default_project);
        let label = |ns: &str| {
            if ns.is_empty() {
                format!("{}/", self.sandbox.default_project)
            } else {
                format!("{ns}/")
            }
        };
        // No question without a declared repository to lose. What is left to remove then is
        // namespaces of forgelab's own making, which is how an interrupted destroy is finished.
        if !doomed.is_empty() {
            for ns in &namespaces {
                self.printf(format!(
                    "  - {:<28} namespace, with all it holds: only if forgelab made it\n",
                    label(ns)
                ));
            }
            self.printf("\n  Deletion cannot be undone: pull requests, issue numbers and history go with them.\n");
            if namespaces.is_empty() {
                self.printf(format!(
                    "  Only these {} are deleted; anything else in {} is left alone.\n",
                    doomed.len(),
                    self.sandbox.org
                ));
            } else {
                self.printf(format!("  Only these {} and those namespaces are deleted; anything else in {} is left alone.\n", doomed.len(), self.sandbox.org));
            }
            self.confirm()?;
        }

        let total = doomed.len();
        self.progress.phase("delete", doomed.len());
        let results = for_each_collect(doomed, self.concurrency, &self.cancel, |t| async move {
            let r = self
                .forge
                .delete(&t.name)
                .await
                .map_err(|e| CommandError::Other(format!("delete {}: {e}", t.name)));
            self.progress.add(1);
            (t, r)
        })
        .await;
        let mut failures: Vec<RepoFailure> = Vec::new();
        let mut deleted = 0usize;
        for (t, r) in results {
            match r {
                Ok(()) => deleted += 1,
                Err(e) => failures.push(RepoFailure {
                    name: t.name,
                    message: e.to_string(),
                }),
            }
        }
        // Outermost first: one that forgelab made takes everything inside with it, and one it
        // did not make is kept while forgelab's own inside it still go.
        let mut removed = 0;
        for ns in &namespaces {
            match self.forge.delete_namespace(ns).await {
                Ok(Removal::Removed) => {
                    removed += 1;
                    self.printf(format!("  - {:<28} namespace removed\n", label(ns)));
                }
                Ok(Removal::Kept(why)) => {
                    self.printf(format!("  ! {:<28} kept: {why}\n", label(ns)));
                    if why.contains("still being deleted") {
                        failures.push(RepoFailure {
                            name: label(ns),
                            message: format!("namespace {why}"),
                        });
                    }
                }
                Ok(Removal::Absent) => {}
                Err(e) => failures.push(RepoFailure {
                    name: label(ns),
                    message: format!("delete namespace: {e}"),
                }),
            }
        }
        if !failures.is_empty() {
            failures.sort_by(|a, b| a.name.cmp(&b.name));
            return Err(CommandError::Failed(FailureReport {
                total: total + namespaces.len(),
                failures,
            }));
        }
        match (deleted, removed) {
            (0, 0) => self.printf("  nothing to delete\n\n"),
            (_, 0) => self.printf(format!("ok: {deleted} repositories deleted\n")),
            (_, 1) => self.printf(format!(
                "ok: {deleted} repositories deleted, 1 namespace removed\n"
            )),
            _ => self.printf(format!(
                "ok: {deleted} repositories deleted, {removed} namespaces removed\n"
            )),
        }
        Ok(())
    }
}

/// What destroy removes besides the repositories themselves, outermost first: the namespaces
/// this forge holds as things of their own rather than as a prefix of a name, and then the
/// home it gives the repositories that have no namespace.
///
/// That last one is included only if this fleet actually put a repository there. A fleet whose
/// repositories are all namespaced never used that home, and on Azure DevOps the home is a
/// project forgelab made for some other fleet -- marked as its own, and so removed "with all it
/// holds" on the word of a fleet that never touched it.
pub(crate) fn doomed_namespaces(
    spec: &Spec,
    depth: NamespaceDepth,
    default_project: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if depth != NamespaceDepth::None {
        for ns in spec.namespaces() {
            if depth.holds(&ns) {
                out.push(ns);
            }
        }
    }
    if !default_project.is_empty() && spec.repos.iter().any(|r| !r.name.contains('/')) {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{Author, GitIdentity, Repo};

    fn spec(names: &[&str]) -> Spec {
        Spec {
            version: 1,
            git: GitIdentity {
                author: Author::default(),
                timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
            },
            repos: names
                .iter()
                .map(|n| Repo {
                    name: n.to_string(),
                    ..Repo::default()
                })
                .collect(),
        }
    }

    fn show(ns: &[String]) -> String {
        ns.iter()
            .map(|n| {
                if n.is_empty() {
                    "<default project>"
                } else {
                    n.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A fleet that lives entirely under namespaces never put anything in the home a forge
    /// gives to repositories without one, so destroy must not remove it.
    #[test]
    fn spares_an_unused_default_project() {
        let got = doomed_namespaces(
            &spec(&[
                "acme/payments/api",
                "acme/platform/observability/collector",
                "acme/handbook",
            ]),
            NamespaceDepth::Depth(1),
            "fleet",
        );
        assert_eq!(show(&got), "acme");
    }

    #[test]
    fn removes_a_used_default_project() {
        let got = doomed_namespaces(
            &spec(&["dotfiles", "services/api"]),
            NamespaceDepth::Depth(1),
            "fleet",
        );
        assert_eq!(show(&got), "services, <default project>");
    }

    #[test]
    fn without_a_default_project() {
        for s in [spec(&["dotfiles"]), spec(&["acme/payments/api"])] {
            let got = doomed_namespaces(&s, NamespaceDepth::Any, "");
            assert!(!got.iter().any(String::is_empty), "{}", show(&got));
        }
    }

    #[test]
    fn follows_depth() {
        let s = spec(&["acme/payments/gateway/api", "acme/handbook"]);
        assert_eq!(
            show(&doomed_namespaces(&s, NamespaceDepth::Depth(1), "")),
            "acme"
        );
        assert_eq!(
            show(&doomed_namespaces(&s, NamespaceDepth::Any, "")),
            "acme, acme/payments, acme/payments/gateway"
        );
        assert_eq!(show(&doomed_namespaces(&s, NamespaceDepth::None, "")), "");
    }
}
