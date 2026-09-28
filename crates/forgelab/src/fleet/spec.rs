//! `fleet.yaml` plus the directory tree under `repos/`, resolved into one repository list.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::FleetError;
use super::digest::is_os_junk;

/// The fleet.yaml schema version this crate understands. It is checked rather than ignored so
/// that a future schema cannot be silently read under today's rules.
pub const SPEC_VERSION: i64 = 1;

/// Fixed names inside a fleet directory.
pub const SPEC_FILE: &str = "fleet.yaml";
pub const REPOS_DIR: &str = "repos";

pub const VISIBILITY_PRIVATE: &str = "private";
pub const VISIBILITY_PUBLIC: &str = "public";

const DEFAULT_BRANCH: &str = "main";

/// A fleet: what to create, and the pinned identity to create it with.
#[derive(Debug, Clone)]
pub struct Spec {
    pub version: i64,
    pub git: GitIdentity,
    pub repos: Vec<Repo>,
}

impl Spec {
    /// Every namespace the repositories sit in, ancestors included, each one before what is
    /// inside it.
    pub fn namespaces(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        for r in &self.repos {
            let mut ns = parent(&r.name);
            while let Some(p) = ns {
                if !seen.insert(p.to_string()) {
                    break;
                }
                ns = parent(p);
            }
        }
        seen.into_iter().collect()
    }
}

fn parent(name: &str) -> Option<&str> {
    name.rfind('/').map(|i| &name[..i])
}

/// The author, committer and clock used for every seeded commit, which is what makes the
/// resulting commit SHAs identical across machines and across runs.
#[derive(Debug, Clone)]
pub struct GitIdentity {
    pub author: Author,
    pub timestamp: jiff::Timestamp,
}

/// A git identity.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Author {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
}

/// One declared repository. `name` and `dir` come from the directory tree; the remaining
/// fields come from the defaults and the optional overrides in fleet.yaml.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    /// The path under repos/: "dotfiles", or "platform/core/api" inside namespaces.
    pub name: String,
    /// The fixture directory, relative to the fleet directory: "repos/platform/core/api".
    pub dir: String,
    pub default_branch: String,
    pub visibility: String,
    pub topics: Vec<String>,
    pub archived: bool,
    /// Empty repositories are created and never pushed to: no commits, no branches.
    pub empty: bool,
    /// Tags all point at the seed commit.
    pub tags: Vec<String>,
}

