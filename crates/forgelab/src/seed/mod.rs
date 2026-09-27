//! Turns fixture directories into deterministic git commits and moves refs on a forge. git
//! runs as a subprocess, never through a library, because the determinism guarantees are
//! about the exact environment git runs in.

pub mod env;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use tempfile::TempDir;

use crate::fleet::{GitIdentity, Repo, is_os_junk};

/// Marks the seed commit on the forge. Reset points the default branch back at it, and
/// because the tag is already on the server that push transfers no objects.
pub const BASELINE_TAG: &str = "forgelab-baseline";

/// How git failures are reported. The message never contains a credential.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("{0}")]
    Git(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

impl SeedError {
    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        SeedError::Io { context: context.into(), source }
    }
}

/// An authenticated git remote. The URL carries no credential; the credential travels as a
/// header (see `env::git_env`).
#[derive(Clone, Debug)]
pub struct GitRemote {
    pub url: url::Url,
    pub auth: Option<GitAuth>,
}

/// A username and secret that a forge accepts as Basic authentication over HTTP.
#[derive(Clone, Debug)]
pub struct GitAuth {
    pub username: String,
    pub secret: SecretString,
}

impl GitRemote {
    /// `scheme://host[:port]`, the scope the Authorization header is limited to.
    pub fn origin(&self) -> String {
        let mut o = format!("{}://{}", self.url.scheme(), self.url.host_str().unwrap_or(""));
        if let Some(p) = self.url.port() {
            o.push(':');
            o.push_str(&p.to_string());
        }
        o
    }

    /// Everything in a git message that must never reach a log: the secret itself and its
    /// base64 form in the header.
    fn secrets(&self) -> Vec<String> {
        match &self.auth {
            Some(a) => vec![a.secret.expose_secret().to_string(), a.basic()],
            None => Vec::new(),
        }
    }
}

/// A fixture materialised as a local git repository, ready to push.
pub struct Built {
    /// The seed commit.
    pub sha: String,
    dir: TempDir,
    branch: String,
    env: Vec<(String, String)>,
}

/// Materialises one fixture directory as a git repository with a pinned identity and clock.
/// It needs git and nothing else: no forge, no network.
///
/// Content is pushed with git rather than written through a forge's contents API on
/// purpose: the API stamps the current time into every commit, which would give a different
/// SHA on every run and leave nothing stable to assert against.
///
/// Every repository is committed at the same pinned timestamp rather than at an increasing
/// one, so a repository's SHA is a function of its own content alone. Adding a new fixture
/// therefore cannot change any existing repository's SHA.
pub async fn build(root: &Path, r: &Repo, id: &GitIdentity) -> Result<Built, SeedError> {
    let work = tempfile::Builder::new()
        .prefix(&format!("forgelab-{}-", r.name.replace('/', "-")))
        .tempdir()
        .map_err(|e| SeedError::io("create scratch directory", e))?;
    let env = env::git_env(Some(id), None);

    copy_tree(&root.join(&r.dir), work.path()).map_err(|e| SeedError::Invalid(format!("copy {}: {e}", r.dir)))?;

    let mut steps: Vec<Vec<String>> = vec![
        strs(&["-c", &format!("init.defaultBranch={}", r.default_branch), "init", "-q"]),
        strs(&["add", "-A"]),
        strs(&["commit", "-q", "--allow-empty", "-m", &format!("seed: {}", r.name)]),
        strs(&["tag", BASELINE_TAG]),
    ];
    for tag in &r.tags {
        steps.push(strs(&["tag", tag]));
    }
    for args in &steps {
        git(work.path(), &env, &[], args).await?;
    }
    let sha = git(work.path(), &env, &[], &strs(&["rev-parse", "HEAD"])).await?;
    Ok(Built { sha, dir: work, branch: r.default_branch.clone(), env })
}

impl Built {
    /// Force-pushes the seed commit, the declared tags and the baseline tag.
    ///
    /// Forced because the fixture directory is authoritative: re-seeding a changed fixture
    /// replaces the single seed commit, which is never a fast-forward.
    pub async fn push(&self, remote: &GitRemote) -> Result<(), SeedError> {
        let mut env = self.env.clone();
        env.extend(env::git_env(None, Some(remote)).into_iter().filter(|(k, _)| k.starts_with("GIT_CONFIG_")));
        let args = strs(&["push", "-q", "--force", remote.url.as_str(), &format!("refs/heads/{}", self.branch), "--tags"]);
        git_network(self.dir.path(), &env, &remote.secrets(), &args).await.map(drop)
    }

