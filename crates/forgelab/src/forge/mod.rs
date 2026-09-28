//! The small surface forgelab needs from a forge. Implementations are hand-rolled HTTP
//! clients, deliberately not a consumer's SDK or adapter: a harness that verified a sandbox
//! with the client under test would share its bugs.

pub mod classify;
pub mod error;
pub mod http;
pub mod keyed;

pub mod azuredevops;
pub mod forgejo;
pub mod github;
pub mod gitlab;

use std::time::Duration;

use async_trait::async_trait;

pub use error::{Class, ForgeError, TransportError};
pub use http::{
    HttpClient, RequestOpts, ReqwestTransport, RetryPolicy, ScriptedTransport, Transport, WriteLane,
};

pub use crate::seed::{GitAuth, GitRemote};

/// A repository as the forge reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    pub name: String,
    pub default_branch: String,
    /// "private" | "public"
    pub visibility: String,
    pub archived: bool,
    /// The forge holds no commits for it.
    pub empty: bool,
    pub topics: Vec<String>,
}

/// A branch or a tag and the commit it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub sha: String,
}

/// An open pull or merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub number: i64,
    pub title: String,
}

/// How many leading segments of a namespace become something of their own -- a subgroup, a
/// project -- that `create` makes and `delete_namespace` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceDepth {
    /// A namespace is only a prefix of the name.
    None,
    /// The first `n` segments.
    Depth(usize),
    /// All of them.
    Any,
}

impl NamespaceDepth {
    /// Whether a namespace with this many `/` separators is a thing of its own on the forge.
    pub fn holds(self, ns: &str) -> bool {
        match self {
            NamespaceDepth::None => false,
            NamespaceDepth::Any => true,
            NamespaceDepth::Depth(n) => ns.matches('/').count() < n,
        }
    }
}

/// What a forge can express. forgelab skips -- out loud, in the plan -- what a forge cannot
/// hold, rather than demand a different fleet for it: one fleet, one lock, every forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Repositories carry topics. Without them there is nowhere to put the marker either, so
    /// the "never adopt a repository forgelab did not create" guard is off: there, a
    /// repository with a declared name is forgelab's, and only the sandbox's own
    /// configuration and the reach of its token keep it in the right place.
    pub topics: bool,
    /// Visibility is set per repository (not per project).
    pub visibility: bool,
    /// An archived repository cannot be read at all -- not its refs, not its requests.
    /// forgelab can then only check the flag itself.
    pub archived_unreadable: bool,
    pub namespace_depth: NamespaceDepth,
}

/// The description of every namespace `create` makes. Groups and projects carry no topics,
/// so this is their marker, and it says what the topic says of a repository: work in it
/// freely, and expect forgelab to remove it, with all it holds, and seed it again.
pub const NAMESPACE_MARKER: &str = "forgelab-managed";

/// A partial update: `None` fields are left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub default_branch: Option<String>,
    pub visibility: Option<String>,
    pub archived: Option<bool>,
}

/// What `delete_namespace` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// Gone, with all it held.
    Removed,
    /// Left alone, and why.
    Kept(String),
    /// There was no such namespace.
    Absent,
}

/// How a forge likes to be driven: what the sandbox uses unless told otherwise.
#[derive(Debug, Clone, Copy)]
pub struct ForgePolicy {
    /// Repositories worked on at once.
    pub default_concurrency: usize,
    /// How long to wait for a namespace the forge deletes in the background to be gone.
    pub namespace_gone_timeout: Duration,
}

/// A repository's name on a forge that cannot hold all of its namespaces: what it cannot
/// hold is joined with "-". The fleet refuses two names that join to the same one.
pub fn flat_name(name: &str) -> String {
    name.replace('/', "-")
}

/// A `Forge` is bound to one organisation. Every method addresses a repository by name
/// inside it. There is deliberately no list: forgelab looks declared repositories up by name
/// and never enumerates the organisation, so what it did not declare it cannot see.
///
/// A name is the repository's path in the fleet, "platform/core/api": the namespaces it sits
/// in, then the repository. Each forge lands it where it can -- subgroups on GitLab, a
/// project and a flat name on Azure DevOps, a flat name elsewhere -- and creates the
/// namespaces it needs in `create`.
#[async_trait]
pub trait Forge: Send + Sync {
    /// The forge's name, for messages: "GitHub".
    fn name(&self) -> &'static str;

    fn caps(&self) -> Caps;

    fn policy(&self) -> ForgePolicy;

