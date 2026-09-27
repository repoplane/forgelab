//! `sandboxes.yaml`: where a fleet may be applied.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::report::CommandError;

/// The default name of the sandbox configuration inside a fleet directory.
pub const CONFIG_FILE: &str = "sandboxes.yaml";

/// Set on every repository forgelab creates. It is how forgelab tells its own repositories
/// from a same-named stranger, and how a consumer can filter an organisation listing down to
/// the fleet.
pub const DEFAULT_MARKER_TOPIC: &str = "forgelab-managed";

const CONFIG_VERSION: i64 = 1;

/// sandboxes.yaml.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub sandboxes: BTreeMap<String, Sandbox>,
    /// No longer used; still parsed so that `open` can say so. It was a pattern the org had to
    /// match, kept in the same file as the org it was checking -- a speed bump, not a guard.
    /// What scopes a sandbox is what its token can reach, and what protects a wrong target is
    /// checked on the forge: the marker, and declared-repos-only.
    #[serde(default)]
    pub org_allowlist: String,
}

/// One forge organisation. Credentials are descriptors, never values: the file names an
/// environment variable, so nothing secret is committed or passed as an argument.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    #[serde(skip)]
    pub name: String,
    #[serde(default)]
    pub forge: String,
    #[serde(default)]
    pub base_url: String,
    /// On GitLab: the group's full path, e.g. acme-sandbox/services.
    #[serde(default)]
    pub org: String,
    /// Required on Azure DevOps, where every repository lives in a project, and unused
    /// elsewhere. It holds the repositories without a namespace; a namespace names a project
    /// of its own. forgelab makes and removes it like any other.
    #[serde(default)]
    pub default_project: String,
    /// What default_project was called; still parsed so that `sandbox` can say so.
    #[serde(default, rename = "project")]
    pub old_project: String,
    #[serde(default)]
    pub token_env: String,
    #[serde(default)]
    pub marker_topic: String,
    /// How many repositories to work on at once; the forge's own default when absent.
    #[serde(default)]
    pub concurrency: Option<usize>,
}

/// What a sandbox may be called. The names are typed on a command line and offered by shell
/// completion, so they stay free of anything a shell would interpret.
fn is_sandbox_name(s: &str) -> bool {
    crate::fleet::spec::is_repo_name(s)
}

/// A marker topic as forges store one: lowercase letters, digits, '.' and '-', 50 characters
/// at most.
fn is_marker_topic(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    s.len() <= 50
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
}

/// Applies the defaults: the fleet is the current directory, and the config sits in it.
pub fn resolve_paths(fleet_dir: &str, config_path: &str) -> (PathBuf, PathBuf) {
    let fleet = if fleet_dir.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(fleet_dir)
    };
    let config = if config_path.is_empty() {
        fleet.join(CONFIG_FILE)
    } else {
        PathBuf::from(config_path)
    };
    (fleet, config)
}