/// The fleet.yaml entry for one repository. Every field is optional, so a repository that
/// needs nothing special can be absent from the file entirely.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Override {
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    topics: Option<Vec<String>>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    empty: bool,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitFile {
    #[serde(default)]
    author: Author,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    visibility: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFile {
    #[serde(default)]
    version: i64,
    #[serde(default)]
    git: GitFile,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    repos: BTreeMap<String, Option<Override>>,
}

/// A repository or namespace directory name: `^[A-Za-z0-9][A-Za-z0-9._-]*$`.
pub fn is_repo_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Reads fleet.yaml from `root` and walks repos/ for fixture directories. The directory tree
/// is authoritative: an override for a repository that does not exist on disk is an error,
/// because it is almost always a typo or a stale entry.
pub fn load_spec(root: &Path) -> Result<Spec, FleetError> {
    let raw = fs::read(root.join(SPEC_FILE))
        .map_err(|e| FleetError::io(format!("read {SPEC_FILE}"), e))?;
    let ff: FleetFile = serde_yaml_ng::from_slice(&raw)
        .map_err(|e| FleetError::invalid(format!("parse {SPEC_FILE}: {e}")))?;
    if ff.version != SPEC_VERSION {
        return Err(FleetError::invalid(format!(
            "{SPEC_FILE}: version is {}, want {SPEC_VERSION}",
            ff.version
        )));
    }
    if ff.git.author.name.is_empty() || ff.git.author.email.is_empty() {
        return Err(FleetError::invalid(format!(
            "{SPEC_FILE}: git.author.name and git.author.email are required"
        )));
    }
    let timestamp = match ff.git.timestamp.as_deref().map(str::trim) {
        None | Some("") => {
            return Err(FleetError::invalid(format!(
                "{SPEC_FILE}: git.timestamp is required and pins the commit clock"
            )));
        }
        Some(s) => parse_timestamp(s).ok_or_else(|| {
            FleetError::invalid(format!(
                "{SPEC_FILE}: git.timestamp {s:?} is not an RFC 3339 time"
            ))
        })?,
    };
    let default_branch = non_empty(&ff.defaults.default_branch, DEFAULT_BRANCH);
    let default_visibility = non_empty(&ff.defaults.visibility, VISIBILITY_PRIVATE);

    let mut repos = walk_repos(root)?;
    if repos.is_empty() {
        return Err(FleetError::invalid(format!(
            "no repositories found under {REPOS_DIR}/"
        )));
    }

    let mut flat: HashMap<String, String> = HashMap::new();
    for r in &repos {
        // A forge without namespaces joins the path with "-". Two names that join to the same
        // one are refused everywhere, so that a fleet never works on one forge only. Forges
        // match names ignoring case, so Api and api are the same one too.
        let f = r.name.replace('/', "-");
        if let Some(other) = flat.get(&f.to_lowercase()) {
            return Err(FleetError::invalid(format!(
                "{REPOS_DIR}/: {other} and {} are both {f:?} on a forge without namespaces",
                r.name
            )));
        }
        flat.insert(f.to_lowercase(), r.name.clone());
    }
    for key in ff.repos.keys() {
        if !repos.iter().any(|r| &r.name == key) {
            return Err(FleetError::invalid(format!(
                "{SPEC_FILE}: override for {key:?} but no such directory under {REPOS_DIR}/"
            )));
        }
    }

    for r in &mut repos {
        let o = ff.repos.get(&r.name).and_then(Option::as_ref);
        let o = o.map_or_else(Override::default, |o| Override {
            default_branch: o.default_branch.clone(),
            visibility: o.visibility.clone(),
            topics: o.topics.clone(),
            archived: o.archived,
            empty: o.empty,
            tags: o.tags.clone(),
        });
        r.default_branch = non_empty(&o.default_branch, &default_branch);
        r.visibility = non_empty(&o.visibility, &default_visibility);
        r.topics = normalise_topics(o.topics.as_deref().unwrap_or(&[]));
        r.archived = o.archived;
        r.empty = o.empty;
        r.tags = o.tags.unwrap_or_default();
        r.tags.sort();

        if r.visibility != VISIBILITY_PRIVATE && r.visibility != VISIBILITY_PUBLIC {
            return Err(FleetError::invalid(format!(
                "{SPEC_FILE}: {}: visibility {:?}, want private or public",
                r.name, r.visibility
            )));
        }
        if r.empty && !r.tags.is_empty() {
            return Err(FleetError::invalid(format!(
                "{SPEC_FILE}: {}: an empty repository has no commit to tag",
                r.name
            )));
        }
        for t in &r.tags {
            if t == crate::seed::BASELINE_TAG {
                return Err(FleetError::invalid(format!(
                    "{SPEC_FILE}: {}: tag {t:?} is forgelab's own baseline tag and may not be declared",
                    r.name
                )));
            }
            if let Err(why) = check_refname(t) {
                return Err(FleetError::invalid(format!(
                    "{SPEC_FILE}: {}: tag {t:?} is not a valid git tag name: {why}",
                    r.name
                )));
            }
        }
        if let Err(why) = check_refname(&r.default_branch) {
            return Err(FleetError::invalid(format!(
                "{SPEC_FILE}: {}: default_branch {:?} is not a valid git branch name: {why}",
                r.name, r.default_branch
            )));
        }
    }

    Ok(Spec {
        version: ff.version,
        git: GitIdentity {
            author: ff.git.author,
            timestamp,
        },
        repos,
    })
}

/// Parses what YAML and Go's time package accept for a pinned clock: RFC 3339 with any
/// offset, a date-time without zone (taken as UTC), or a bare date (midnight UTC).
pub fn parse_timestamp(s: &str) -> Option<jiff::Timestamp> {
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts);
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return dt
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp());
    }
    if let Ok(d) = s.parse::<jiff::civil::Date>() {
        return d
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp());
    }
    None
}

/// The checks `git check-ref-format` applies to one ref name component sequence, without
/// running git. Returns why the name is refused.
pub fn check_refname(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("begins or ends with '/'");
    }
    if name.contains("//") {
        return Err("has an empty component");
    }
    if name.starts_with('-') {
        return Err("begins with '-'");
    }
    if name == "@" {
        return Err("is '@'");
    }
    if name.contains("..") || name.contains("@{") {
        return Err("contains '..' or '@{'");
    }
    if name.ends_with('.') || name.ends_with(".lock") {
        return Err("ends with '.' or '.lock'");
    }
    for component in name.split('/') {
        if component.starts_with('.') || component.ends_with(".lock") {
            return Err("a component begins with '.' or ends with '.lock'");
        }
    }
    if name.chars().any(|c| {
        c.is_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return Err("contains whitespace, a control character or one of ~ ^ : ? * [ \\");
    }
    Ok(())
}

