//! `Forge` against the GitLab REST API (gitlab.com and self-managed). The "org" is a group,
//! addressed by its full path, which may be nested.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::error::ForgeError;
use super::http::{HttpClient, Pauses, RequestOpts, Transport};
use super::keyed::KeyedOnce;
use super::{
    Caps, Forge, ForgePolicy, GitAuth, GitRemote, NAMESPACE_MARKER, NamespaceDepth, Ref, Removal,
    Repo, Request, Settings, classify, path_escape, query_escape, split_namespace,
};
use crate::util::go_duration;

/// Used when a sandbox names no base_url.
pub const DEFAULT_BASE_URL: &str = "https://gitlab.com";

/// GitLab's access level 40. It is who may push and merge under the rule `allow_force_push`
/// installs: the same audience the group's own default names, so the rule changes only
/// whether a force-push is allowed.
const MAINTAINER_ACCESS: u32 = 40;

/// How often a namespace on its way out is looked at.
const GONE_POLL: Duration = Duration::from_secs(2);

/// Talks to one group, and to the subgroups the fleet's namespaces map to.
/// Projects named in one GraphQL query: GitLab refuses more than fifty full paths.
const GRAPHQL_BATCH: usize = 50;

pub struct Client {
    base_url: String,
    group: String,
    token: SecretString,
    http: HttpClient,
    /// Remembers the group and its subgroups by namespace; "" is the group itself. Making a
    /// subgroup runs once per namespace however many repositories need it at the same time,
    /// and workers on other namespaces do not wait for it.
    groups: KeyedOnce<String, GroupInfo>,
    cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GroupInfo {
    #[serde(default)]
    id: i64,
    #[serde(default, deserialize_with = "super::null_default")]
    full_path: String,
    #[serde(default, deserialize_with = "super::null_default")]
    visibility: String,
    #[serde(default, deserialize_with = "super::null_default")]
    description: String,
    #[serde(default, rename = "marked_for_deletion_on")]
    deleting: Option<String>,
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// GitLab's "Project could not be updated!": a 422, or a 400 carrying that message.
fn is_busy(e: &ForgeError) -> bool {
    match e {
        ForgeError::Status { status: 422, .. } => true,
        ForgeError::Status {
            status: 400, body, ..
        } => body.contains("could not be updated"),
        _ => false,
    }
}

impl Client {
    /// Reports every rate-limit pause to `pauses`, which the run's progress line reads.
    pub fn with_pauses(mut self, pauses: Arc<Pauses>) -> Client {
        self.http = self.http.with_pauses(pauses.clone());
        self
    }

    /// `base_url` is the web address, e.g. https://gitlab.com.
    pub fn new(
        base_url: &str,
        group: &str,
        token: SecretString,
        transport: Arc<dyn Transport>,
        cancel: CancellationToken,
    ) -> Result<Client, ForgeError> {
        let base_url = if base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            base_url
        };
        let ok = url::Url::parse(base_url)
            .ok()
            .filter(|u| !u.scheme().is_empty() && u.host_str().is_some())
            .is_some();
        if !ok {
            return Err(ForgeError::msg(format!(
                "base URL {base_url:?} has no scheme or host"
            )));
        }
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(token.expose_secret())
            .unwrap_or_else(|_| HeaderValue::from_static(""));
        auth.set_sensitive(true);
        headers.insert("private-token", auth);
        let http = HttpClient::new("GitLab", transport, classify::gitlab, headers)
            .with_cancel(cancel.clone());
        Ok(Client {
            base_url: base_url.trim_end_matches('/').to_string(),
            group: group.trim_matches('/').to_string(),
            token,
            http,
            groups: KeyedOnce::default(),
            cancel,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v4{path}", self.base_url)
    }

    /// The API path of one project, addressed by its URL-encoded full path. Paths carry
    /// %2F-encoded project paths, so they travel as one segment.
    fn project(&self, name: &str) -> String {
        format!(
            "/projects/{}",
            path_escape(&format!("{}/{name}", self.group))
        )
    }

    /// Looks up to `GRAPHQL_BATCH` projects up by full path in one query. A project that does
    /// not exist, answers under another path (renamed) or is scheduled for deletion is simply
    /// not in the answer, which is what `get` says of each.
    ///
    /// The head it reports is the last commit to touch the default branch's tree, which is the
    /// branch's own head unless that is an empty commit. It is only ever a shortcut for the
    /// readiness wait; the comparison against the lock reads refs from git.
    async fn lookup(&self, names: &[String]) -> Result<Vec<Option<Repo>>, ForgeError> {
        #[derive(Deserialize)]
        struct Commit {
            #[serde(default, deserialize_with = "super::null_default")]
            sha: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Tree {
            #[serde(default)]
            last_commit: Option<Commit>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Repository {
            #[serde(default)]
            empty: bool,
            #[serde(default, deserialize_with = "super::null_default")]
            root_ref: String,
            #[serde(default)]
            tree: Option<Tree>,
        }
        #[derive(Deserialize)]
        struct Mr {
            iid: String,
            #[serde(default, deserialize_with = "super::null_default")]
            title: String,
        }
        #[derive(Deserialize)]
        struct Mrs {
            count: usize,
            #[serde(default, deserialize_with = "super::null_default")]
            nodes: Vec<Mr>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            full_path: String,
            #[serde(default, deserialize_with = "super::null_default")]
            visibility: String,
            #[serde(default)]
            archived: bool,
            #[serde(default, deserialize_with = "super::null_default")]
            topics: Vec<String>,
            #[serde(default, deserialize_with = "super::null_default")]
            description: String,
            #[serde(default)]
            marked_for_deletion_on: Option<String>,
            #[serde(default)]
            repository: Option<Repository>,
            merge_requests: Mrs,
        }
        #[derive(Deserialize)]
        struct Projects {
            #[serde(default, deserialize_with = "super::null_default")]
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            projects: Option<Projects>,
        }
        #[derive(Deserialize)]
        struct GqlError {
            #[serde(default, deserialize_with = "super::null_default")]
            message: String,
        }
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            data: Option<Data>,
            #[serde(default, deserialize_with = "super::null_default")]
            errors: Vec<GqlError>,
        }

        const QUERY: &str = "query($p: [String!]) { projects(fullPaths: $p, first: 50) { nodes { \
            fullPath visibility archived topics description markedForDeletionOn \
            repository { empty rootRef tree { lastCommit { sha } } } \
            mergeRequests(state: opened, first: 100) { count nodes { iid title } } } } }";
        let paths: Vec<String> = names
            .iter()
            .map(|n| format!("{}/{n}", self.group))
            .collect();
        let body = serde_json::json!({ "query": QUERY, "variables": { "p": paths } });
        let (_, answer): (_, Option<Answer>) = self
            .http
            .json(
                Method::POST,
                &format!("{}/api/graphql", self.base_url),
                Some(&body),
                RequestOpts {
                    idempotent: Some(true), // a query writes nothing
                    ..Default::default()
                },
            )
            .await?;
        let answer = answer.ok_or_else(|| ForgeError::msg("graphql: empty answer"))?;
        if let Some(e) = answer.errors.first() {
            return Err(ForgeError::msg(format!("graphql: {}", e.message)));
        }
        let nodes = answer
            .data
            .and_then(|d| d.projects)
            .ok_or_else(|| ForgeError::msg("graphql: answer without projects"))?
            .nodes;
        let mut by_path: std::collections::HashMap<String, Node> = nodes
            .into_iter()
            .map(|n| (n.full_path.to_lowercase(), n))
            .collect();
        let mut out = Vec::with_capacity(names.len());
        for (name, path) in names.iter().zip(&paths) {
            let Some(n) = by_path.remove(&path.to_lowercase()) else {
                out.push(None);
                continue;
            };
            if n.marked_for_deletion_on.is_some() {
                out.push(None);
                continue;
            }
            let repo = n.repository.unwrap_or(Repository {
                empty: true,
                root_ref: String::new(),
                tree: None,
            });
            // GitLab's "empty" is updated after a push, not by it. `get` knows how to ask the
            // repository itself, and an empty project is rare enough to ask it one at a time.
            if repo.empty {
                out.push(self.get(name).await?);
                continue;
            }
            let mrs = n.merge_requests;
            let open_requests = if mrs.nodes.len() == mrs.count {
                let mut v = Vec::with_capacity(mrs.nodes.len());
                for m in mrs.nodes {
                    let number = m.iid.parse::<i64>().map_err(|e| {
                        ForgeError::msg(format!("graphql: merge request iid {:?}: {e}", m.iid))
                    })?;
                    v.push(Request {
                        number,
                        title: m.title,
                    });
                }
                Some(v)
            } else {
                None // more than one page of them: left to `open_requests`, which pages
            };
            out.push(Some(Repo {
                name: name.clone(),
                default_branch: repo.root_ref,
                visibility: n.visibility,
                archived: n.archived,
                empty: false,
                topics: n.topics,
                description: n.description,
                head: repo
                    .tree
                    .and_then(|t| t.last_commit)
                    .map(|c| c.sha)
                    .filter(|s| !s.is_empty()),
                open_requests,
            }));
        }
        Ok(out)
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Option<T>, ForgeError> {
        self.http
            .json(method, &self.url(path), body, RequestOpts::default())
            .await
            .map(|(_, v)| v)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(), ForgeError> {
        self.http
            .call(method, &self.url(path), body, RequestOpts::default())
            .await
            .map(drop)
    }

    /// Follows X-Next-Page until it is empty, which is the only authoritative signal: the
    /// total-count headers are omitted on large collections.
    async fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, ForgeError> {
        let sep = if path.contains('?') { "&" } else { "?" };
        let mut page = "1".to_string();
        let mut all = Vec::new();
        while !page.is_empty() {
            let (headers, items): (_, Option<Vec<T>>) = self
                .http
                .json(
                    Method::GET,
                    &self.url(&format!("{path}{sep}per_page=100&page={page}")),
                    None::<&serde_json::Value>,
                    RequestOpts::default(),
                )
                .await?;
            all.extend(items.unwrap_or_default());
            page = headers
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .trim()
                .to_string();
        }
        Ok(all)
    }

    fn full_path(&self, ns: &str) -> String {
        if ns.is_empty() {
            self.group.clone()
        } else {
            format!("{}/{ns}", self.group)
        }
    }

    /// One GET of a group by full path; `None` on 404.
    async fn fetch_group(&self, full: &str) -> Result<Option<GroupInfo>, ForgeError> {
        match self
            .json::<GroupInfo>(
                Method::GET,
                &format!("/groups/{}?with_projects=false", path_escape(full)),
                None,
            )
            .await
        {
            Ok(g) => Ok(Some(g.unwrap_or_default())),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Resolves a namespace to its group. The sandbox's own group must exist. A missing
    /// subgroup is reported as such, or with `create` is made, parents first, as visible as
    /// its parent: a subgroup cannot be more, and a public fixture needs every group above it
    /// to be.
    ///
    /// A subgroup GitLab has only scheduled for deletion still answers, and keeps its path:
    /// nothing can be put in it. Where the Go client refused at once, this waits for it to go
    /// -- deletion is asynchronous, and a destroy followed by an apply is the normal way to a
    /// clean slate -- and makes the subgroup again once the path is free.
    fn group_of<'a>(
        &'a self,
        ns: &'a str,
        create: bool,
    ) -> BoxFut<'a, Result<Option<GroupInfo>, ForgeError>> {
        Box::pin(async move {
            let key = ns.to_string();
            if let Some(g) = self.groups.get(&key) {
                return Ok(Some(g));
            }
            let full = self.full_path(ns);
            if !create {
                let g = self.fetch_group(&full).await?;
                match g {
                    None if ns.is_empty() => {
                        return Err(ForgeError::msg(format!(
                            "group {:?} does not exist, or the token cannot see it",
                            self.group
                        )));
                    }
                    None => return Ok(None),
                    Some(g) => {
                        if g.deleting.is_none() {
                            self.groups.set(&key, g.clone());
                        }
                        return Ok(Some(g));
                    }
                }
            }
            let g = self
                .groups
                .get_or_try_init(&key, async {
                    match self.fetch_group(&full).await? {
                        Some(g) if g.deleting.is_none() => Ok(g),
                        Some(g) => {
                            self.wait_gone(g.id, &g.full_path).await?;
                            self.create_group(ns).await
                        }
                        None if ns.is_empty() => Err(ForgeError::msg(format!(
                            "group {:?} does not exist, or the token cannot see it",
                            self.group
                        ))),
                        None => self.create_group(ns).await,
                    }
                })
                .await?;
            Ok(Some(g))
        })
    }

    /// Makes one subgroup under its (possibly freshly made) parent.
    async fn create_group(&self, ns: &str) -> Result<GroupInfo, ForgeError> {
        let (parent_ns, leaf) = split_namespace(ns);
        let parent = self.group_of(parent_ns, true).await?.ok_or_else(|| {
            ForgeError::msg(format!(
                "group {:?} does not exist",
                self.full_path(parent_ns)
            ))
        })?;
        let body = serde_json::json!({
            "name": leaf, "path": leaf, "parent_id": parent.id, "visibility": parent.visibility,
            "description": NAMESPACE_MARKER,
        });
        let full = self.full_path(ns);
        match self
            .http
            .json::<GroupInfo>(
                Method::POST,
                &self.url("/groups"),
                Some(&body),
                RequestOpts {
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await
        {
            Ok((_, g)) => Ok(g.unwrap_or_default()),
            Err(e) => Err(ForgeError::msg(format!("create subgroup {full}: {e}"))),
        }
    }

    /// Waits for a group GitLab is deleting in the background to be gone, asking for the
    /// permanent removal on the way, and says so if it is still there at the end.
    async fn wait_gone(&self, id: i64, full_path: &str) -> Result<(), ForgeError> {
        match self.poll_gone(id, true).await? {
            Removal::Removed => Ok(()),
            _ => Err(ForgeError::msg(format!(
                "subgroup {full_path} is pending deletion: remove it for good on GitLab, or wait for it to go"
            ))),
        }
    }

    /// Polls a group by id until it answers 404, for up to the policy's bound. A group that
    /// turns out to be merely scheduled is removed for good with a second DELETE naming the
    /// path the scheduling renamed it to; a refusal of that is logged, since the wait itself
    /// may still succeed on an instance that removes on its own clock.
    async fn poll_gone(&self, id: i64, purge: bool) -> Result<Removal, ForgeError> {
        let by_id = format!("/groups/{id}");
        let bound = self.policy().namespace_gone_timeout;
        let deadline = tokio::time::Instant::now() + bound;
        let mut purged = !purge;
        loop {
            let after = match self
                .json::<GroupInfo>(Method::GET, &format!("{by_id}?with_projects=false"), None)
                .await
            {
                Ok(g) => g.unwrap_or_default(),
                Err(e) if e.is_not_found() => return Ok(Removal::Removed),
                Err(e) => return Err(e),
            };
            if after.deleting.is_some() && !purged {
                purged = true;
                if let Err(e) = self
                    .call(
                        Method::DELETE,
                        &format!(
                            "{by_id}?permanently_remove=true&full_path={}",
                            query_escape(&after.full_path)
                        ),
                        None,
                    )
                    .await
                {
                    tracing::debug!(
                        "GitLab: permanent removal of group {} refused: {e}",
                        after.full_path
                    );
                }
            }
            if tokio::time::Instant::now() + GONE_POLL > deadline {
                return Ok(Removal::Kept(format!(
                    "still being deleted by GitLab after {}",
                    go_duration(bound)
                )));
            }
            tokio::select! {
                _ = tokio::time::sleep(GONE_POLL) => {}
                _ = self.cancel.cancelled() => return Err(ForgeError::Cancelled),
            }
        }
    }

    /// Edits the project. GitLab answers a burst of updates -- a fleet being applied, eight
    /// projects at a time -- with a bare 422 "Project could not be updated!" that succeeds
    /// when simply tried again, so it is, a few times.
    async fn update(&self, name: &str, fields: serde_json::Value) -> Result<(), ForgeError> {
        let path = self.project(name);
        self.busy_retried(|| self.call(Method::PUT, &path, Some(&fields)))
            .await
            .map(drop)
    }

    /// Tries a request again, a few times, when GitLab answers "Project could not be
    /// updated!": a bare refusal that a burst of changes provokes and a second try cures. It
    /// comes as a 422 on an update, and as a 400 on a delete -- one of a hundred and eight
    /// deletions, four at a time, in a real destroy of the scale fleet.
    async fn busy_retried<T, F, Fut>(&self, send: F) -> Result<T, ForgeError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, ForgeError>>,
    {
        let mut last = None;
        for attempt in 1..=4u64 {
            match send().await {
                Err(e) if is_busy(&e) => {
                    last = Some(e);
                    if attempt < 4 {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(attempt)) => {}
                            _ = self.cancel.cancelled() => return Err(ForgeError::Cancelled),
                        }
                    }
                }
                other => return other,
            }
        }
        Err(last.expect("four attempts leave an error"))
    }

    async fn refs(&self, name: &str, kind: &str) -> Result<Vec<Ref>, ForgeError> {
        #[derive(Deserialize)]
        struct Commit {
            id: String,
        }
        #[derive(Deserialize)]
        struct RawRef {
            name: String,
            commit: Commit,
        }
        let items: Vec<RawRef> = match self
            .list(&format!("{}/repository/{kind}", self.project(name)))
            .await
        {
            Ok(items) => items,
            // A project with no commits may have no repository to list.
            Err(e) if e.is_not_found() => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(items
            .into_iter()
            .map(|r| Ref {
                name: r.name,
                sha: r.commit.id,
            })
            .collect())
    }
}

#[derive(Deserialize, Default)]
struct ProjectRead {
    #[serde(default)]
    id: i64,
    #[serde(
        default,
        rename = "path_with_namespace",
        deserialize_with = "super::null_default"
    )]
    full_path: String,
    #[serde(default, rename = "marked_for_deletion_on")]
    deleting: Option<String>,
}

#[async_trait]
impl Forge for Client {
    fn name(&self) -> &'static str {
        "GitLab"
    }

    /// Everything forgelab declares has a home here.
    fn caps(&self) -> Caps {
        Caps {
            topics: true,
            marker: true,
            visibility: true,
            archived_unreadable: false,
            namespace_depth: NamespaceDepth::Any,
        }
    }

    fn policy(&self) -> ForgePolicy {
        ForgePolicy {
            default_concurrency: 8,
            namespace_gone_timeout: Duration::from_secs(300),
        }
    }

    /// Only checks: on gitlab.com a top-level group cannot be created through the API.
    async fn ensure_org(&self) -> Result<(), ForgeError> {
        self.group_of("", false).await.map(drop)
    }

    fn batch_size(&self) -> usize {
        GRAPHQL_BATCH
    }

    async fn get_many(&self, names: &[String]) -> Result<Vec<Option<Repo>>, ForgeError> {
        let mut out = Vec::with_capacity(names.len());
        for chunk in names.chunks(GRAPHQL_BATCH) {
            out.extend(self.lookup(chunk).await?);
        }
        Ok(out)
    }

    async fn get(&self, name: &str) -> Result<Option<Repo>, ForgeError> {
        #[derive(Deserialize, Default)]
        struct Raw {
            #[serde(
                default,
                rename = "path_with_namespace",
                deserialize_with = "super::null_default"
            )]
            full_path: String,
            #[serde(default, deserialize_with = "super::null_default")]
            default_branch: String,
            #[serde(default, deserialize_with = "super::null_default")]
            visibility: String,
            #[serde(default)]
            archived: bool,
            #[serde(default)]
            empty_repo: bool,
            #[serde(default, deserialize_with = "super::null_default")]
            topics: Vec<String>,
            #[serde(default, rename = "marked_for_deletion_on")]
            deleting: Option<String>,
            #[serde(default, deserialize_with = "super::null_default")]
            description: String,
        }
        let raw: Raw = match self.json(Method::GET, &self.project(name), None).await {
            Ok(Some(raw)) => raw,
            Ok(None) => return Ok(None),
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        // A project answering under another path was renamed; one scheduled for deletion is
        // already gone as far as the fleet is concerned.
        if !raw
            .full_path
            .eq_ignore_ascii_case(&format!("{}/{name}", self.group))
            || raw.deleting.is_some()
        {
            return Ok(None);
        }
        let mut r = Repo {
            name: name.to_string(),
            default_branch: raw.default_branch,
            visibility: raw.visibility,
            archived: raw.archived,
            empty: raw.empty_repo,
            topics: raw.topics,
            description: raw.description,
            ..Repo::default()
        };
        // empty_repo is updated after a push, not by it. When it claims "empty", ask the
        // repository itself, so that a project seeded a moment ago is not reported as unseeded.
        if r.empty {
            r.empty = self.branches(name).await?.is_empty();
            if r.empty {
                // No commits -- or a project deleted a moment ago and still answered from a
                // cache. Only the project itself can say; a 404 now means it is gone.
                if let Err(e) = self.call(Method::GET, &self.project(name), None).await
                    && e.is_status(&[404])
                {
                    return Ok(None);
                }
            }
        }
        Ok(Some(r))
    }

    /// Sets the topics in the same call, so a project never exists unmarked. It ignores
    /// `default_branch`: the first branch pushed becomes the default, and `update_settings`
    /// pins it.
    async fn create(
        &self,
        name: &str,
        visibility: &str,
        _default_branch: &str,
        topics: &[String],
        marker: &str,
    ) -> Result<(), ForgeError> {
        let (ns, leaf) = split_namespace(name);
        let g = self.group_of(ns, true).await?.ok_or_else(|| {
            ForgeError::msg(format!("group {:?} does not exist", self.full_path(ns)))
        })?;
        if g.deleting.is_some() {
            return Err(ForgeError::msg(format!(
                "subgroup {} is pending deletion: remove it for good on GitLab, or wait for it to go",
                g.full_path
            )));
        }
        let body = serde_json::json!({
            "name": leaf,
            "path": leaf,
            "namespace_id": g.id,
            "visibility": visibility,
            "topics": topics,
            "description": marker,
            "initialize_with_readme": false,
        });
        self.http
            .call(
                Method::POST,
                &self.url("/projects"),
                Some(&body),
                RequestOpts {
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await
            .map(drop)
    }

    /// Removes the project for good. On gitlab.com a first DELETE only schedules it: the
    /// project is renamed, which frees its path at once, and lingers for days. A second DELETE
    /// naming that new path removes it now, so that a destroy does not leave a dozen ghosts
    /// behind each time.
    async fn delete(&self, name: &str) -> Result<(), ForgeError> {
        let before: ProjectRead = match self.json(Method::GET, &self.project(name), None).await {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => return Err(e),
        };
        let by_id = format!("/projects/{}", before.id);
        match self
            .busy_retried(|| self.call(Method::DELETE, &by_id, None))
            .await
        {
            Err(e) if e.is_not_found() => return Ok(()),
            other => other?,
        }

        // What the first delete did has to be read back: gitlab.com only schedules the removal,
        // while an instance configured without the delay performs it outright. Only a 404 says
        // the project is really gone. Any other error leaves the question unanswered, and
        // answering it "gone" is what let a project sit in deletion_scheduled while destroy
        // reported it deleted -- a whole delete silently downgraded to a rename.
        let after: ProjectRead = match self.json(Method::GET, &by_id, None).await {
            Ok(p) => p.unwrap_or_default(),
            Err(e) if e.is_not_found() => return Ok(()), // removed outright
            Err(e) => {
                return Err(ForgeError::msg(format!(
                    "{name}: deleted, then could not be read back: {e}"
                )));
            }
        };
        if after.deleting.is_none() {
            return Ok(()); // not scheduled, so the first delete was the whole of it
        }

        // Scheduled. The second call is what makes it permanent, and it has to name the path
        // the scheduling renamed the project to, not the one it was deleted by.
        self.call(
            Method::DELETE,
            &format!(
                "{by_id}?permanently_remove=true&full_path={}",
                query_escape(&after.full_path)
            ),
            None,
        )
        .await
        .map_err(|e| {
            ForgeError::msg(format!(
                "{}: scheduled for deletion but not removed: {e}",
                after.full_path
            ))
        })
    }

    /// Removes a subgroup forgelab made, with all it holds, and for good, the way `delete`
    /// does a project: a first DELETE may only schedule it, a second naming its new path
    /// removes it now. The removal is waited for, since GitLab deletes in the background and a
    /// re-seed needs the path; one that is still there after the bound is reported as kept,
    /// and the caller says so.
    async fn delete_namespace(&self, ns: &str) -> Result<Removal, ForgeError> {
        if ns.is_empty() {
            return Ok(Removal::Absent); // the sandbox's own group
        }
        let Some(g) = self.group_of(ns, false).await? else {
            return Ok(Removal::Absent);
        };
        if !g.description.starts_with(NAMESPACE_MARKER) {
            return Ok(Removal::Kept("not created by forgelab".to_string()));
        }
        let prefix = format!("{ns}/");
        self.groups
            .invalidate_where(|k| k == ns || k.starts_with(&prefix));
        // One that an earlier destroy only managed to schedule goes straight to the second DELETE.
        if g.deleting.is_none() {
            match self
                .call(Method::DELETE, &format!("/groups/{}", g.id), None)
                .await
            {
                Err(e) if e.is_not_found() => return Ok(Removal::Removed),
                other => other?,
            }
        }
        self.poll_gone(g.id, true).await
    }

    async fn update_settings(&self, name: &str, s: Settings) -> Result<(), ForgeError> {
        // Archiving has endpoints of its own, and an archived project rejects everything else,
        // so unarchive comes first and archive last.
        if s.archived == Some(false) {
            self.call(
                Method::POST,
                &format!("{}/unarchive", self.project(name)),
                None,
            )
            .await?;
        }
        let mut fields = serde_json::Map::new();
        if let Some(b) = &s.default_branch {
            fields.insert("default_branch".into(), b.clone().into());
        }
        if let Some(v) = &s.visibility {
            fields.insert("visibility".into(), v.clone().into());
        }
        if !fields.is_empty() {
            self.update(name, serde_json::Value::Object(fields)).await?;
        }
        if let Some(b) = &s.default_branch {
            self.allow_force_push(name, b).await?;
        }
        if s.archived == Some(true) {
            self.call(
                Method::POST,
                &format!("{}/archive", self.project(name)),
                None,
            )
            .await?;
        }
        Ok(())
    }

    async fn set_topics(&self, name: &str, topics: &[String]) -> Result<(), ForgeError> {
        self.update(name, serde_json::json!({"topics": topics}))
            .await
    }

    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError> {
        self.refs(name, "branches").await
    }

    async fn delete_branch(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!(
                "{}/repository/branches/{}",
                self.project(name),
                path_escape(branch)
            ),
            None,
        )
        .await
    }

    async fn delete_tag(&self, name: &str, tag: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!(
                "{}/repository/tags/{}",
                self.project(name),
                path_escape(tag)
            ),
            None,
        )
        .await
    }

    /// Makes sure `branch` can be force-pushed. GitLab protects a default branch the moment
    /// it is first pushed, and protected means "no force-push": without this, reset fails
    /// with "You are not allowed to force push code to a protected branch".
    ///
    /// What it installs is an explicit project rule that permits a force-push. That outranks
    /// the group's default_branch_protection_defaults, which is the case removing a rule
    /// cannot reach: inherited protection never materialises as a project rule, so the branch
    /// reports protected while /protected_branches answers an empty list. It also leaves the
    /// branch protected in every other respect, which is closer to a real repository than
    /// unprotecting it.
    ///
    /// Both calls are here because neither alone converges. GitLab protects a default branch
    /// a moment *after* the first push returns, so a rule can appear between any two
    /// requests: create first and the answer may be 409, in which case the rule to amend is
    /// the one that just arrived. Deleting and creating instead would only race the forge
    /// again -- which it did, on eight projects out of a hundred and eight.
    async fn allow_force_push(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        let body = serde_json::json!({
            "name": branch,
            "allow_force_push": true,
            "push_access_level": MAINTAINER_ACCESS,
            "merge_access_level": MAINTAINER_ACCESS,
        });
        match self
            .call(
                Method::POST,
                &format!("{}/protected_branches", self.project(name)),
                Some(&body),
            )
            .await
        {
            Err(e) if e.is_status(&[409]) => {}
            other => return other,
        }
        self.call(
            Method::PATCH,
            &format!(
                "{}/protected_branches/{}",
                self.project(name),
                path_escape(branch)
            ),
            Some(&serde_json::json!({"allow_force_push": true})),
        )
        .await
    }

    async fn open_requests(&self, name: &str) -> Result<Vec<Request>, ForgeError> {
        #[derive(Deserialize)]
        struct Mr {
            iid: i64,
            #[serde(default, deserialize_with = "super::null_default")]
            title: String,
        }
        let items: Vec<Mr> = self
            .list(&format!(
                "{}/merge_requests?state=opened",
                self.project(name)
            ))
            .await?;
        Ok(items
            .into_iter()
            .map(|m| Request {
                number: m.iid,
                title: m.title,
            })
            .collect())
    }

    /// Closes rather than deletes: closing is enough for verify, needs a lower role, and
    /// behaves like the other forges. The IID is never reused either way.
    async fn close_request(&self, name: &str, number: i64) -> Result<(), ForgeError> {
        self.call(
            Method::PUT,
            &format!("{}/merge_requests/{number}", self.project(name)),
            Some(&serde_json::json!({"state_event": "close"})),
        )
        .await
    }

    /// GitLab accepts a personal access token as the password; oauth2 is the conventional
    /// username. The token travels as a header, never in the URL.
    fn git_remote(&self, name: &str) -> Result<GitRemote, ForgeError> {
        let mut u = url::Url::parse(&self.base_url)
            .map_err(|e| ForgeError::msg(format!("parse base URL {:?}: {e}", self.base_url)))?;
        let path = format!(
            "{}/{}/{name}.git",
            u.path().trim_end_matches('/'),
            self.group
        );
        u.set_path(&path);
        Ok(GitRemote {
            url: u,
            auth: Some(GitAuth {
                username: "oauth2".into(),
                secret: self.token.clone(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::http::ScriptedTransport;
    use bytes::Bytes;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Handler = Box<dyn Fn(&http::Request<Bytes>) -> http::Response<Bytes> + Send + Sync>;

    /// A fake API for the nested group acme-sandbox/services.
    fn serve(h: Handler) -> (Client, Arc<ScriptedTransport>) {
        let t = ScriptedTransport::new(move |r| h(r));
        let c = Client::new(
            "http://gl.test",
            "acme-sandbox/services",
            "s3cret".to_string().into(),
            t.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        (c, t)
    }

    fn reply(status: u16, body: &str) -> http::Response<Bytes> {
        ScriptedTransport::reply(status, body)
    }

    fn body_json(r: &http::Request<Bytes>) -> serde_json::Value {
        serde_json::from_slice(r.body()).unwrap_or(serde_json::Value::Null)
    }

    const SVC: &str = "/api/v4/projects/acme-sandbox%2Fservices%2Fsvc";
    const GROUP: &str = "/api/v4/groups/acme-sandbox%2Fservices";

    #[tokio::test(start_paused = true)]
    async fn get() {
        let (c, t) = serve(Box::new(|r| {
            assert_eq!(
                r.headers().get("private-token").unwrap(),
                "s3cret",
                "no token on {}",
                r.uri().path()
            );
            match r.uri().path() {
                SVC => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/svc","default_branch":"main","visibility":"private","archived":true,"empty_repo":false,"topics":["a"]}"#,
                ),
                // just pushed: empty_repo has not caught up, the repository has
                "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh" => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/fresh","visibility":"public","empty_repo":true}"#,
                ),
                "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh/repository/branches" => {
                    reply(200, r#"[{"name":"main","commit":{"id":"abc"}}]"#)
                }
                "/api/v4/projects/acme-sandbox%2Fservices%2Fbare" => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/bare","visibility":"private","empty_repo":true}"#,
                ),
                "/api/v4/projects/acme-sandbox%2Fservices%2Fbare/repository/branches" => {
                    reply(200, "[]")
                }
                "/api/v4/projects/acme-sandbox%2Fservices%2Fold-name" => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/new-name"}"#,
                ),
                "/api/v4/projects/acme-sandbox%2Fservices%2Fdoomed" => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/doomed","marked_for_deletion_on":"2026-09-26"}"#,
                ),
                // a namespace is more of the same path; its leaf alone says nothing
                "/api/v4/projects/acme-sandbox%2Fservices%2Fcore%2Fapi" => reply(
                    200,
                    r#"{"path":"api","path_with_namespace":"acme-sandbox/services/core/api","visibility":"private"}"#,
                ),
                _ => reply(404, ""),
            }
        }));
        let r = c.get("core/api").await.unwrap().expect("core/api");
        assert_eq!(r.name, "core/api");
        t.reset();
        let r = c.get("svc").await.unwrap().expect("svc");
        assert!(
            r.visibility == "private" && r.archived && !r.empty && r.topics.len() == 1,
            "{r:?}"
        );
        assert_eq!(
            t.seen()[0],
            format!("GET {SVC}"),
            "the nested project path must travel as one %2F-encoded segment"
        );
        assert!(
            !c.get("fresh").await.unwrap().unwrap().empty,
            "a lagging empty_repo flag must not read as unseeded"
        );
        assert!(c.get("bare").await.unwrap().unwrap().empty);
        for name in ["missing", "old-name", "doomed"] {
            assert!(
                c.get(name).await.unwrap().is_none(),
                "{name} must read as missing"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn list_follows_next_page() {
        let (c, _) = serve(Box::new(|r| {
            if r.uri().query().unwrap_or("").ends_with("&page=1") {
                return http::Response::builder()
                    .status(200)
                    .header("X-Next-Page", "2")
                    .body(Bytes::from(
                        r#"[{"iid":1,"title":"a"},{"iid":2,"title":"b"}]"#,
                    ))
                    .unwrap();
            }
            reply(200, r#"[{"iid":3,"title":"c"}]"#)
        }));
        let reqs = c.open_requests("svc").await.unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[2].number, 3);
    }

    /// Order matters: an archived project rejects every other write, and the default branch
    /// is unprotected because reset has to force-push it.
    #[tokio::test(start_paused = true)]
    async fn update_settings_order() {
        let (c, t) = serve(Box::new(|r| {
            if r.uri().path().contains("/protected_branches/") {
                return reply(404, ""); // not protected is fine
            }
            reply(200, "")
        }));
        c.update_settings(
            "svc",
            Settings {
                default_branch: Some("release/1".into()),
                visibility: Some("private".into()),
                archived: Some(false),
            },
        )
        .await
        .unwrap();
        c.update_settings(
            "svc",
            Settings {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            t.seen(),
            [
                format!("POST {SVC}/unarchive"),
                format!("PUT {SVC}"),
                format!("POST {SVC}/protected_branches"),
                format!("POST {SVC}/archive")
            ]
        );
    }

    /// With no rule in the way, one create is the whole of it, and it permits a force-push.
    #[tokio::test(start_paused = true)]
    async fn allow_force_push() {
        let rule = Arc::new(Mutex::new(serde_json::Value::Null));
        let r2 = rule.clone();
        let (c, t) = serve(Box::new(move |r| {
            *r2.lock().unwrap() = body_json(r);
            reply(201, "")
        }));
        c.allow_force_push("svc", "release/1").await.unwrap();
        assert_eq!(t.seen(), [format!("POST {SVC}/protected_branches")]);
        let rule = rule.lock().unwrap();
        assert!(
            rule["name"] == "release/1" && rule["allow_force_push"] == true,
            "the rule does not permit force-push: {rule}"
        );
    }

    /// The regression. GitLab protects a default branch a moment *after* the first push
    /// returns, so the rule can arrive between any two requests and the create answers 409.
    /// Amending the one that turned up is what converges; deleting and creating again only
    /// races the forge a second time, which is how eight projects out of a hundred and eight
    /// ended up protected against the force-push that reset depends on -- while verify, which
    /// knows nothing of branch protection, went on reporting them green.
    #[tokio::test(start_paused = true)]
    async fn allow_force_push_amends_a_rule_that_arrives_first() {
        let patched = Arc::new(Mutex::new(serde_json::Value::Null));
        let p2 = patched.clone();
        let (c, t) = serve(Box::new(move |r| {
            if r.method() == Method::POST {
                return reply(
                    409,
                    r#"{"message":"Protected branch 'main' already exists"}"#,
                );
            }
            *p2.lock().unwrap() = body_json(r);
            reply(200, "")
        }));
        c.allow_force_push("svc", "main")
            .await
            .expect("a rule that already exists is not a failure");
        assert_eq!(
            t.seen(),
            [
                format!("POST {SVC}/protected_branches"),
                format!("PATCH {SVC}/protected_branches/main")
            ]
        );
        assert_eq!(
            patched.lock().unwrap()["allow_force_push"],
            true,
            "the existing rule was not amended to permit force-push"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn allow_force_push_refused() {
        let (c, _) = serve(Box::new(|_| reply(403, "")));
        c.allow_force_push("svc", "main")
            .await
            .expect_err("a refused unprotect must surface");
    }

    /// gitlab.com only schedules a deletion; the second call makes it real.
    #[tokio::test(start_paused = true)]
    async fn delete_is_permanent() {
        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let q2 = queries.clone();
        let (c, t) = serve(Box::new(move |r| {
            let mut q = q2.lock().unwrap();
            if r.method() == Method::DELETE {
                let full_path = r
                    .uri()
                    .query()
                    .unwrap_or("")
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("full_path="))
                    .unwrap_or("")
                    .to_string();
                q.push(full_path);
                return reply(202, "");
            }
            if q.is_empty() {
                reply(
                    200,
                    r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc"}"#,
                )
            } else {
                reply(
                    200,
                    r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc-deleted-42","marked_for_deletion_on":"2026-09-26"}"#,
                )
            }
        }));
        c.delete("svc").await.unwrap();
        assert_eq!(
            t.seen(),
            [
                format!("GET {SVC}"),
                "DELETE /api/v4/projects/42".to_string(),
                "GET /api/v4/projects/42".to_string(),
                "DELETE /api/v4/projects/42".to_string()
            ]
        );
        let q = queries.lock().unwrap();
        assert_eq!(
            *q,
            ["", "acme-sandbox%2Fservices%2Fsvc-deleted-42"],
            "the second DELETE must name the renamed path"
        );
    }

    /// Answers the first GET with a live project, the DELETE with 202, and the read-back with
    /// `read_back`. Counts deletes so a test can tell whether the permanent one was attempted.
    fn delete_server(read_back: fn() -> http::Response<Bytes>) -> (Client, Arc<AtomicUsize>) {
        let deletes = Arc::new(AtomicUsize::new(0));
        let d2 = deletes.clone();
        let (c, _) = serve(Box::new(move |r| {
            if r.method() == Method::DELETE {
                d2.fetch_add(1, Ordering::SeqCst);
                return reply(202, "");
            }
            if d2.load(Ordering::SeqCst) == 0 {
                return reply(
                    200,
                    r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc"}"#,
                );
            }
            read_back()
        }));
        (c, deletes)
    }

    /// An instance without the deletion delay removes the project outright, and the
    /// read-back 404s. That is the one answer that means gone.
    #[tokio::test(start_paused = true)]
    async fn delete_accepts_an_immediate_removal() {
        let (c, deletes) = delete_server(|| reply(404, ""));
        c.delete("svc")
            .await
            .expect("a 404 read-back means the project is gone");
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            1,
            "nothing left to remove, so no second delete"
        );
    }

    /// A project still there but not scheduled was removed by the first call as far as GitLab
    /// is concerned; there is nothing to make permanent.
    #[tokio::test(start_paused = true)]
    async fn delete_accepts_an_unscheduled_project() {
        let (c, deletes) = delete_server(|| {
            reply(
                200,
                r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc"}"#,
            )
        });
        c.delete("svc").await.unwrap();
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            1,
            "not scheduled, so no second delete"
        );
    }

    /// The regression. A read-back that fails for any other reason says nothing about whether
    /// the project went, and the delete used to report success anyway -- which is how one was
    /// left sitting in deletion_scheduled after destroy said it had deleted it.
    #[tokio::test(start_paused = true)]
    async fn delete_refuses_to_guess_from_a_failed_read_back() {
        let (c, deletes) = delete_server(|| reply(500, ""));
        let err = c
            .delete("svc")
            .await
            .expect_err("a read-back that failed is not evidence the project is gone");
        assert!(err.to_string().contains("could not be read back"), "{err}");
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            1,
            "the permanent delete cannot be attempted without the renamed path"
        );
    }

    /// A refused permanent delete leaves the project scheduled, which is exactly the state
    /// the caller must hear about rather than success.
    #[tokio::test(start_paused = true)]
    async fn delete_surfaces_a_refused_permanent_removal() {
        let deletes = Arc::new(AtomicUsize::new(0));
        let d2 = deletes.clone();
        let (c, _) = serve(Box::new(move |r| {
            if r.method() == Method::DELETE {
                if d2.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                    return reply(403, "");
                }
                return reply(202, "");
            }
            if d2.load(Ordering::SeqCst) == 0 {
                return reply(
                    200,
                    r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc"}"#,
                );
            }
            reply(
                200,
                r#"{"id":42,"path_with_namespace":"acme-sandbox/services/svc-deleted-42","marked_for_deletion_on":"2026-09-26"}"#,
            )
        }));
        let err = c
            .delete("svc")
            .await
            .expect_err("a project left scheduled must not be reported as deleted");
        assert!(
            err.to_string()
                .contains("scheduled for deletion but not removed"),
            "{err}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delete_of_a_missing_project_is_not_an_error() {
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        c.delete("gone").await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn create_and_requests() {
        let body = Arc::new(Mutex::new(serde_json::Value::Null));
        let b2 = body.clone();
        let (c, t) = serve(Box::new(move |r| {
            if r.uri().path() == GROUP {
                return reply(200, r#"{"id":7}"#);
            }
            *b2.lock().unwrap() = body_json(r);
            reply(200, "")
        }));
        c.create(
            "svc",
            "private",
            "master",
            &["forgelab-managed".into()],
            "forgelab-managed",
        )
        .await
        .unwrap();
        {
            let b = body.lock().unwrap();
            assert!(
                b["namespace_id"] == 7
                    && b["path"] == "svc"
                    && b["visibility"] == "private"
                    && b["initialize_with_readme"] == false,
                "create: {b}"
            );
        }
        c.create("other", "public", "main", &[], "forgelab-managed")
            .await
            .unwrap();
        assert_eq!(
            t.seen().iter().filter(|s| s.contains("/groups/")).count(),
            1,
            "the group id must be looked up once"
        );

        c.close_request("svc", 7).await.unwrap();
        assert_eq!(body.lock().unwrap()["state_event"], "close");
        c.delete_branch("svc", "feature/run-42").await.unwrap();
        assert_eq!(
            t.seen().last().unwrap(),
            &format!("DELETE {SVC}/repository/branches/feature%2Frun-42")
        );

        let remote = c.git_remote("svc").unwrap();
        assert_eq!(
            remote.url.as_str(),
            "http://gl.test/acme-sandbox/services/svc.git"
        );
        let auth = remote.auth.unwrap();
        assert_eq!(auth.username, "oauth2");
        assert_eq!(auth.secret.expose_secret(), "s3cret");
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_is_waited_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let (c, _) = serve(Box::new(move |_| {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                return http::Response::builder()
                    .status(429)
                    .header("Retry-After", "3")
                    .body(Bytes::from("slow down"))
                    .unwrap();
            }
            reply(200, "")
        }));
        c.set_topics("svc", &[]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A bare 422 on an update is tried again a few times, and no time is wasted after the
    /// last attempt.
    #[tokio::test(start_paused = true)]
    async fn update_retries_a_bare_422() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let (c, _) = serve(Box::new(move |_| {
            if c2.fetch_add(1, Ordering::SeqCst) < 2 {
                return reply(422, r#"{"message":"Project could not be updated!"}"#);
            }
            reply(200, "")
        }));
        c.set_topics("svc", &[]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        calls.store(0, Ordering::SeqCst);
        let (c, _) = serve(Box::new(|_| reply(422, "")));
        let started = tokio::time::Instant::now();
        assert!(
            c.set_topics("svc", &[])
                .await
                .unwrap_err()
                .is_status(&[422])
        );
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1 + 2 + 3),
            "three pauses between four attempts, none after the last"
        );
    }

    /// GitLab sends `null` for an empty field -- a group being deleted had one -- and that is
    /// read as empty, not as a response that fails to decode.
    #[test]
    fn nulls_read_as_empty() {
        let g: GroupInfo = serde_json::from_str(
            r#"{"id":1,"full_path":"acme-sandbox/acme","visibility":"public","description":null,"marked_for_deletion_on":null}"#,
        )
        .unwrap();
        assert_eq!((g.id, g.description.as_str(), g.deleting), (1, "", None));
        let g: GroupInfo =
            serde_json::from_str(r#"{"id":2,"full_path":null,"visibility":null}"#).unwrap();
        assert_eq!((g.full_path.as_str(), g.visibility.as_str()), ("", ""));
    }

    /// A delete refused with a 400 "Project could not be updated!" is tried again, as seen in a
    /// real destroy of the scale fleet; a 400 saying anything else is not.
    #[tokio::test(start_paused = true)]
    async fn delete_retries_a_busy_refusal() {
        let deletes = Arc::new(AtomicUsize::new(0));
        let d2 = deletes.clone();
        let (c, _) = serve(Box::new(move |r| match r.method().as_str() {
            "GET" if d2.load(Ordering::SeqCst) < 2 => reply(
                200,
                r#"{"id":7,"path_with_namespace":"acme-sandbox/services/svc"}"#,
            ),
            "GET" => reply(404, ""),
            "DELETE" if d2.fetch_add(1, Ordering::SeqCst) == 0 => {
                reply(400, r#"{"message":"Project could not be updated!"}"#)
            }
            "DELETE" => reply(202, ""),
            m => panic!("unexpected {m}"),
        }));
        c.delete("svc").await.unwrap();
        assert_eq!(deletes.load(Ordering::SeqCst), 2);

        let (c, t) = serve(Box::new(|r| match r.method().as_str() {
            "GET" => reply(
                200,
                r#"{"id":7,"path_with_namespace":"acme-sandbox/services/svc"}"#,
            ),
            _ => reply(400, r#"{"message":"bad request"}"#),
        }));
        assert!(c.delete("svc").await.unwrap_err().is_status(&[400]));
        assert_eq!(
            t.seen().iter().filter(|s| s.starts_with("DELETE")).count(),
            1
        );
    }

    /// A namespace is a chain of subgroups: made on the way to the first project that needs
    /// them, each as visible as its parent, and never looked up twice.
    #[tokio::test(start_paused = true)]
    async fn create_makes_subgroups() {
        let posts = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let p2 = posts.clone();
        let (c, t) = serve(Box::new(move |r| {
            match (r.method().as_str(), r.uri().path()) {
                ("GET", GROUP) => reply(200, r#"{"id":7,"visibility":"public"}"#),
                ("GET", _) => reply(404, ""),
                _ => {
                    let mut p = p2.lock().unwrap();
                    p.push(body_json(r));
                    reply(
                        200,
                        &format!(r#"{{"id":{},"visibility":"public"}}"#, 7 + p.len()),
                    )
                }
            }
        }));
        c.create(
            "platform/core/api",
            "private",
            "main",
            &[],
            "forgelab-managed",
        )
        .await
        .unwrap();
        c.create(
            "platform/core/cli",
            "private",
            "main",
            &[],
            "forgelab-managed",
        )
        .await
        .unwrap();
        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), 4, "{posts:?}");
        assert_eq!(
            posts[0],
            serde_json::json!({"name": "platform", "path": "platform", "parent_id": 7, "visibility": "public", "description": NAMESPACE_MARKER})
        );
        assert_eq!(
            posts[1],
            serde_json::json!({"name": "core", "path": "core", "parent_id": 8, "visibility": "public", "description": NAMESPACE_MARKER})
        );
        assert!(
            posts[2]["path"] == "api"
                && posts[2]["namespace_id"] == 9
                && posts[3]["namespace_id"] == 9,
            "projects: {:?}",
            &posts[2..]
        );
        assert_eq!(
            t.seen().iter().filter(|s| s.starts_with("GET ")).count(),
            3,
            "want one lookup per group: {:?}",
            t.seen()
        );
    }

    /// Eight repositories in one new subgroup make it once, without the other seven waiting
    /// on a lock while it is made.
    #[tokio::test(start_paused = true)]
    async fn concurrent_creates_make_a_subgroup_once() {
        let posts = Arc::new(AtomicUsize::new(0));
        let p2 = posts.clone();
        let (c, _) = serve(Box::new(move |r| {
            match (r.method().as_str(), r.uri().path()) {
                ("GET", GROUP) => reply(200, r#"{"id":7,"visibility":"public"}"#),
                ("GET", _) => reply(404, ""),
                ("POST", "/api/v4/groups") => {
                    p2.fetch_add(1, Ordering::SeqCst);
                    reply(200, r#"{"id":8,"visibility":"public"}"#)
                }
                _ => reply(200, ""),
            }
        }));
        let c = Arc::new(c);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..8 {
            let c = c.clone();
            tasks.spawn(async move {
                c.create(
                    &format!("platform/svc{i}"),
                    "private",
                    "main",
                    &[],
                    "forgelab-managed",
                )
                .await
            });
        }
        while let Some(r) = tasks.join_next().await {
            r.unwrap().unwrap();
        }
        assert_eq!(posts.load(Ordering::SeqCst), 1);
    }

    /// The marker alone decides: what a marked subgroup holds goes with it, as a marked
    /// repository's branches and requests do.
    #[tokio::test(start_paused = true)]
    async fn delete_namespace() {
        const CORE: &str = "/api/v4/groups/acme-sandbox%2Fservices%2Fcore";
        struct Case {
            name: &'static str,
            group: &'static str,
            want: Removal,
            deletes: usize,
        }
        let cases = [
            Case {
                name: "ours",
                group: r#"{"id":9,"description":"forgelab-managed"}"#,
                want: Removal::Removed,
                deletes: 2,
            },
            Case {
                name: "somebody else's",
                group: r#"{"id":9,"description":"Platform team"}"#,
                want: Removal::Kept("not created by forgelab".into()),
                deletes: 0,
            },
            Case {
                name: "scheduled before",
                group: r#"{"id":9,"description":"forgelab-managed","marked_for_deletion_on":"2026-09-26"}"#,
                want: Removal::Removed,
                deletes: 1,
            },
        ];
        for tc in cases {
            let deletes = Arc::new(AtomicUsize::new(0));
            let d2 = deletes.clone();
            let (group, want_deletes) = (tc.group, tc.deletes);
            let (c, t) = serve(Box::new(move |r| {
                if r.method() == Method::DELETE {
                    d2.fetch_add(1, Ordering::SeqCst);
                    return reply(202, "");
                }
                if r.uri().path() == CORE {
                    return reply(200, group);
                }
                if d2.load(Ordering::SeqCst) < want_deletes {
                    // scheduled only, and renamed: the second DELETE names that path
                    return reply(
                        200,
                        r#"{"full_path":"acme-sandbox/services/core-deletion_scheduled-9","marked_for_deletion_on":"2026-09-26"}"#,
                    );
                }
                reply(404, "")
            }));
            let got = c.delete_namespace("core").await.unwrap();
            assert_eq!(got, tc.want, "{}", tc.name);
            assert_eq!(deletes.load(Ordering::SeqCst), tc.deletes, "{}", tc.name);
            assert!(
                !t.seen().iter().any(|s| s.contains("/projects")),
                "{}: what the subgroup holds is nobody's business: {:?}",
                tc.name,
                t.seen()
            );
        }

        let (c, t) = serve(Box::new(|_| reply(404, "")));
        for ns in ["gone", ""] {
            assert_eq!(
                c.delete_namespace(ns).await.unwrap(),
                Removal::Absent,
                "{ns:?} is neither removed nor kept"
            );
        }
        assert_eq!(
            t.seen().len(),
            1,
            "the sandbox's own group is not even looked at: {:?}",
            t.seen()
        );
    }

    /// A subgroup GitLab never finishes deleting is reported as kept after the bound, not as
    /// removed -- and the caller exits non-zero on a kept namespace.
    #[tokio::test(start_paused = true)]
    async fn delete_namespace_reports_a_group_that_never_goes() {
        let (c, t) = serve(Box::new(|r| {
            if r.method() == Method::DELETE {
                return reply(202, "");
            }
            reply(
                200,
                r#"{"id":9,"full_path":"acme-sandbox/services/core","description":"forgelab-managed","marked_for_deletion_on":"2026-09-26"}"#,
            )
        }));
        let started = tokio::time::Instant::now();
        let got = c.delete_namespace("core").await.unwrap();
        assert_eq!(
            got,
            Removal::Kept("still being deleted by GitLab after 5m0s".into())
        );
        assert!(
            started.elapsed() >= Duration::from_secs(290),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            t.seen().iter().filter(|s| s.starts_with("DELETE ")).count(),
            1,
            "the permanent removal is asked for once: {:?}",
            t.seen()
        );
    }

    /// A subgroup that GitLab has only scheduled for deletion still answers, and keeps its
    /// path. Where the Go client refused at once, the port waits for it to go and makes it
    /// again; one that never goes is refused with the same words.
    #[tokio::test(start_paused = true)]
    async fn create_waits_for_a_subgroup_pending_deletion() {
        let gets = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let (g2, p2) = (gets.clone(), posts.clone());
        let (c, _) = serve(Box::new(move |r| {
            match (r.method().as_str(), r.uri().path()) {
                ("GET", GROUP) => reply(
                    200,
                    r#"{"id":7,"full_path":"acme-sandbox/services","visibility":"private"}"#,
                ),
                ("GET", "/api/v4/groups/acme-sandbox%2Fservices%2Fcore") => reply(
                    200,
                    r#"{"id":9,"full_path":"acme-sandbox/services/core","marked_for_deletion_on":"2026-09-26"}"#,
                ),
                ("GET", "/api/v4/groups/9") => {
                    // gone on the third look
                    if g2.fetch_add(1, Ordering::SeqCst) < 2 {
                        reply(
                            200,
                            r#"{"id":9,"full_path":"acme-sandbox/services/core-deletion_scheduled-9","marked_for_deletion_on":"2026-09-26"}"#,
                        )
                    } else {
                        reply(404, "")
                    }
                }
                ("DELETE", _) => reply(202, ""),
                ("POST", path) => {
                    p2.lock().unwrap().push((path.to_string(), body_json(r)));
                    reply(
                        200,
                        r#"{"id":10,"full_path":"acme-sandbox/services/core","visibility":"private"}"#,
                    )
                }
                _ => reply(404, ""),
            }
        }));
        c.create("core/api", "private", "main", &[], "forgelab-managed")
            .await
            .unwrap();
        {
            let posts = posts.lock().unwrap();
            assert_eq!(posts.len(), 2, "{posts:?}");
            assert_eq!(posts[0].0, "/api/v4/groups");
            assert_eq!(posts[0].1["path"], "core");
            assert_eq!(posts[1].0, "/api/v4/projects");
            assert_eq!(posts[1].1["namespace_id"], 10);
        }

        let (c, _) = serve(Box::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("GET", GROUP) => reply(
                200,
                r#"{"id":7,"full_path":"acme-sandbox/services","visibility":"private"}"#,
            ),
            ("DELETE", _) => reply(403, ""),
            _ => reply(
                200,
                r#"{"id":9,"full_path":"acme-sandbox/services/core","marked_for_deletion_on":"2026-09-26"}"#,
            ),
        }));
        let err = c
            .create("core/api", "private", "main", &[], "forgelab-managed")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pending deletion"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_org_names_a_missing_group() {
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        let err = c.ensure_org().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "group \"acme-sandbox/services\" does not exist, or the token cannot see it"
        );
    }

    fn project(path: &str, empty: bool, mrs: (usize, &[&str])) -> serde_json::Value {
        serde_json::json!({
            "fullPath": path,
            "visibility": "private",
            "archived": false,
            "topics": ["go", "forgelab-managed"],
            "markedForDeletionOn": null,
            "repository": {"empty": empty, "rootRef": if empty { None } else { Some("main") },
                           "tree": if empty { None } else { Some(serde_json::json!({"lastCommit": {"sha": "abc"}})) }},
            "mergeRequests": {"count": mrs.0, "nodes": mrs.1.iter().map(|i| serde_json::json!({"iid": i, "title": "t"})).collect::<Vec<_>>()},
        })
    }

    /// Fifty full paths to a query; whatever is not in the answer -- missing, renamed, being
    /// deleted -- is missing, and a project that says it is empty is asked again by REST,
    /// which knows how to tell an unseeded project from one seeded a moment ago.
    #[tokio::test(start_paused = true)]
    async fn get_many_looks_projects_up_by_full_path() {
        let (c, t) = serve(Box::new(|r| {
            if r.uri().path() == "/api/graphql" {
                let paths = body_json(r)["variables"]["p"].clone();
                assert_eq!(paths[0], "acme-sandbox/services/platform/api");
                let mut deleting = project("acme-sandbox/services/doomed", false, (0, &[]));
                deleting["markedForDeletionOn"] = "2026-10-02".into();
                return reply(
                    200,
                    &serde_json::json!({"data": {"projects": {"nodes": [
                        project("acme-sandbox/services/Platform/API", false, (1, &["7"])),
                        deleting,
                        project("acme-sandbox/services/fresh", true, (0, &[])),
                        project("acme-sandbox/services/busy", false, (150, &["1"])),
                    ]}}})
                    .to_string(),
                );
            }
            // `get` asking about the one that claimed to be empty.
            match r.uri().path() {
                "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh" => reply(
                    200,
                    r#"{"path_with_namespace":"acme-sandbox/services/fresh","default_branch":"main","visibility":"private","empty_repo":true,"topics":["forgelab-managed"]}"#,
                ),
                "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh/repository/branches" => {
                    reply(200, r#"[{"name":"main","commit":{"id":"abc"}}]"#)
                }
                other => panic!("unexpected {other}"),
            }
        }));
        let names: Vec<String> = ["platform/api", "gone", "doomed", "fresh", "busy"]
            .map(String::from)
            .to_vec();
        let got = c.get_many(&names).await.unwrap();
        assert_eq!(t.seen()[0], "POST /api/graphql");

        let api = got[0].as_ref().expect("matched case-insensitively");
        assert_eq!(api.name, "platform/api");
        assert_eq!(api.default_branch, "main");
        assert_eq!(api.head.as_deref(), Some("abc"));
        assert_eq!(api.topics, ["go", "forgelab-managed"]);
        assert_eq!(
            api.open_requests,
            Some(vec![Request {
                number: 7,
                title: "t".into()
            }])
        );
        assert!(got[1].is_none(), "not in the answer");
        assert!(got[2].is_none(), "scheduled for deletion");
        let fresh = got[3].as_ref().expect("fresh");
        assert!(!fresh.empty, "REST found its branch: seeded a moment ago");
        assert_eq!(
            got[4].as_ref().unwrap().open_requests,
            None,
            "more than a page"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn get_many_batches_by_fifty_and_fails_on_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let (c, _) = serve(Box::new(move |r| {
            assert!(body_json(r)["variables"]["p"].as_array().unwrap().len() <= 50);
            if c2.fetch_add(1, Ordering::SeqCst) == 2 {
                return reply(200, r#"{"errors":[{"message":"boom"}]}"#);
            }
            reply(200, r#"{"data":{"projects":{"nodes":[]}}}"#)
        }));
        let names: Vec<String> = (0..120).map(|i| format!("svc-{i}")).collect();
        let err = c.get_many(&names).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "120 names, batches of 50");
    }
}