    /// The scratch directory, for tests.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// Points `branch`, and every declared tag, back at the baseline tag that is already on the
/// forge. It fetches one commit and pushes refs only -- no fixture tree, no clone cache, and
/// no knowledge of what the content is.
pub async fn reset_to_baseline(remote: &GitRemote, branch: &str, tags: &[String]) -> Result<(), SeedError> {
    let work = scratch("forgelab-reset-")?;
    let env = env::git_env(None, Some(remote));
    let base = format!("refs/tags/{BASELINE_TAG}");
    let mut push = strs(&["push", "-q", "--force", remote.url.as_str(), &format!("{base}:refs/heads/{branch}")]);
    for tag in tags {
        push.push(format!("{base}:refs/tags/{tag}"));
    }
    git(work.path(), &env, &[], &strs(&["init", "-q", "--bare"])).await?;
    git_network(work.path(), &env, &remote.secrets(), &strs(&["fetch", "-q", "--depth=1", remote.url.as_str(), &format!("{base}:{base}")])).await?;
    git_network(work.path(), &env, &remote.secrets(), &push).await?;
    Ok(())
}

/// Deletes refs on the forge: `refs/tags/v1`, `refs/heads/old`. Used by apply to remove what
/// an earlier apply created and the fleet no longer declares.
pub async fn delete_remote_refs(remote: &GitRemote, refs: &[String]) -> Result<(), SeedError> {
    if refs.is_empty() {
        return Ok(());
    }
    let work = scratch("forgelab-delref-")?;
    let env = env::git_env(None, Some(remote));
    let mut push = strs(&["push", "-q", remote.url.as_str()]);
    for r in refs {
        push.push(format!(":{r}"));
    }
    git(work.path(), &env, &[], &strs(&["init", "-q", "--bare"])).await?;
    git_network(work.path(), &env, &remote.secrets(), &push).await.map(drop)
}

/// Reads a repository's branches and tags straight from git, as name -> commit SHA.
///
/// This, not a forge's branch and tag listings, is what the sandbox is compared against:
/// those listings are caches, and at least one forge serves them stale for seconds after a
/// write -- long enough for a verify run right after a test to miss a pushed commit. git has
/// no such window, answers the same way on every forge, and returns both kinds of ref in one
/// round trip. An annotated tag is reported at the commit it points to.
pub async fn ls_remote(remote: &GitRemote) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>), SeedError> {
    let work = scratch("forgelab-ls-")?;
    let env = env::git_env(None, Some(remote));
    let out = git_network(work.path(), &env, &remote.secrets(), &strs(&["ls-remote", "--heads", "--tags", remote.url.as_str()])).await?;
    Ok(parse_ls_remote(&out))
}

/// Parses `git ls-remote` output. The peeled line (`^{}`) follows the tag's own and wins.
pub fn parse_ls_remote(out: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut branches = BTreeMap::new();
    let mut tags = BTreeMap::new();
    for line in out.lines() {
        let Some((sha, r)) = line.split_once('\t') else { continue };
        if let Some(b) = r.strip_prefix("refs/heads/") {
            branches.insert(b.to_string(), sha.to_string());
        } else if let Some(t) = r.strip_suffix("^{}") {
            if let Some(t) = t.strip_prefix("refs/tags/") {
                tags.insert(t.to_string(), sha.to_string());
            }
        } else if let Some(t) = r.strip_prefix("refs/tags/") {
            tags.entry(t.to_string()).or_insert_with(|| sha.to_string());
        }
    }
    (branches, tags)
}

/// Checks that the git on PATH is recent enough to take credentials through `GIT_CONFIG_*`
/// (2.31, March 2021). Called once, before any network operation.
pub async fn check_git() -> Result<String, SeedError> {
    let out = git(Path::new("."), &env::git_env(None, None), &[], &strs(&["--version"])).await?;
    let version = out.trim().strip_prefix("git version ").unwrap_or(out.trim()).to_string();
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) < (2, 31) {
        return Err(SeedError::Invalid(format!(
            "git {version} is too old: forgelab needs git 2.31 or newer to pass credentials without putting them on the command line"
        )));
    }
    Ok(version)
}