fn non_empty(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

/// Every repository under repos/, sorted by name. The tree says which directory is which: one
/// that holds a file is a repository, and one that holds only directories is a namespace,
/// whose path becomes part of the names below it.
fn walk_repos(root: &Path) -> Result<Vec<Repo>, FleetError> {
    let mut out = walk_dir(root, REPOS_DIR)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Directory entries sorted by name, the order Go's fs.ReadDir returns them in.
fn sorted_entries(path: &Path, label: &str) -> Result<Vec<fs::DirEntry>, FleetError> {
    let rd = fs::read_dir(path).map_err(|e| FleetError::io(format!("read {label}/"), e))?;
    let mut entries = rd
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| FleetError::io(format!("read {label}/"), e))?;
    entries.sort_by_key(|a| a.file_name());
    Ok(entries)
}

fn walk_dir(root: &Path, dir: &str) -> Result<Vec<Repo>, FleetError> {
    let mut out = Vec::new();
    for e in sorted_entries(&root.join(dir), dir)? {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir || name.starts_with('.') {
            continue;
        }
        let p = format!("{dir}/{name}");
        if !is_repo_name(&name) {
            return Err(FleetError::invalid(format!(
                "{p}: not a valid repository name"
            )));
        }
        if holds_file(root, &p)? {
            out.push(Repo {
                name: p[REPOS_DIR.len() + 1..].to_string(),
                dir: p,
                ..Repo::default()
            });
            continue;
        }
        let nested = walk_dir(root, &p)?;
        if nested.is_empty() {
            return Err(FleetError::invalid(format!(
                "{p}: holds no file, so it is a namespace, yet no repository is under it"
            )));
        }
        out.extend(nested);
    }
    Ok(out)
}

fn holds_file(root: &Path, dir: &str) -> Result<bool, FleetError> {
    for e in sorted_entries(&root.join(dir), dir)? {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir && !is_os_junk(&e.file_name().to_string_lossy()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Lowercases, dedupes and sorts. Never nil: a forge reports an empty list as [], and a lock
/// that wrote null would diverge from it.
pub fn normalise_topics(input: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in input {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    const SPEC_YAML: &str = "version: 1\ngit:\n  author: {name: \"Forgelab Fixture\", email: \"fixture@forgelab.test\"}\n  timestamp: \"2026-01-01T00:00:00Z\"\nrepos:\n  legacy: {default_branch: master, topics: [Batch, legacy, batch]}\n  bare: {empty: true}\n";

    /// A fleet on disk: fleet.yaml plus the files listed as (path, content).
    pub(crate) fn fleet(spec: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(SPEC_FILE), spec).unwrap();
        for (p, c) in files {
            let full = dir.path().join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, c).unwrap();
        }
        dir
    }

    fn base(spec: &str) -> tempfile::TempDir {
        fleet(
            spec,
            &[
                ("repos/legacy/README.md", "# legacy\n"),
                ("repos/plain/README.md", "# plain\n"),
                ("repos/bare/.gitkeep", ""),
            ],
        )
    }

    fn add(dir: &tempfile::TempDir, p: &str, c: &str) {
        let full = dir.path().join(p);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, c).unwrap();
    }

    #[test]
    fn applies_defaults_and_overrides() {
        let dir = base(SPEC_YAML);
        let spec = load_spec(dir.path()).unwrap();
        assert_eq!(spec.repos.len(), 3);
        let (bare, legacy, plain) = (&spec.repos[0], &spec.repos[1], &spec.repos[2]);
        assert!(bare.empty);
        assert_eq!(legacy.default_branch, "master");
        assert_eq!(legacy.topics, vec!["batch", "legacy"]);
        assert_eq!(plain.default_branch, "main");
        assert_eq!(plain.visibility, VISIBILITY_PRIVATE);
        assert!(plain.topics.is_empty());
        assert_eq!(env::format(&spec.git.timestamp), "2026-01-01T00:00:00Z");
    }

    mod env {
        pub fn format(ts: &jiff::Timestamp) -> String {
            crate::seed::env::format_timestamp(ts)
        }
    }

    #[test]
    fn rejects() {
        let cases: Vec<(&str, String)> = vec![
            (
                "no such directory",
                SPEC_YAML.replacen("legacy:", "legcy:", 1),
            ),
            (
                "version is 2",
                SPEC_YAML.replacen("version: 1", "version: 2", 1),
            ),
            (
                "visibility",
                format!("{SPEC_YAML}  plain: {{visibility: internal}}\n"),
            ),
            (
                "no commit to tag",
                SPEC_YAML.replacen("{empty: true}", "{empty: true, tags: [v1]}", 1),
            ),
            // New in the port: typos are refused instead of ignored.
            (
                "unknown field `default-branch`",
                SPEC_YAML.replacen("default_branch", "default-branch", 1),
            ),
            (
                "baseline tag",
                SPEC_YAML.replacen("legacy: {", "legacy: {tags: [forgelab-baseline], ", 1),
            ),
            (
                "not a valid git tag name",
                SPEC_YAML.replacen("legacy: {", "legacy: {tags: [\"a b\"], ", 1),
            ),
            (
                "git.timestamp is required",
                SPEC_YAML.replacen("  timestamp: \"2026-01-01T00:00:00Z\"\n", "", 1),
            ),
            (
                "git.author.name and git.author.email",
                SPEC_YAML.replacen("email: \"fixture@forgelab.test\"", "email: \"\"", 1),
            ),
        ];
        for (want, yaml) in cases {
            let dir = base(&yaml);
            match load_spec(dir.path()) {
                Err(e) if e.to_string().contains(want) => {}
                other => panic!("want error containing {want:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unquoted_timestamp_and_offsets() {
        for (raw, want) in [
            ("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"),
            ("\"2026-01-01T02:00:00+02:00\"", "2026-01-01T00:00:00Z"),
            ("2026-01-01", "2026-01-01T00:00:00Z"),
        ] {
            let dir = base(&SPEC_YAML.replacen("\"2026-01-01T00:00:00Z\"", raw, 1));
            let spec = load_spec(dir.path()).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(env::format(&spec.git.timestamp), want, "{raw}");
        }
    }

    /// The tree alone says which directory is which: one that holds a file is a repository,
    /// one that holds only directories is a namespace.
    #[test]
    fn walks_namespaces() {
        let dir = base(&format!(
            "{SPEC_YAML}  platform/core/api: {{topics: [service]}}\n"
        ));
        add(&dir, "repos/platform/core/api/README.md", "# api\n");
        add(
            &dir,
            "repos/platform/core/api/src/main.go",
            "package main\n",
        );
        add(&dir, "repos/platform/tooling/run.sh", "#!/bin/sh\n");
        add(&dir, "repos/payments/api/.gitkeep", "");
        add(&dir, "repos/payments/.DS_Store", "junk");

        let spec = load_spec(dir.path()).unwrap();
        let names: Vec<&str> = spec.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bare",
                "legacy",
                "payments/api",
                "plain",
                "platform/core/api",
                "platform/tooling"
            ]
        );
        let api = spec
            .repos
            .iter()
            .find(|r| r.name == "platform/core/api")
            .unwrap();
        assert_eq!(api.dir, "repos/platform/core/api");
        assert_eq!(api.topics.len(), 1);
        assert_eq!(spec.namespaces(), ["payments", "platform", "platform/core"]);

        // A file makes it a repository, so the override below it no longer names one.
        add(&dir, "repos/platform/core/NOTES.md", "stray\n");
        let err = load_spec(dir.path()).unwrap_err().to_string();
        assert!(err.contains("no such directory"), "{err}");
    }

    #[test]
    fn rejects_trees() {
        let cases: &[(&str, &[&str])] = &[
            ("no repository is under it", &["repos/hollow/.hidden/x"]),
            (
                "both \"a-b-c\"",
                &["repos/a-b/c/README.md", "repos/a/b-c/README.md"],
            ),
            (
                "both \"x-y-z\"",
                &["repos/X-y/z/README.md", "repos/x/y-z/README.md"],
            ),
            ("not a valid", &["repos/team one/api/README.md"]),
        ];
        for (want, extra) in cases {
            let dir = base(SPEC_YAML);
            for p in *extra {
                add(&dir, p, "x\n");
            }
            match load_spec(dir.path()) {
                Err(e) if e.to_string().contains(want) => {}
                other => panic!("want error containing {want:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn refnames() {
        for ok in ["v1", "release/1.2", "feature-x", "a.b"] {
            assert!(check_refname(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "-x", "a..b", "a b", "x.lock", ".hidden", "a//b", "/a", "a/", "a~b", "@", "a@{b",
        ] {
            assert!(check_refname(bad).is_err(), "{bad}");
        }
    }
}
