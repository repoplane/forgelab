//! The environment git runs in: built from scratch, never inherited.

use base64::Engine as _;
use secrecy::ExposeSecret as _;

use super::GitRemote;
use crate::fleet::GitIdentity;

/// Variables handed through from the caller. Proxies and CA bundles have to reach git for a
/// forge behind a corporate proxy or with a private certificate to work; the API client
/// honours the same variables.
const PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "GIT_SSL_NO_VERIFY",
];

/// Builds the environment from scratch rather than inheriting it. Any GIT_DIR,
/// GIT_WORK_TREE, GIT_INDEX_FILE or GIT_OBJECT_DIRECTORY in the caller's environment would
/// redirect these commands at the caller's own repository -- which happens for real inside
/// a git hook, `git rebase --exec` or `git bisect run`.
///
/// Credentials travel as an `Authorization` header scoped to the forge's origin through
/// `GIT_CONFIG_*`, never in the URL: an argument is visible to every user of the machine in
/// `ps`, an environment variable only to the process and root.
pub fn git_env(id: Option<&GitIdentity>, remote: Option<&GitRemote>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for key in PASSTHROUGH {
        if let Ok(v) = std::env::var(key) {
            env.push((key.to_string(), v));
        }
    }
    if let Some(id) = id {
        let stamp = format_timestamp(&id.timestamp);
        env.push(("GIT_AUTHOR_NAME".into(), id.author.name.clone()));
        env.push(("GIT_AUTHOR_EMAIL".into(), id.author.email.clone()));
        env.push(("GIT_AUTHOR_DATE".into(), stamp.clone()));
        env.push(("GIT_COMMITTER_NAME".into(), id.author.name.clone()));
        env.push(("GIT_COMMITTER_EMAIL".into(), id.author.email.clone()));
        env.push(("GIT_COMMITTER_DATE".into(), stamp));
    }
    env.push(("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()));
    env.push(("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()));
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    if let Some(remote) = remote
        && let Some(auth) = &remote.auth
    {
        env.push(("GIT_CONFIG_COUNT".into(), "1".into()));
        env.push((
            "GIT_CONFIG_KEY_0".into(),
            format!("http.{}/.extraHeader", remote.origin()),
        ));
        env.push((
            "GIT_CONFIG_VALUE_0".into(),
            format!("Authorization: Basic {}", auth.basic()),
        ));
    }
    env
}

/// The pinned clock the way Go's `time.Time.UTC().Format(time.RFC3339)` prints it.
pub fn format_timestamp(ts: &jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl super::GitAuth {
    /// The `user:secret` pair, base64-encoded for a Basic Authorization header.
    pub fn basic(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            self.username,
            self.secret.expose_secret()
        ))
    }
}
