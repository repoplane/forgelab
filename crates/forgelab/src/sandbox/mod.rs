//! The commands: plan, apply, verify, reset, destroy.
//!
//! Everything here loops over the declared fleet and looks repositories up by name. Nothing
//! enumerates the organisation, so a repository the fleet does not declare is never
//! compared, reported or touched.

pub mod apply;
pub mod config;
pub mod destroy;
pub mod pool;
pub mod ready;
pub mod report;
pub mod reset;
pub mod verify;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

pub use config::{CONFIG_FILE, Config, DEFAULT_MARKER_TOPIC, Sandbox, load_config, names};
pub use report::{CommandError, FailureReport, RepoFailure};

use crate::fleet::{self, Lock};
use crate::forge::{self, Forge, GitRemote, Transport};

/// Where a command's human-readable output goes.
pub type Output = Arc<Mutex<dyn Write + Send>>;

/// Configures `Env::open`.
pub struct Options {
    /// Directory holding fleet.yaml, repos/ and fleet.lock.json. Default ".".
    pub fleet_dir: String,
    /// Default: `<fleet_dir>/sandboxes.yaml`.
    pub config_path: String,
    pub sandbox: String,
    /// Skip the destroy confirmation.
    pub yes: bool,
    pub verbose: bool,
    /// Overrides the sandbox's and the forge's concurrency.
    pub concurrency: Option<usize>,
    /// Default stdin.
    pub input: Option<Box<dyn BufRead + Send>>,
    /// Default stdout.
    pub out: Option<Output>,
    /// If set, used for every forge request.
    pub transport: Option<Arc<dyn Transport>>,
    pub cancel: Option<CancellationToken>,
    /// If set, used instead of the variable `token_env` names. For tests, which must not
    /// change the process environment under each other.
    pub token: Option<SecretString>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fleet_dir: ".".into(),
            config_path: String::new(),
            sandbox: String::new(),
            yes: false,
            verbose: false,
            concurrency: None,
            input: None,
            out: None,
            transport: None,
            cancel: None,
            token: None,
        }
    }
}

/// A resolved sandbox plus the fleet directory it is driven from.
pub struct Env {
    pub sandbox: Sandbox,
    pub forge: Arc<dyn Forge>,
    pub fleet_dir: PathBuf,
    /// Repositories worked on at once.
    pub concurrency: usize,
    pub cancel: CancellationToken,
    yes: bool,
    verbose: bool,
    input: Mutex<Option<Box<dyn BufRead + Send>>>,
    out: Output,
}

impl Env {
    /// Resolves the sandbox and builds the forge client.
    pub fn open(o: Options) -> Result<Env, CommandError> {
        let (fleet_dir, config_path) = config::resolve_paths(&o.fleet_dir, &o.config_path);
        let cfg = load_config(&config_path)?;
        if !cfg.org_allowlist.is_empty() {
            eprintln!(
                "forgelab: note: org_allowlist is no longer used; remove it from {}",
                config_path.display()
            );
        }
        let sb = cfg.sandbox(&o.sandbox)?;
        let token: SecretString = match o.token {
            Some(t) => t,
            None => {
                let token = std::env::var(&sb.token_env).unwrap_or_default();
                if token.is_empty() {
                    return Err(CommandError::Other(format!(
                        "sandbox {:?}: environment variable {} is empty",
                        sb.name, sb.token_env
                    )));
                }
                token.into()
            }
        };
        let cancel = o.cancel.unwrap_or_default();
        let transport: Arc<dyn Transport> = match o.transport {
            Some(t) => t,
            None => Arc::new(forge::ReqwestTransport::new()?),
        };
        let forge: Arc<dyn Forge> = match sb.forge.as_str() {
            "forgejo" => Arc::new(
                forge::forgejo::Client::new(
                    &sb.base_url,
                    &sb.org,
                    token,
                    transport,
                    cancel.clone(),
                )
                .map_err(|e| CommandError::Other(format!("sandbox {:?}: {e}", sb.name)))?,
            ),
            "github" => Arc::new(
                forge::github::Client::new(&sb.base_url, &sb.org, token, transport, cancel.clone())
                    .map_err(|e| CommandError::Other(format!("sandbox {:?}: {e}", sb.name)))?,
            ),
            "gitlab" => Arc::new(
                forge::gitlab::Client::new(&sb.base_url, &sb.org, token, transport, cancel.clone())
                    .map_err(|e| CommandError::Other(format!("sandbox {:?}: {e}", sb.name)))?,
            ),
            "azuredevops" => Arc::new(
                forge::azuredevops::Client::new(
                    &sb.base_url,
                    &sb.org,
                    &sb.default_project,
                    token,
                    transport,
                    cancel.clone(),
                )
                .map_err(|e| CommandError::Other(format!("sandbox {:?}: {e}", sb.name)))?,
            ),
            other => {
                return Err(CommandError::Other(format!(
                    "sandbox {:?}: forge {other:?} is not supported (forgejo, github, gitlab and azuredevops are)",
                    sb.name
                )));
            }
        };
        let concurrency = o
            .concurrency
            .or(sb.concurrency)
            .unwrap_or_else(|| forge.policy().default_concurrency)
            .max(1);
        Ok(Env {
            sandbox: sb,
            forge,
            fleet_dir,
            concurrency,
            cancel,
            yes: o.yes,
            verbose: o.verbose,
            input: Mutex::new(o.input),
            out: o
                .out
                .unwrap_or_else(|| Arc::new(Mutex::new(std::io::stdout()))),
        })
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.fleet_dir.join(fleet::LOCK_FILE)
    }