    /// Creates the organisation where a forge lets a token do that, and otherwise checks it
    /// exists.
    async fn ensure_org(&self) -> Result<(), ForgeError>;

    /// `None` for a missing repository AND for one that answers under a different name
    /// (forges redirect the old name of a renamed repository).
    async fn get(&self, name: &str) -> Result<Option<Repo>, ForgeError>;

    /// Must not return success with the topics unset: they carry the marker that tells
    /// forgelab the repository is its own, and an unmarked repository is one that a re-run of
    /// apply will refuse to touch. A forge that cannot set them in the same call sets them
    /// next, and deletes what it just created if that fails.
    async fn create(
        &self,
        name: &str,
        visibility: &str,
        default_branch: &str,
        topics: &[String],
    ) -> Result<(), ForgeError>;

    /// Removes the repository. One that is already gone is not an error: a destroy that is
    /// run again, or that raced a cache, must still finish.
    async fn delete(&self, name: &str) -> Result<(), ForgeError>;

    /// Removes a namespace that carries the `NAMESPACE_MARKER`, with all it holds; otherwise
    /// says why it was left alone. A namespace that does not exist is `Absent`. The namespace
    /// "" is where repositories without one live: removable where forgelab makes it (an Azure
    /// DevOps default project), and otherwise the sandbox itself, which is never touched.
    async fn delete_namespace(&self, ns: &str) -> Result<Removal, ForgeError>;

    async fn update_settings(&self, name: &str, s: Settings) -> Result<(), ForgeError>;

    async fn set_topics(&self, name: &str, topics: &[String]) -> Result<(), ForgeError>;

    /// The forge's own listing. It is only used to wait until the forge has caught up with a
    /// push, which is what a consumer reading the API will see; the comparison against the
    /// lock reads refs from git instead (`seed::ls_remote`).
    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError>;

    async fn delete_branch(&self, name: &str, branch: &str) -> Result<(), ForgeError>;

    async fn delete_tag(&self, name: &str, tag: &str) -> Result<(), ForgeError>;

    /// Makes sure `branch` can be force-pushed, lifting whatever the forge or a test put in
    /// the way. forgelab calls it immediately before every force-push rather than trusting
    /// that an earlier step already did: a branch can be protected at any time, by the forge
    /// itself (GitLab protects a default branch on first push, possibly a moment after the
    /// push returns), by an interrupted apply, or by the test that just ran.
    async fn allow_force_push(&self, name: &str, branch: &str) -> Result<(), ForgeError>;

    async fn open_requests(&self, name: &str) -> Result<Vec<Request>, ForgeError>;

    async fn close_request(&self, name: &str, number: i64) -> Result<(), ForgeError>;

    /// An authenticated HTTP remote for git.
    fn git_remote(&self, name: &str) -> Result<GitRemote, ForgeError>;
}

/// Reads a JSON `null` as the type's default. `#[serde(default)]` covers a missing field
/// only, and forges send `null` where a string or a list is empty: GitLab for a group being
/// deleted, which is what stopped a real destroy of the scale fleet. Every defaulted string and
/// list field in the clients goes through this.
pub(crate) fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    use serde::Deserialize as _;
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Splits "platform/core/api" into its namespace and leaf: ("platform/core", "api"). A name
/// without a namespace has "" for one.
pub fn split_namespace(name: &str) -> (&str, &str) {
    match name.rfind('/') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => ("", name),
    }
}

/// Percent-encodes one path segment the way Go's `url.PathEscape` does.
pub fn path_escape(s: &str) -> String {
    const KEEP: &[u8] = b"-_.~!$&'()*+,;=:@";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || KEEP.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a query value the way Go's `url.QueryEscape` does.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Escapes each path segment but keeps the slashes a ref name may contain.
pub fn escape_ref(r: &str) -> String {
    r.split('/').map(path_escape).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(flat_name("platform/core/api"), "platform-core-api");
        assert_eq!(
            split_namespace("platform/core/api"),
            ("platform/core", "api")
        );
        assert_eq!(split_namespace("api"), ("", "api"));
        assert_eq!(
            path_escape("acme-sandbox/services"),
            "acme-sandbox%2Fservices"
        );
        assert_eq!(escape_ref("feature/a b"), "feature/a%20b");
        assert!(NamespaceDepth::Depth(1).holds("platform"));
        assert!(!NamespaceDepth::Depth(1).holds("platform/core"));
        assert!(NamespaceDepth::Any.holds("a/b/c"));
        assert!(!NamespaceDepth::None.holds("a"));
    }
}