fn scratch(prefix: &str) -> Result<TempDir, SeedError> {
    tempfile::Builder::new().prefix(prefix).tempdir().map_err(|e| SeedError::io("create scratch directory", e))
}

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// git failures that a second try tends to cure. Everything else is reported at once.
fn is_transient_git_failure(stderr: &str) -> bool {
    const HINTS: &[&str] = &[
        "HTTP 500",
        "HTTP 502",
        "HTTP 503",
        "HTTP 504",
        "HTTP 429",
        "Connection reset",
        "Connection refused",
        "Could not resolve host",
        "early EOF",
        "RPC failed",
        "unexpected disconnect",
        "The remote end hung up unexpectedly",
        "Operation timed out",
        "Empty reply from server",
        "Recv failure",
        "Send failure",
        "SSL_read",
    ];
    HINTS.iter().any(|h| stderr.contains(h))
}

/// Runs a git command that talks to a forge, retrying a transient failure a few times.
async fn git_network(dir: &Path, env: &[(String, String)], secrets: &[String], args: &[String]) -> Result<String, SeedError> {
    let mut wait = Duration::from_secs(1);
    for attempt in 1..=3 {
        match git(dir, env, secrets, args).await {
            Ok(out) => return Ok(out),
            Err(SeedError::Git(msg)) if attempt < 3 && is_transient_git_failure(&msg) => {
                tracing::debug!(attempt, "git: transient failure, retrying: {msg}");
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Runs one git command in `dir` and returns its trimmed stdout. `secrets` must not reach a
/// log or a CI transcript, and are redacted from the error.
async fn git(dir: &Path, env: &[(String, String)], secrets: &[String], args: &[String]) -> Result<String, SeedError> {
    let mut full: Vec<String> = strs(&[
        "-c", "commit.gpgsign=false",
        "-c", "tag.gpgsign=false",
        "-c", "gc.auto=0",
        "-c", "core.autocrlf=false",
        // Not covered by GIT_CONFIG_GLOBAL=/dev/null: git looks for the global ignore and
        // attributes files at their default XDG paths regardless of which config files it
        // reads. A machine with "*.properties" in ~/.config/git/ignore would otherwise drop
        // that file from the commit and produce a different SHA, silently, on that machine only.
        "-c", "core.excludesFile=/dev/null",
        "-c", "core.attributesFile=/dev/null",
    ]);
    full.extend(args.iter().cloned());
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&full)
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = cmd.output().await.map_err(|e| SeedError::io("run git", e))?;
    if !out.status.success() {
        let status = match out.status.code() {
            Some(c) => format!("exit status {c}"),
            None => "killed by signal".to_string(),
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(SeedError::Git(format!(
            "git {}: {status}: {}",
            redact(&args.join(" "), secrets),
            redact(stderr.trim(), secrets)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Removes credentials from text that may reach a log or a CI transcript.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s, "<redacted>");
        }
    }
    out
}

/// Materialises a fixture directory into an empty working directory. File modes are
/// normalised to 0644/0755 so that a checkout's umask cannot leak into the commit.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    fn walk(src: &Path, rel: &Path, dst: &Path) -> Result<(), String> {
        let here = src.join(rel);
        let mut entries: Vec<_> = std::fs::read_dir(&here)
            .map_err(|e| format!("read {}: {e}", here.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read {}: {e}", here.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let rel = rel.join(&name);
            let target = dst.join(&rel);
            let ft = e.file_type().map_err(|err| format!("{}: {err}", rel.display()))?;
            if ft.is_dir() {
                std::fs::create_dir_all(&target).map_err(|err| format!("mkdir {}: {err}", target.display()))?;
                walk(src, &rel, dst)?;
                continue;
            }
            if is_os_junk(&name.to_string_lossy()) {
                continue;
            }
            if !ft.is_file() {
                return Err(format!("{}: only regular files and directories may be seeded", src.join(&rel).display()));
            }
            let meta = e.metadata().map_err(|err| format!("{}: {err}", rel.display()))?;
            let mode = if crate::fleet::digest::is_executable(&meta) { 0o755 } else { 0o644 };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|err| format!("mkdir {}: {err}", parent.display()))?;
            }
            std::fs::copy(src.join(&rel), &target).map_err(|err| format!("copy {}: {err}", rel.display()))?;
            set_mode(&target, mode).map_err(|err| format!("chmod {}: {err}", rel.display()))?;
        }
        Ok(())
    }
    walk(src, Path::new(""), dst)
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The executable bit is part of the git tree object and therefore of the commit SHA,
        // so it is set explicitly rather than left to the umask.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// A path type alias kept for readers of the Go code, where a fixture was an `fs.FS` path.
pub type FixtureDir = PathBuf;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{Author, GitIdentity, Repo};

    fn identity() -> GitIdentity {
        GitIdentity {
            author: Author { name: "Forgelab Fixture".into(), email: "fixture@forgelab.test".into() },
            timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
        }
    }

    fn fixture() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let svc = dir.path().join("repos/svc");
        std::fs::create_dir_all(&svc).unwrap();
        std::fs::write(svc.join("README.md"), "# svc\n").unwrap();
        std::fs::write(svc.join("app.properties"), "a=b\n").unwrap();
        std::fs::write(svc.join("run.sh"), "#!/bin/sh\n").unwrap();
        set_mode(&svc.join("run.sh"), 0o755).unwrap();
        std::fs::write(svc.join(".DS_Store"), "junk").unwrap();
        let repo = Repo { name: "svc".into(), dir: "repos/svc".into(), default_branch: "main".into(), tags: vec!["v1".into()], ..Repo::default() };
        (dir, repo)
    }

    async fn build_sha(root: &Path, repo: &Repo) -> String {
        build(root, repo, &identity()).await.unwrap().sha
    }

    #[tokio::test]
    async fn build_is_deterministic() {
        let (dir, repo) = fixture();
        let (first, second) = (build_sha(dir.path(), &repo).await, build_sha(dir.path(), &repo).await);
        assert_eq!(first, second);
        assert_eq!(first.len(), 40);
    }

    /// A global ignore file is read from its default XDG path even when GIT_CONFIG_GLOBAL is
    /// /dev/null. Without core.excludesFile=/dev/null this machine would silently commit a
    /// different tree. Also: an inherited GIT_DIR would retarget the seeder at the caller's
    /// own repository. Both are one test because they both set process-wide variables.
    #[tokio::test]
    async fn build_ignores_the_callers_environment() {
        let (dir, repo) = fixture();
        let want = build_sha(dir.path(), &repo).await;

        let home = tempfile::tempdir().unwrap();
        let ignore = home.path().join(".config/git/ignore");
        std::fs::create_dir_all(ignore.parent().unwrap()).unwrap();
        std::fs::write(&ignore, "*.properties\n").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // SAFETY: tests in this module run single-threaded with respect to these variables
        // and restore them below.
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("GIT_DIR", elsewhere.path().join("elsewhere.git"));
            std::env::set_var("GIT_WORK_TREE", elsewhere.path());
        }
        let got = build_sha(dir.path(), &repo).await;
        unsafe {
            match old_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
        }
        assert_eq!(got, want, "the caller's environment changed the SHA");
    }

    #[test]
    fn redact_removes_every_secret() {
        let secrets = vec!["s3cret".to_string(), "czNjcmV0".to_string()];
        assert_eq!(redact("push s3cret failed czNjcmV0", &secrets), "push <redacted> failed <redacted>");
    }

    #[test]
    fn ls_remote_prefers_peeled_tags() {
        let out = "aaa\trefs/heads/main\nbbb\trefs/tags/v1\nccc\trefs/tags/v1^{}\nddd\trefs/tags/v2\n";
        let (branches, tags) = parse_ls_remote(out);
        assert_eq!(branches["main"], "aaa");
        assert_eq!(tags["v1"], "ccc");
        assert_eq!(tags["v2"], "ddd");
    }

    #[test]
    fn auth_header_is_scoped_to_the_origin() {
        let remote = GitRemote {
            url: "http://localhost:3000/org/repo.git".parse().unwrap(),
            auth: Some(GitAuth { username: "forgelab".into(), secret: "tok".to_string().into() }),
        };
        let env = env::git_env(None, Some(&remote));
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("GIT_CONFIG_KEY_0"), "http.http://localhost:3000/.extraHeader");
        assert_eq!(get("GIT_CONFIG_VALUE_0"), "Authorization: Basic Zm9yZ2VsYWI6dG9r");
        assert!(env.iter().all(|(k, _)| k != "GIT_DIR"));
    }

    #[tokio::test]
    async fn git_is_recent_enough() {
        check_git().await.unwrap();
    }
}