    pub(crate) fn printf(&self, s: impl AsRef<str>) {
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = out.write_all(s.as_ref().as_bytes());
        let _ = out.flush();
    }

    pub(crate) fn debugf(&self, s: impl AsRef<str>) {
        if self.verbose {
            self.printf(s);
        }
    }

    pub(crate) fn header(&self, verb: &str, n: usize) {
        self.printf(format!(
            "\n  {verb}   sandbox {} · {} · {}/{} · {n} repositories\n\n",
            self.sandbox.name,
            self.sandbox.forge,
            self.sandbox.base_url.trim_end_matches('/'),
            self.sandbox.org
        ));
    }

    /// Reads the lock and checks it still describes the fleet on disk. Resetting against a
    /// stale baseline is not something to retry, so both failures are guard errors.
    pub(crate) fn load_lock(&self) -> Result<Lock, CommandError> {
        let lock = match fleet::read_lock(&self.lock_path()) {
            Ok(l) => l,
            Err(e) if e.is_not_found() => {
                return Err(CommandError::Guard(format!(
                    "no {}: run `forgelab apply --sandbox {}`",
                    fleet::LOCK_FILE,
                    self.sandbox.name
                )));
            }
            Err(e) => return Err(CommandError::Guard(e.to_string())),
        };
        let digest = fleet::digest(&self.fleet_dir)?;
        if digest != lock.fleet_digest {
            return Err(CommandError::Guard(format!(
                "the fleet changed since {} was written: run `forgelab apply --sandbox {}` and commit the lock",
                fleet::LOCK_FILE,
                self.sandbox.name
            )));
        }
        Ok(lock)
    }

    /// The lock as it was before this run, if there is one that parses. Advisory: apply uses
    /// it to know what an earlier apply created and the fleet no longer declares.
    pub(crate) fn previous_lock(&self) -> Option<Lock> {
        fleet::read_lock(&self.lock_path()).ok()
    }

    /// Asks before destroy, the one command that deletes. --yes skips it.
    pub(crate) fn confirm(&self) -> Result<(), CommandError> {
        if self.yes {
            return Ok(());
        }
        self.printf("\n  Delete them? [y/N] ");
        let mut input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        let line = match input.as_mut() {
            Some(r) => read_line(r.as_mut()),
            None => read_line(&mut std::io::stdin().lock()),
        };
        confirm_answer(&line)
    }

    /// Looks a repository up, and does not take one "missing" answer for it: a forge may
    /// answer 404 for a moment after a write to a repository that is there. A missing
    /// repository is an exit-2 verdict for verify and a skipped deletion for destroy, so it
    /// is asked again, for a few seconds, before it is believed.
    pub(crate) async fn get_confirmed(
        &self,
        name: &str,
    ) -> Result<Option<forge::Repo>, CommandError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        let mut pause = std::time::Duration::from_millis(100);
        loop {
            match self.forge.get(name).await? {
                Some(r) => return Ok(Some(r)),
                None if tokio::time::Instant::now() >= deadline => return Ok(None),
                None => {
                    tokio::select! {
                        _ = tokio::time::sleep(pause) => {}
                        _ = self.cancel.cancelled() => return Err(CommandError::Other("interrupted".into())),
                    }
                    pause = (pause * 2).min(std::time::Duration::from_secs(1));
                }
            }
        }
    }

    pub(crate) fn git_remote(&self, name: &str) -> Result<GitRemote, CommandError> {
        Ok(self.forge.git_remote(name)?)
    }

    /// Where the fleet lives, for the fixture builder.
    pub(crate) fn root(&self) -> &Path {
        &self.fleet_dir
    }
}

fn read_line(r: &mut dyn BufRead) -> String {
    let mut line = String::new();
    let _ = r.read_line(&mut line);
    line
}

/// Deliberately a plain y/N: with one sandbox there is nothing to mistake it for. Typing the
/// sandbox name earns its keep once two cloud sandboxes exist on the same forge -- and then
/// the prompt must not print the expected answer.
pub(crate) fn confirm_answer(line: &str) -> Result<(), CommandError> {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(CommandError::Other(
            "not confirmed; nothing was changed".into(),
        )),
    }
}

/// The declared topics plus the sandbox marker.
pub(crate) fn with_marker(topics: &[String], marker: &str) -> Vec<String> {
    let mut out = topics.to_vec();
    out.push(marker.to_string());
    out
}

/// The forge's topics without the sandbox marker, sorted for comparison.
pub(crate) fn without_marker(topics: &[String], marker: &str) -> Vec<String> {
    let mut out: Vec<String> = topics
        .iter()
        .filter(|t| t.as_str() != marker)
        .cloned()
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_answers() {
        for (input, want) in [
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
            ("", false),
            ("local\n", false),
        ] {
            assert_eq!(confirm_answer(input).is_ok(), want, "{input:?}");
        }
    }

    #[test]
    fn markers() {
        assert_eq!(with_marker(&["a".into()], "m"), ["a", "m"]);
        assert_eq!(
            without_marker(&["m".into(), "b".into(), "a".into()], "m"),
            ["a", "b"]
        );
    }
}