/// The sandboxes a config declares, sorted; empty if the config cannot be read. It exists for
/// shell completion, which wants names and no errors -- and whose input is a file that may
/// have come with somebody else's repository, so a name a shell could interpret is never
/// returned.
pub fn names(fleet_dir: &str, config_path: &str) -> Vec<String> {
    let (_, config) = resolve_paths(fleet_dir, config_path);
    match load_config(&config) {
        Ok(cfg) => cfg
            .sandboxes
            .keys()
            .filter(|n| is_sandbox_name(n))
            .cloned()
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Reads sandboxes.yaml.
pub fn load_config(path: &Path) -> Result<Config, CommandError> {
    let raw = std::fs::read(path)
        .map_err(|e| CommandError::Other(format!("read {}: {e}", path.display())))?;
    let c: Config = serde_yaml_ng::from_slice(&raw)
        .map_err(|e| CommandError::Other(format!("parse {}: {e}", path.display())))?;
    if c.version != CONFIG_VERSION {
        return Err(CommandError::Other(format!(
            "{}: version is {}, want {CONFIG_VERSION}",
            path.display(),
            c.version
        )));
    }
    for name in c.sandboxes.keys() {
        if !is_sandbox_name(name) {
            return Err(CommandError::Other(format!(
                "{}: sandbox name {name:?}: use letters, digits, '.', '_' and '-'",
                path.display()
            )));
        }
    }
    Ok(c)
}

impl Config {
    /// Resolves a sandbox by name. The org is only reachable through here: there is no flag
    /// that takes one.
    pub fn sandbox(&self, name: &str) -> Result<Sandbox, CommandError> {
        let Some(sb) = self.sandboxes.get(name) else {
            return Err(CommandError::Other(format!(
                "no sandbox {name:?} in the config"
            )));
        };
        let mut sb = sb.clone();
        sb.name = name.to_string();
        if sb.marker_topic.is_empty() {
            sb.marker_topic = DEFAULT_MARKER_TOPIC.to_string();
        }
        // Forges store topics lowercased; a marker that is not would never match what they
        // hold, and every repository would then fail the ownership guard.
        sb.marker_topic = sb.marker_topic.trim().to_lowercase();
        if !is_marker_topic(&sb.marker_topic) {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: marker_topic {:?}: use lowercase letters, digits, '.' and '-', 50 characters at most",
                sb.marker_topic
            )));
        }
        // Only a self-hosted GitHub or GitLab needs to say where it lives.
        if sb.base_url.is_empty() {
            sb.base_url = match sb.forge.as_str() {
                "github" => crate::forge::github::DEFAULT_BASE_URL,
                "gitlab" => crate::forge::gitlab::DEFAULT_BASE_URL,
                "azuredevops" => crate::forge::azuredevops::DEFAULT_BASE_URL,
                _ => "",
            }
            .to_string();
        }
        if !sb.old_project.is_empty() {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: project is now called default_project"
            )));
        }
        if sb.forge == "azuredevops" && sb.default_project.is_empty() {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: azuredevops needs a default_project"
            )));
        }
        // Elsewhere there is no such thing, and destroy would list it as a namespace to remove.
        if sb.forge != "azuredevops" && !sb.default_project.is_empty() {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: default_project only exists on azuredevops, not on {}",
                sb.forge
            )));
        }
        if sb.forge.is_empty()
            || sb.base_url.is_empty()
            || sb.org.is_empty()
            || sb.token_env.is_empty()
        {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: forge, base_url, org and token_env are required"
            )));
        }
        match url::Url::parse(&sb.base_url) {
            Ok(u) if u.host_str().is_some_and(|h| !h.is_empty()) => {}
            _ => {
                return Err(CommandError::Other(format!(
                    "sandbox {name:?}: base_url {:?} is not a URL",
                    sb.base_url
                )));
            }
        }
        if sb.concurrency == Some(0) {
            return Err(CommandError::Other(format!(
                "sandbox {name:?}: concurrency must be at least 1"
            )));
        }
        Ok(sb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CONFIG: &str = "version: 1
org_allowlist: '^this-is-ignored-now$'
sandboxes:
  gh:      {forge: github,      org: acme-sandbox, token_env: T}
  gl:      {forge: gitlab,      org: acme-sandbox/services, token_env: T}
  ado:     {forge: azuredevops, org: acme, default_project: sandbox, token_env: T}
  local:   {forge: forgejo,     base_url: \"http://127.0.0.1:3000\", org: anything, token_env: T}
  nourl:   {forge: forgejo,     org: acme-sandbox, token_env: T}
  badurl:  {forge: forgejo,     base_url: \"localhost:3000\", org: acme-sandbox, token_env: T}
  partial: {forge: github,      org: acme-sandbox}
  noproj:  {forge: azuredevops, org: acme, token_env: T}
  oldproj: {forge: azuredevops, org: acme, project: sandbox, token_env: T}
  ghproj:  {forge: github,      org: acme-sandbox, default_project: fleet, token_env: T}
  upper:   {forge: github,      org: acme-sandbox, token_env: T, marker_topic: \" Forgelab-Managed \"}
  badmark: {forge: github,      org: acme-sandbox, token_env: T, marker_topic: \"a b\"}
  paced:   {forge: github,      org: acme-sandbox, token_env: T, concurrency: 2}
";

    #[test]
    fn sandbox_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, TEST_CONFIG).unwrap();
        let cfg = load_config(&path).unwrap();

        // Hosted forges know where they live; the marker topic has a default.
        for (name, base_url, org) in [
            ("gh", "https://github.com", "acme-sandbox"),
            ("gl", "https://gitlab.com", "acme-sandbox/services"),
            ("ado", "https://dev.azure.com", "acme"),
            ("local", "http://127.0.0.1:3000", "anything"),
        ] {
            let sb = cfg.sandbox(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                (
                    sb.base_url.as_str(),
                    sb.org.as_str(),
                    sb.marker_topic.as_str()
                ),
                (base_url, org, DEFAULT_MARKER_TOPIC),
                "{name}"
            );
        }
        // A left-over org_allowlist no longer refuses anything, but is still parsed.
        assert!(!cfg.org_allowlist.is_empty());

        for name in [
            "nourl", "badurl", "partial", "noproj", "ghproj", "nope", "badmark",
        ] {
            assert!(cfg.sandbox(name).is_err(), "{name}: want an error");
        }
        // The key was renamed; a config still using the old one is told, not silently ignored.
        let err = cfg.sandbox("oldproj").unwrap_err().to_string();
        assert!(err.contains("default_project"), "{err}");
        // A marker is stored lowercase by every forge, so it is read that way.
        assert_eq!(
            cfg.sandbox("upper").unwrap().marker_topic,
            "forgelab-managed"
        );
        assert_eq!(cfg.sandbox("paced").unwrap().concurrency, Some(2));
    }

    /// Sandbox names are typed in a shell and offered by completion, so a name a shell would
    /// interpret is refused at load -- and never listed, even from a config that fails to load.
    #[test]
    fn hostile_sandbox_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "version: 1\nsandboxes:\n  ok: {forge: github, org: x, token_env: T}\n  '$(reboot)': {forge: github, org: x, token_env: T}\n").unwrap();
        assert!(load_config(&path).is_err());
        assert!(names(dir.path().to_str().unwrap(), "").is_empty());
    }

    #[test]
    fn typos_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &path,
            "version: 1\nsandboxes:\n  gh: {forge: github, org: x, token-env: T}\n",
        )
        .unwrap();
        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("token-env"), "{err}");
    }
}
