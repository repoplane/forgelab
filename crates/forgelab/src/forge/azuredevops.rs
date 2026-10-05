//! `Forge` against Azure DevOps Services and Server.
//!
//! It is the odd one out. Repositories live in a project inside the organisation; they have
//! no topics and no visibility of their own; and the nearest thing to "archived" is
//! "disabled", which makes a repository unreadable rather than read-only. See `caps`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use http::{HeaderMap, HeaderValue, Method};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::error::ForgeError;
use super::http::{Answer, HttpClient, Pauses, RequestOpts, Transport};
use super::keyed::KeyedOnce;
use super::{
    Caps, Forge, ForgePolicy, GitAuth, GitRemote, NAMESPACE_MARKER, NamespaceDepth, Ref, Removal,
    Repo, Request, Settings, classify, flat_name, path_escape, query_escape,
};

/// Used when a sandbox names no base_url.
pub const DEFAULT_BASE_URL: &str = "https://dev.azure.com";

const API_VERSION: &str = "api-version=7.1";
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// Talks to one organisation. Repositories without a namespace live in its default project; a
/// namespace's first segment names another project, and the rest is joined into the name.
/// Every project is made by `create` when first needed, the default one included.
pub struct Client {
    base_url: String,
    org: String,
    project: String,
    token: SecretString,
    http: HttpClient,
    /// Remembers project ids by name. Making a project runs once however many repositories
    /// need it at the same time, and workers in other projects do not wait for it.
    projects: KeyedOnce<String, ProjectInfo>,
    /// Project creations go one at a time. The organisation builds each one in the background,
    /// and several requested at once can fail with no reason given -- which is what the first
    /// apply of a fleet with three namespaces did.
    creating: tokio::sync::Mutex<()>,
    cancel: CancellationToken,
}

#[derive(Debug, Clone)]
struct ProjectInfo {
    id: String,
    /// Made by this client, so still holding only what it came with.
    born: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Repository {
    #[serde(default, deserialize_with = "super::null_default")]
    id: String,
    #[serde(default, deserialize_with = "super::null_default")]
    name: String,
    #[serde(
        default,
        rename = "defaultBranch",
        deserialize_with = "super::null_default"
    )]
    default_branch: String,
    #[serde(default, rename = "isDisabled")]
    is_disabled: bool,
    /// Where lookup found it.
    #[serde(skip)]
    project: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Operation {
    #[serde(default, deserialize_with = "super::null_default")]
    id: String,
    #[serde(default, deserialize_with = "super::null_default")]
    status: String,
    #[serde(
        default,
        rename = "resultMessage",
        deserialize_with = "super::null_default"
    )]
    result_message: String,
    #[serde(
        default,
        rename = "detailedMessage",
        deserialize_with = "super::null_default"
    )]
    detailed_message: String,
}

#[derive(Deserialize)]
struct Values<T> {
    #[serde(
        default = "Vec::new",
        bound(deserialize = "T: Deserialize<'de>"),
        deserialize_with = "super::null_default"
    )]
    value: Vec<T>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawRef {
    #[serde(default, deserialize_with = "super::null_default")]
    name: String,
    #[serde(default, rename = "objectId", deserialize_with = "super::null_default")]
    object_id: String,
}

impl Client {
    /// Reports every rate-limit pause to `pauses`, which the run's progress line reads.
    pub fn with_pauses(mut self, pauses: Arc<Pauses>) -> Client {
        self.http = self.http.with_pauses(pauses.clone());
        self
    }

    /// `base_url` is https://dev.azure.com, or a Server's collection root.
    pub fn new(
        base_url: &str,
        org: &str,
        default_project: &str,
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
        // A PAT is the password of an empty username.
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!(":{}", token.expose_secret()));
        let mut auth = HeaderValue::from_str(&format!("Basic {basic}"))
            .unwrap_or_else(|_| HeaderValue::from_static("Basic"));
        auth.set_sensitive(true);
        headers.insert("authorization", auth);
        let http = HttpClient::new("Azure DevOps", transport, classify::azuredevops, headers)
            .with_cancel(cancel.clone());
        Ok(Client {
            base_url: base_url.trim_end_matches('/').to_string(),
            org: org.to_string(),
            project: default_project.to_string(),
            token,
            http,
            projects: KeyedOnce::default(),
            creating: tokio::sync::Mutex::new(()),
            cancel,
        })
    }

    /// `path` is relative to the organisation; `query` carries no api-version.
    fn url(&self, path: &str, query: &str) -> String {
        let mut target = format!(
            "{}/{}{path}?{API_VERSION}",
            self.base_url,
            path_escape(&self.org)
        );
        if !query.is_empty() {
            target.push('&');
            target.push_str(query);
        }
        target
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(HeaderMap, Option<T>), ForgeError> {
        self.http
            .json(
                method.clone(),
                &self.url(path, query),
                body,
                RequestOpts::default(),
            )
            .await
            .map_err(|e| self.sign_in(e, &method, path))
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Answer, ForgeError> {
        self.http
            .call(
                method.clone(),
                &self.url(path, query),
                body,
                RequestOpts::default(),
            )
            .await
            .map_err(|e| self.sign_in(e, &method, path))
    }

    /// An expired or wrong PAT is answered with a 203 and a sign-in page, not a 401. Said in
    /// words rather than as a page of HTML.
    fn sign_in(&self, e: ForgeError, method: &Method, path: &str) -> ForgeError {
        if e.is_status(&[203]) {
            return ForgeError::msg(format!(
                "{method} {path}: not authenticated: the token is wrong, expired, or not valid for organisation {:?}",
                self.org
            ));
        }
        e
    }

    async fn sleep(&self, d: Duration) -> Result<(), ForgeError> {
        tokio::select! {
            _ = tokio::time::sleep(d) => Ok(()),
            _ = self.cancel.cancelled() => Err(ForgeError::Cancelled),
        }
    }

    /// Lands a fleet name: "dotfiles" is that repository in the sandbox's project, and
    /// "platform/core/api" is core-api in the project platform.
    fn split(&self, name: &str) -> Result<(String, String), ForgeError> {
        match name.split_once('/') {
            None => Ok((self.project.clone(), name.to_string())),
            Some((project, rest)) => {
                // Otherwise platform/x and x would be one repository, which the fleet cannot
                // see coming.
                if project.eq_ignore_ascii_case(&self.project) {
                    return Err(ForgeError::msg(format!(
                        "namespace {project:?} is the sandbox's own project, where repositories without a namespace already go: rename one of the two"
                    )));
                }
                Ok((project.to_string(), flat_name(rest)))
            }
        }
    }

    fn git(project: &str, suffix: &str) -> String {
        format!("/{}/_apis/git{suffix}", path_escape(project))
    }

    async fn fetch_project_id(&self, project: &str) -> Result<Option<String>, ForgeError> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "super::null_default")]
            id: String,
        }
        match self
            .json::<P>(
                Method::GET,
                &format!("/_apis/projects/{}", path_escape(project)),
                "",
                None,
            )
            .await
        {
            Ok((_, p)) => Ok(Some(p.map(|p| p.id).unwrap_or_default())),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Resolves a project to its id. One that is missing is reported as such, or with `create`
    /// is made -- once, however many repositories ask at the same time, and without anyone
    /// else waiting on it.
    async fn project_ident(
        &self,
        project: &str,
        create: bool,
    ) -> Result<Option<ProjectInfo>, ForgeError> {
        let key = project.to_string();
        if let Some(p) = self.projects.get(&key) {
            return Ok(Some(p));
        }
        if !create {
            return match self.fetch_project_id(project).await? {
                Some(id) => {
                    let p = ProjectInfo { id, born: false };
                    self.projects.set(&key, p.clone());
                    Ok(Some(p))
                }
                None => Ok(None),
            };
        }
        let p = self
            .projects
            .get_or_try_init(&key, async {
                if let Some(id) = self.fetch_project_id(project).await? {
                    return Ok(ProjectInfo { id, born: false });
                }
                self.create_project_retried(project)
                    .await
                    .map_err(|e| ForgeError::msg(format!("create project {project}: {e}")))?;
                match self.fetch_project_id(project).await? {
                    Some(id) => Ok(ProjectInfo { id, born: true }),
                    None => Err(ForgeError::msg(format!(
                        "create project {project}: made, yet it does not answer"
                    ))),
                }
            })
            .await?;
        Ok(Some(p))
    }

    /// Makes a project, one at a time across the client, and tries again when the background
    /// operation fails: that failure has come with no reason and gone away on the next try.
    /// Before each new try the project is looked up, since a failed operation may still have
    /// left it behind.
    async fn create_project_retried(&self, project: &str) -> Result<(), ForgeError> {
        let _one_at_a_time = self.creating.lock().await;
        let mut wait = Duration::from_secs(5);
        for attempt in 1..=3 {
            match self.create_project(project).await {
                Ok(()) => return Ok(()),
                Err(e) if attempt < 3 && e.to_string().starts_with("operation failed") => {
                    tracing::debug!(
                        project,
                        attempt,
                        "project creation failed, trying again: {e}"
                    );
                    self.sleep(wait).await?;
                    wait *= 2;
                    if self.fetch_project_id(project).await?.is_some() {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    /// Makes a private Git project on the organisation's default process, and waits for it:
    /// Azure DevOps builds a project in the background.
    async fn create_project(&self, project: &str) -> Result<(), ForgeError> {
        #[derive(Deserialize)]
        struct Process {
            #[serde(default, deserialize_with = "super::null_default")]
            id: String,
            #[serde(default, rename = "isDefault")]
            is_default: bool,
        }
        let (_, procs): (_, Option<Values<Process>>) = self
            .json(Method::GET, "/_apis/process/processes", "", None)
            .await?;
        let procs = procs.map(|v| v.value).unwrap_or_default();
        let Some(first) = procs.first() else {
            return Err(ForgeError::msg(
                "the organisation lists no process to create it on",
            ));
        };
        let process = procs
            .iter()
            .find(|p| p.is_default)
            .unwrap_or(first)
            .id
            .clone();
        let body = serde_json::json!({
            "name": project,
            "description": NAMESPACE_MARKER,
            "visibility": "private",
            "capabilities": {
                "versioncontrol": {"sourceControlType": "Git"},
                "processTemplate": {"templateTypeId": process},
            },
        });
        let (_, op): (_, Option<Operation>) = self
            .http
            .json(
                Method::POST,
                &self.url("/_apis/projects", ""),
                Some(&body),
                RequestOpts {
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        self.wait(op.unwrap_or_default()).await
    }

    /// Polls a background operation, for about three minutes.
    async fn wait(&self, mut op: Operation) -> Result<(), ForgeError> {
        for _ in 0..90 {
            self.sleep(Duration::from_secs(2)).await?;
            let (_, next): (_, Option<Operation>) = self
                .json(
                    Method::GET,
                    &format!("/_apis/operations/{}", path_escape(&op.id)),
                    "",
                    None,
                )
                .await?;
            if let Some(next) = next {
                op = Operation {
                    id: if next.id.is_empty() { op.id } else { next.id },
                    ..next
                };
            }
            match op.status.as_str() {
                "succeeded" => return Ok(()),
                "failed" | "cancelled" => {
                    let why = [op.result_message.trim(), op.detailed_message.trim()]
                        .into_iter()
                        .filter(|m| !m.is_empty())
                        .collect::<Vec<_>>()
                        .join(": ");
                    let why = if why.is_empty() {
                        "Azure DevOps gave no reason".to_string()
                    } else {
                        why
                    };
                    return Err(ForgeError::msg(format!("operation {}: {why}", op.status)));
                }
                _ => {}
            }
        }
        Err(ForgeError::msg(format!(
            "operation {} is still {}",
            op.id, op.status
        )))
    }

    /// Finds a repository by name. Azure DevOps caches both ways of asking for about a second,
    /// in opposite directions, so each is used for what it gets right:
    ///
    ///   - a direct GET sees a new repository at once, but answers 404 for a disabled one and
    ///     keeps answering 200 for one that was just deleted (`get` catches that: its refs are
    ///     gone);
    ///   - the project's listing is the only place a disabled repository shows up, but it lags
    ///     on repositories just created and on a flag just changed.
    ///
    /// So: GET first, and on a 404 the listing decides between "disabled" and "missing". If
    /// the listing still calls it enabled, one of the two caches is stale; look again shortly.
    async fn lookup(&self, name: &str) -> Result<Option<Repository>, ForgeError> {
        let (project, name) = self.split(name)?;
        match self
            .json::<Repository>(
                Method::GET,
                &Self::git(&project, &format!("/repositories/{}", path_escape(&name))),
                "",
                None,
            )
            .await
        {
            Ok((_, Some(mut r))) => {
                r.project = project;
                return Ok(if r.name.eq_ignore_ascii_case(&name) {
                    Some(r)
                } else {
                    None
                });
            }
            Ok((_, None)) => return Ok(None),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
        for attempt in 1.. {
            let all: Vec<Repository> = match self
                .json::<Values<Repository>>(
                    Method::GET,
                    &Self::git(&project, "/repositories"),
                    "",
                    None,
                )
                .await
            {
                Ok((_, v)) => v.map(|v| v.value).unwrap_or_default(),
                Err(e) if e.is_not_found() => return Ok(None), // no such project yet: create makes it
                Err(e) => return Err(e),
            };
            match all.into_iter().find(|r| r.name.eq_ignore_ascii_case(&name)) {
                None => return Ok(None),
                Some(mut r) if r.is_disabled => {
                    r.project = project;
                    return Ok(Some(r));
                }
                Some(_) if attempt == 4 => return Ok(None), // listed as enabled, yet unreadable: gone
                Some(_) => {}
            }
            self.sleep(Duration::from_secs(1)).await?;
        }
        unreachable!()
    }

    /// Lists refs under a prefix such as "heads/", following the continuation token.
    async fn refs_in(
        &self,
        project: &str,
        name: &str,
        filter: &str,
    ) -> Result<Vec<RawRef>, ForgeError> {
        let mut all = Vec::new();
        let mut token = String::new();
        loop {
            let mut q = format!("filter={}", query_escape(filter));
            if !token.is_empty() {
                q.push_str(&format!("&continuationToken={}", query_escape(&token)));
            }
            let (headers, page): (_, Option<Values<RawRef>>) = self
                .json(
                    Method::GET,
                    &Self::git(
                        project,
                        &format!("/repositories/{}/refs", path_escape(name)),
                    ),
                    &q,
                    None,
                )
                .await?;
            all.extend(page.map(|p| p.value).unwrap_or_default());
            token = headers
                .get("x-ms-continuationtoken")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                return Ok(all);
            }
        }
    }

    async fn refs(&self, name: &str, filter: &str) -> Result<Vec<RawRef>, ForgeError> {
        let (project, name) = self.split(name)?;
        self.refs_in(&project, &name, filter).await
    }

    /// Updates the ref to the zero id, which needs the id it currently points at.
    async fn delete_ref(&self, name: &str, full: &str) -> Result<(), ForgeError> {
        let (project, name) = self.split(name)?;
        let items = self
            .refs_in(&project, &name, full.strip_prefix("refs/").unwrap_or(full))
            .await?;
        // The filter is a prefix match.
        let Some(r) = items.into_iter().find(|r| r.name == full) else {
            return Ok(()); // already gone
        };
        #[derive(Deserialize)]
        struct Update {
            #[serde(default)]
            success: bool,
            #[serde(
                default,
                rename = "updateStatus",
                deserialize_with = "super::null_default"
            )]
            update_status: String,
        }
        let body = serde_json::json!([{ "name": full, "oldObjectId": r.object_id, "newObjectId": ZERO_SHA }]);
        let (_, res): (_, Option<Values<Update>>) = self
            .http
            .json(
                Method::POST,
                &self.url(
                    &Self::git(
                        &project,
                        &format!("/repositories/{}/refs", path_escape(&name)),
                    ),
                    "",
                ),
                Some(&body),
                RequestOpts {
                    idempotent: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        match res.and_then(|v| v.value.into_iter().next()) {
            Some(u) if u.success => Ok(()),
            Some(u) => Err(ForgeError::msg(format!(
                "delete {full}: {}",
                u.update_status
            ))),
            None => Err(ForgeError::msg(format!("delete {full}: no result"))),
        }
    }

    async fn patch_repo(
        &self,
        r: &Repository,
        fields: serde_json::Value,
    ) -> Result<(), ForgeError> {
        self.call(
            Method::PATCH,
            &Self::git(&r.project, &format!("/repositories/{}", r.id)),
            "",
            Some(&fields),
        )
        .await
        .map(drop)
    }
}

#[async_trait]
impl Forge for Client {
    fn name(&self) -> &'static str {
        "Azure DevOps"
    }

    /// No topics (so no marker), visibility belongs to the project, and a disabled repository
    /// answers 404 to everything but the project's listing.
    fn caps(&self) -> Caps {
        Caps {
            topics: false,
            marker: false,
            visibility: false,
            archived_unreadable: true,
            namespace_depth: NamespaceDepth::Depth(1),
        }
    }

    fn policy(&self) -> ForgePolicy {
        ForgePolicy {
            default_concurrency: 8,
            namespace_gone_timeout: Duration::from_secs(300),
        }
    }

    /// Checks the organisation answers to this token: an organisation cannot be created
    /// through the API. Projects are made by `create`.
    async fn ensure_org(&self) -> Result<(), ForgeError> {
        match self
            .call(Method::GET, "/_apis/projects", "$top=1", None)
            .await
        {
            Err(e) if e.is_not_found() => Err(ForgeError::msg(format!(
                "organisation {:?} does not exist, or the token cannot see it",
                self.org
            ))),
            Err(e) => Err(e),
            Ok(_) => Ok(()),
        }
    }

    async fn get(&self, name: &str) -> Result<Option<Repo>, ForgeError> {
        let Some(r) = self.lookup(name).await? else {
            return Ok(None);
        };
        let mut out = Repo {
            name: r.name.clone(),
            default_branch: r
                .default_branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.default_branch)
                .to_string(),
            visibility: "private".to_string(), // the project's; not comparable per repository, see caps
            archived: r.is_disabled,
            empty: false,
            topics: Vec::new(),
            ..Repo::default()
        };
        if r.is_disabled {
            return Ok(Some(out)); // unreadable: nothing more can be learned
        }
        // `size` stays 0 well after a push, so emptiness is read from the refs.
        match self.branches(name).await {
            Ok(branches) => out.empty = branches.is_empty(),
            Err(e) if e.is_not_found() => return Ok(None), // the GET answered from cache for a repository just deleted
            Err(e) => return Err(e),
        }
        Ok(Some(out))
    }

    /// Ignores visibility, default_branch and topics: none of them exists per repository at
    /// creation. The first branch pushed becomes the default.
    async fn create(
        &self,
        name: &str,
        _visibility: &str,
        _default_branch: &str,
        _topics: &[String],
        _marker: &str,
    ) -> Result<(), ForgeError> {
        let (project, name) = self.split(name)?;
        let p = self
            .project_ident(&project, true)
            .await?
            .expect("a project is made when asked for");
        let body = serde_json::json!({ "name": name, "project": {"id": p.id} });
        match self
            .http
            .call(
                Method::POST,
                &self.url(&Self::git(&project, "/repositories"), ""),
                Some(&body),
                RequestOpts {
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await
        {
            // A project made a moment ago came with an empty repository of its own name, and a
            // fixture by that name takes it over. In any other project a conflict is a conflict.
            Err(e) if e.is_status(&[409]) && p.born && name.eq_ignore_ascii_case(&project) => {
                Ok(())
            }
            Err(e) => Err(e),
            Ok(_) => Ok(()),
        }
    }

    /// Removes the repository for good. A DELETE alone only moves it to the project's recycle
    /// bin; the name is free again at once, but the repository lingers, so it is purged. One
    /// that is already gone -- the lookup may answer from a cache for a repository deleted a
    /// moment ago -- is not an error.
    async fn delete(&self, name: &str) -> Result<(), ForgeError> {
        let Some(r) = self.lookup(name).await? else {
            return Ok(());
        };
        // A disabled repository answers 404 to everything, its own deletion included.
        if r.is_disabled {
            match self
                .patch_repo(&r, serde_json::json!({"isDisabled": false}))
                .await
            {
                Err(e) if e.is_not_found() => return Ok(()),
                Err(e) => return Err(ForgeError::msg(format!("enable before delete: {e}"))),
                Ok(()) => {}
            }
        }
        match self
            .call(
                Method::DELETE,
                &Self::git(&r.project, &format!("/repositories/{}", r.id)),
                "",
                None,
            )
            .await
        {
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        if let Err(e) = self
            .call(
                Method::DELETE,
                &Self::git(&r.project, &format!("/recycleBin/repositories/{}", r.id)),
                "",
                None,
            )
            .await
        {
            tracing::debug!("Azure DevOps: {name}: the recycle bin kept the repository: {e}");
        }
        Ok(())
    }

    /// Removes a project forgelab made, with all it holds. Only the first segment of a
    /// namespace is a project (see `caps`), and "" is the default one.
    async fn delete_namespace(&self, ns: &str) -> Result<Removal, ForgeError> {
        if ns.contains('/') {
            return Ok(Removal::Absent);
        }
        let ns = if ns.is_empty() {
            self.project.as_str()
        } else {
            ns
        };
        #[derive(Deserialize, Default)]
        struct P {
            #[serde(default, deserialize_with = "super::null_default")]
            id: String,
            #[serde(default, deserialize_with = "super::null_default")]
            description: String,
        }
        let p: P = match self
            .json::<P>(
                Method::GET,
                &format!("/_apis/projects/{}", path_escape(ns)),
                "",
                None,
            )
            .await
        {
            Ok((_, p)) => p.unwrap_or_default(),
            Err(e) if e.is_not_found() => return Ok(Removal::Absent),
            Err(e) => return Err(e),
        };
        if !p.description.starts_with(NAMESPACE_MARKER) {
            return Ok(Removal::Kept("not created by forgelab".to_string()));
        }
        self.projects.invalidate_where(|k| k == ns);
        let (_, op): (_, Option<Operation>) = match self
            .json(
                Method::DELETE,
                &format!("/_apis/projects/{}", p.id),
                "",
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(e) if e.is_not_found() => return Ok(Removal::Absent),
            Err(e) => return Err(e),
        };
        self.wait(op.unwrap_or_default()).await?;
        Ok(Removal::Removed)
    }

    async fn update_settings(&self, name: &str, s: Settings) -> Result<(), ForgeError> {
        let Some(r) = self.lookup(name).await? else {
            return Err(ForgeError::msg(format!("repository {name:?} not found")));
        };
        // A disabled repository rejects everything else, so enable first and disable last.
        if s.archived == Some(false) && r.is_disabled {
            self.patch_repo(&r, serde_json::json!({"isDisabled": false}))
                .await?;
        }
        if let Some(b) = &s.default_branch
            && format!("refs/heads/{b}") != r.default_branch
        {
            self.patch_repo(
                &r,
                serde_json::json!({"defaultBranch": format!("refs/heads/{b}")}),
            )
            .await?;
        }
        if s.archived == Some(true) && !r.is_disabled {
            self.patch_repo(&r, serde_json::json!({"isDisabled": true}))
                .await?;
        }
        Ok(())
    }

    /// Has nowhere to put them.
    async fn set_topics(&self, _name: &str, _topics: &[String]) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError> {
        let items = self.refs(name, "heads/").await?;
        Ok(items
            .into_iter()
            .map(|r| Ref {
                name: r
                    .name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&r.name)
                    .to_string(),
                sha: r.object_id,
            })
            .collect())
    }

    async fn delete_branch(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        self.delete_ref(name, &format!("refs/heads/{branch}")).await
    }

    async fn delete_tag(&self, name: &str, tag: &str) -> Result<(), ForgeError> {
        self.delete_ref(name, &format!("refs/tags/{tag}")).await
    }

    /// Nothing to lift: force-push is a permission here, not a branch setting, and whoever
    /// may delete repositories has it.
    async fn allow_force_push(&self, _name: &str, _branch: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    /// Pages with `$skip` until a page comes back empty: a short page is not the last page
    /// when the server clamps `$top` below what was asked for.
    async fn open_requests(&self, name: &str) -> Result<Vec<Request>, ForgeError> {
        #[derive(Deserialize)]
        struct Pull {
            #[serde(rename = "pullRequestId")]
            id: i64,
            #[serde(default, deserialize_with = "super::null_default")]
            title: String,
        }
        let (project, name) = self.split(name)?;
        const PAGE: usize = 100;
        let mut out = Vec::new();
        let mut skip = 0;
        loop {
            let q = format!("searchCriteria.status=active&$top={PAGE}&$skip={skip}");
            let (_, page): (_, Option<Values<Pull>>) = self
                .json(
                    Method::GET,
                    &Self::git(
                        &project,
                        &format!("/repositories/{}/pullrequests", path_escape(&name)),
                    ),
                    &q,
                    None,
                )
                .await?;
            let page = page.map(|p| p.value).unwrap_or_default();
            if page.is_empty() {
                return Ok(out);
            }
            skip += page.len();
            out.extend(page.into_iter().map(|p| Request {
                number: p.id,
                title: p.title,
            }));
        }
    }

    /// Abandons. The id is unique across the project, not per repository, and is never reused.
    async fn close_request(&self, name: &str, number: i64) -> Result<(), ForgeError> {
        let (project, name) = self.split(name)?;
        self.call(
            Method::PATCH,
            &Self::git(
                &project,
                &format!("/repositories/{}/pullrequests/{number}", path_escape(&name)),
            ),
            "",
            Some(&serde_json::json!({"status": "abandoned"})),
        )
        .await
        .map(drop)
    }

    /// Any username works with a PAT as the password. The token travels as a header, never in
    /// the URL.
    fn git_remote(&self, name: &str) -> Result<GitRemote, ForgeError> {
        let (project, name) = self.split(name)?;
        let mut u = url::Url::parse(&self.base_url)
            .map_err(|e| ForgeError::msg(format!("parse base URL {:?}: {e}", self.base_url)))?;
        u.path_segments_mut()
            .map_err(|_| {
                ForgeError::msg(format!("base URL {:?} cannot take a path", self.base_url))
            })?
            .pop_if_empty()
            .extend([self.org.as_str(), project.as_str(), "_git", name.as_str()]);
        Ok(GitRemote {
            url: u,
            auth: Some(GitAuth {
                username: "forgelab".into(),
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    type Handler = Box<dyn Fn(&http::Request<Bytes>) -> http::Response<Bytes> + Send + Sync>;

    const REPOS: &str = "/acme/sandbox/_apis/git/repositories";

    fn serve(h: Handler) -> (Client, Arc<ScriptedTransport>) {
        let t = ScriptedTransport::new(move |r| {
            let auth = r
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert_eq!(auth, "Basic OnMzY3JldA==", "no PAT on {}", r.uri().path());
            assert!(
                r.uri().query().unwrap_or("").contains("api-version="),
                "no api-version on {}",
                r.uri().path()
            );
            h(r)
        });
        let c = Client::new(
            "http://ado.test",
            "acme",
            "sandbox",
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

    /// A direct GET comes first; on a 404 the listing tells a disabled repository from a
    /// missing one; and a repository whose refs are gone was deleted a moment ago, whatever
    /// the GET says.
    #[tokio::test(start_paused = true)]
    async fn get() {
        let lists = Arc::new(AtomicUsize::new(0));
        let l2 = lists.clone();
        let (c, _) = serve(Box::new(move |r| {
            let path = r.uri().path().to_string();
            match path.as_str() {
                p if p == format!("{REPOS}/svc") => reply(
                    200,
                    r#"{"id":"1","name":"svc","defaultBranch":"refs/heads/master"}"#,
                ),
                p if p == format!("{REPOS}/svc/refs") => reply(
                    200,
                    r#"{"value":[{"name":"refs/heads/master","objectId":"abc"},{"name":"refs/heads/feature/x","objectId":"def"}]}"#,
                ),
                p if p == format!("{REPOS}/bare") => reply(200, r#"{"id":"2","name":"bare"}"#),
                p if p == format!("{REPOS}/bare/refs") => reply(200, r#"{"value":[]}"#),
                // deleted a second ago: the GET still answers, the refs do not
                p if p == format!("{REPOS}/ghost") => reply(200, r#"{"id":"4","name":"ghost"}"#),
                REPOS => {
                    // "lagging" was disabled a moment ago and the listing has not caught up yet
                    let n = l2.fetch_add(1, Ordering::SeqCst) + 1;
                    reply(
                        200,
                        &format!(
                            r#"{{"value":[{{"id":"3","name":"Retired","isDisabled":true}},{{"id":"5","name":"lagging","isDisabled":{}}}]}}"#,
                            n > 2
                        ),
                    )
                }
                _ => reply(404, ""),
            }
        }));
        let r = c.get("svc").await.unwrap().expect("svc");
        assert!(
            r.default_branch == "master" && !r.archived && !r.empty,
            "{r:?}"
        );
        assert!(c.get("bare").await.unwrap().unwrap().empty);
        assert!(
            c.get("retired")
                .await
                .unwrap()
                .expect("a disabled repository is found through the listing")
                .archived
        );
        for name in ["missing", "ghost"] {
            assert!(c.get(name).await.unwrap().is_none(), "{name}");
        }
        lists.store(0, Ordering::SeqCst);
        let r = c.get("lagging").await.unwrap().expect("lagging");
        assert!(r.archived);
        assert_eq!(
            lists.load(Ordering::SeqCst),
            3,
            "a stale listing is asked again"
        );
    }

    /// A disabled repository refuses even its own deletion, and a deletion only reaches the
    /// recycle bin: enable, delete, purge.
    #[tokio::test(start_paused = true)]
    async fn delete_enables_then_purges() {
        let (c, t) = serve(Box::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("GET", REPOS) => reply(
                200,
                r#"{"value":[{"id":"42","name":"svc","isDisabled":true}]}"#,
            ),
            ("GET", _) => reply(404, ""), // disabled: unreadable
            _ => reply(200, ""),
        }));
        c.delete("svc").await.unwrap();
        assert_eq!(
            t.seen(),
            [
                format!("GET {REPOS}/svc"),
                format!("GET {REPOS}"),
                format!("PATCH {REPOS}/42"),
                format!("DELETE {REPOS}/42"),
                "DELETE /acme/sandbox/_apis/git/recycleBin/repositories/42".to_string()
            ]
        );
    }

    /// The GET answered from a cache for a repository deleted a moment ago; the DELETE then
    /// says 404. That is "already gone", not a failure that stops a destroy.
    #[tokio::test(start_paused = true)]
    async fn delete_of_a_repository_that_just_went_is_not_an_error() {
        let (c, t) = serve(Box::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("GET", p) if p == format!("{REPOS}/svc") => reply(200, r#"{"id":"42","name":"svc"}"#),
            ("DELETE", _) => reply(
                404,
                r#"{"message":"TF401019: The Git repository with name or identifier 42 does not exist"}"#,
            ),
            _ => reply(404, ""),
        }));
        c.delete("svc").await.unwrap();
        assert_eq!(t.seen().last().unwrap(), &format!("DELETE {REPOS}/42"));
        // And one the lookup no longer sees at all.
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        c.delete("svc").await.unwrap();
    }

    /// Disabled rejects every other write: enable first, disable last, and skip what already
    /// holds.
    #[tokio::test(start_paused = true)]
    async fn update_settings_order() {
        let patches = Arc::new(Mutex::new(Vec::<String>::new()));
        let p2 = patches.clone();
        let (c, _) = serve(Box::new(move |r| match r.method().as_str() {
            "GET" => {
                if r.uri().path() != REPOS {
                    return reply(404, "");
                }
                reply(
                    200,
                    r#"{"value":[{"id":"1","name":"svc","defaultBranch":"refs/heads/main","isDisabled":true}]}"#,
                )
            }
            "PATCH" => {
                let body = body_json(r);
                let mut p = p2.lock().unwrap();
                for (k, v) in body.as_object().unwrap() {
                    p.push(format!("{k}={v}"));
                }
                reply(200, "")
            }
            _ => reply(200, ""),
        }));
        c.update_settings(
            "svc",
            Settings {
                default_branch: Some("master".into()),
                archived: Some(false),
                ..Default::default()
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
            patches.lock().unwrap().join(", "),
            "isDisabled=false, defaultBranch=\"refs/heads/master\"",
            "already disabled, so no third one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delete_branch_and_close_request() {
        let body = Arc::new(Mutex::new(serde_json::Value::Null));
        let b2 = body.clone();
        let (c, t) = serve(Box::new(move |r| {
            if r.method() == Method::GET {
                // the filter is a prefix match: run-42-rollback must not be mistaken for run-42
                return reply(
                    200,
                    r#"{"value":[{"name":"refs/heads/campaign/run-42-rollback","objectId":"bbb"},{"name":"refs/heads/campaign/run-42","objectId":"aaa"}]}"#,
                );
            }
            *b2.lock().unwrap() = body_json(r);
            reply(
                200,
                r#"{"value":[{"success":true,"updateStatus":"succeeded"}]}"#,
            )
        }));
        c.delete_branch("svc", "campaign/run-42").await.unwrap();
        {
            let update = body.lock().unwrap()[0].clone();
            assert!(
                update["name"] == "refs/heads/campaign/run-42"
                    && update["oldObjectId"] == "aaa"
                    && update["newObjectId"] == ZERO_SHA,
                "ref update: {update}"
            );
        }
        c.close_request("svc", 7).await.unwrap();
        assert_eq!(
            t.seen().last().unwrap(),
            &format!("PATCH {REPOS}/svc/pullrequests/7")
        );
        assert_eq!(body.lock().unwrap()["status"], "abandoned");
    }

    #[test]
    fn caps_and_git_remote() {
        let t = ScriptedTransport::new(|_| reply(200, ""));
        let c = Client::new(
            "",
            "acme",
            "my sandbox",
            "s3cret".to_string().into(),
            t,
            CancellationToken::new(),
        )
        .unwrap();
        let caps = c.caps();
        assert!(
            !caps.topics && !caps.visibility && caps.archived_unreadable,
            "{caps:?}"
        );
        let remote = c.git_remote("svc").unwrap();
        assert_eq!(
            remote.url.as_str(),
            "https://dev.azure.com/acme/my%20sandbox/_git/svc"
        );
        let auth = remote.auth.unwrap();
        assert_eq!(auth.username, "forgelab");
        assert_eq!(auth.secret.expose_secret(), "s3cret");
    }

    /// A namespace's first segment is a project and the rest joins the name. The project is
    /// made on the way to the first repository that needs it, and that is waited for.
    #[tokio::test(start_paused = true)]
    async fn create_makes_project() {
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let polls = Arc::new(AtomicUsize::new(0));
        let (b2, p2) = (bodies.clone(), polls.clone());
        let (c, t) = serve(Box::new(move |r| {
            let mut bodies = b2.lock().unwrap();
            match (r.method().as_str(), r.uri().path()) {
                ("POST", _) => {
                    bodies.push(body_json(r));
                    reply(200, r#"{"id":"op1","status":"queued"}"#)
                }
                ("GET", "/acme/_apis/projects/platform") if bodies.is_empty() => reply(404, ""),
                ("GET", "/acme/_apis/projects/platform") => reply(200, r#"{"id":"p9"}"#),
                ("GET", "/acme/_apis/process/processes") => reply(
                    200,
                    r#"{"value":[{"id":"scrum"},{"id":"agile","isDefault":true}]}"#,
                ),
                ("GET", "/acme/_apis/operations/op1") => {
                    if p2.fetch_add(1, Ordering::SeqCst) == 0 {
                        return reply(200, r#"{"id":"op1","status":"inProgress"}"#);
                    }
                    reply(200, r#"{"id":"op1","status":"succeeded"}"#)
                }
                (m, p) => panic!("unexpected {m} {p}"),
            }
        }));
        c.create("platform/core/api", "", "", &[], "forgelab-managed")
            .await
            .unwrap();
        c.create("platform/tooling", "", "", &[], "forgelab-managed")
            .await
            .unwrap();
        {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 3, "{bodies:?}\n{:?}", t.seen());
            assert_eq!(polls.load(Ordering::SeqCst), 2);
            let caps = &bodies[0]["capabilities"];
            assert!(
                bodies[0]["name"] == "platform"
                    && bodies[0]["description"] == NAMESPACE_MARKER
                    && caps["processTemplate"]["templateTypeId"] == "agile"
                    && caps["versioncontrol"]["sourceControlType"] == "Git",
                "project: {}",
                bodies[0]
            );
            assert!(
                bodies[1]["name"] == "core-api"
                    && bodies[1]["project"]["id"] == "p9"
                    && bodies[2]["name"] == "tooling",
                "repositories: {:?}",
                &bodies[1..]
            );
            assert_eq!(
                t.seen().last().unwrap(),
                "POST /acme/platform/_apis/git/repositories"
            );
        }

        assert!(
            c.git_remote("platform/core/api")
                .unwrap()
                .url
                .as_str()
                .ends_with("/acme/platform/_git/core-api")
        );
        // sandbox/x and x would be the same repository
        let err = c.get("Sandbox/x").await.unwrap_err();
        assert!(err.to_string().contains("own project"), "{err}");
    }

    /// A project operation that fails without a reason is tried again, and the reason, when
    /// Azure DevOps gives one, is reported. Seen for real: three projects requested at once,
    /// one of them failed with an empty result message.
    #[tokio::test(start_paused = true)]
    async fn a_failed_project_operation_is_tried_again() {
        let posts = Arc::new(AtomicUsize::new(0));
        let p2 = posts.clone();
        let (c, _t) = serve(Box::new(move |r| {
            let made = p2.load(Ordering::SeqCst);
            match (r.method().as_str(), r.uri().path()) {
                ("POST", "/acme/_apis/projects") => {
                    let n = p2.fetch_add(1, Ordering::SeqCst) + 1;
                    reply(200, &format!(r#"{{"id":"op{n}","status":"queued"}}"#))
                }
                ("POST", _) => reply(201, "{}"),
                ("GET", "/acme/_apis/projects/services") if made < 2 => reply(404, ""),
                ("GET", "/acme/_apis/projects/services") => reply(200, r#"{"id":"p1"}"#),
                ("GET", "/acme/_apis/process/processes") => {
                    reply(200, r#"{"value":[{"id":"agile","isDefault":true}]}"#)
                }
                ("GET", "/acme/_apis/operations/op1") => {
                    reply(200, r#"{"id":"op1","status":"failed","resultMessage":""}"#)
                }
                ("GET", "/acme/_apis/operations/op2") => {
                    reply(200, r#"{"id":"op2","status":"succeeded"}"#)
                }
                (m, p) => panic!("unexpected {m} {p}"),
            }
        }));
        c.create("services/api", "", "", &[], "forgelab-managed")
            .await
            .unwrap();
        assert_eq!(
            posts.load(Ordering::SeqCst),
            2,
            "the failed operation is tried once more"
        );

        // Three failures in a row are reported, with Azure DevOps' own words when it has any.
        let (c, _t) = serve(Box::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("POST", "/acme/_apis/projects") => reply(200, r#"{"id":"op9","status":"queued"}"#),
            ("GET", "/acme/_apis/projects/services") => reply(404, ""),
            ("GET", "/acme/_apis/process/processes") => {
                reply(200, r#"{"value":[{"id":"agile","isDefault":true}]}"#)
            }
            ("GET", "/acme/_apis/operations/op9") => reply(
                200,
                r#"{"id":"op9","status":"failed","resultMessage":"","detailedMessage":"TF200019: name in use"}"#,
            ),
            (m, p) => panic!("unexpected {m} {p}"),
        }));
        let err = c
            .create("services/api", "", "", &[], "forgelab-managed")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("operation failed: TF200019: name in use"),
            "{err}"
        );
    }

    /// Eight repositories in one new project make it once, and none of them waits on a lock
    /// while another project is made.
    #[tokio::test(start_paused = true)]
    async fn concurrent_creates_make_a_project_once() {
        let made = Arc::new(AtomicUsize::new(0));
        let m2 = made.clone();
        let (c, _) = serve(Box::new(move |r| {
            match (r.method().as_str(), r.uri().path()) {
                ("POST", "/acme/_apis/projects") => {
                    m2.fetch_add(1, Ordering::SeqCst);
                    reply(200, r#"{"id":"op1","status":"queued"}"#)
                }
                ("POST", _) => reply(200, ""),
                ("GET", "/acme/_apis/projects/platform") if m2.load(Ordering::SeqCst) == 0 => {
                    reply(404, "")
                }
                ("GET", "/acme/_apis/projects/platform") => reply(200, r#"{"id":"p9"}"#),
                ("GET", "/acme/_apis/process/processes") => {
                    reply(200, r#"{"value":[{"id":"agile","isDefault":true}]}"#)
                }
                ("GET", "/acme/_apis/operations/op1") => {
                    reply(200, r#"{"id":"op1","status":"succeeded"}"#)
                }
                _ => reply(404, ""),
            }
        }));
        let c = Arc::new(c);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..8 {
            let c = c.clone();
            tasks.spawn(async move {
                c.create(&format!("platform/svc{i}"), "", "", &[], "forgelab-managed")
                    .await
            });
        }
        while let Some(r) = tasks.join_next().await {
            r.unwrap().unwrap();
        }
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }

    /// Before its project exists, a repository is simply missing.
    #[tokio::test(start_paused = true)]
    async fn get_in_missing_project() {
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        assert!(c.get("platform/api").await.unwrap().is_none());
    }

    /// The marker alone decides: what a marked project holds goes with it, as a marked
    /// repository's branches and requests do.
    #[tokio::test(start_paused = true)]
    async fn delete_namespace() {
        for (name, project, want) in [
            (
                "ours",
                r#"{"id":"p9","description":"forgelab-managed"}"#,
                Removal::Removed,
            ),
            (
                "somebody else's",
                r#"{"id":"p9","description":"Platform team"}"#,
                Removal::Kept("not created by forgelab".into()),
            ),
        ] {
            let deleted = Arc::new(AtomicBool::new(false));
            let d2 = deleted.clone();
            let (c, _) = serve(Box::new(move |r| {
                match (r.method().as_str(), r.uri().path()) {
                    ("DELETE", "/acme/_apis/projects/p9") => {
                        d2.store(true, Ordering::SeqCst);
                        reply(200, r#"{"id":"op1"}"#)
                    }
                    (_, "/acme/_apis/projects/platform") => reply(200, project),
                    (_, "/acme/_apis/operations/op1") => {
                        reply(200, r#"{"id":"op1","status":"succeeded"}"#)
                    }
                    // in particular, not the repositories: what it holds is nobody's business
                    (m, p) => panic!("{name}: unexpected {m} {p}"),
                }
            }));
            assert_eq!(
                c.delete_namespace("platform").await.unwrap(),
                want,
                "{name}"
            );
            assert_eq!(
                deleted.load(Ordering::SeqCst),
                want == Removal::Removed,
                "{name}"
            );
        }

        // Deeper than a project there is nothing to remove; "" is the default project.
        let (c, t) = serve(Box::new(|_| reply(404, "")));
        for ns in ["platform/core", "", "gone"] {
            assert_eq!(
                c.delete_namespace(ns).await.unwrap(),
                Removal::Absent,
                "{ns}"
            );
        }
        assert_eq!(
            t.seen(),
            [
                "GET /acme/_apis/projects/sandbox",
                "GET /acme/_apis/projects/gone"
            ],
            "only projects are looked up"
        );
    }

    /// Only a project made a moment ago is known to hold nothing but its born-with repository.
    #[tokio::test(start_paused = true)]
    async fn create_conflict_in_existing_project() {
        let (c, _) = serve(Box::new(|r| {
            if r.method() == Method::POST {
                return reply(409, "exists");
            }
            reply(200, r#"{"id":"p9"}"#)
        }));
        c.create("platform/platform", "", "", &[], "forgelab-managed")
            .await
            .expect_err("a conflict in a project that was already there must surface");
    }

    /// The default project is made like any other, by the first repository without a
    /// namespace.
    #[tokio::test(start_paused = true)]
    async fn create_makes_default_project() {
        let posts = Arc::new(Mutex::new(Vec::<String>::new()));
        let p2 = posts.clone();
        let (c, _) = serve(Box::new(move |r| {
            let mut posts = p2.lock().unwrap();
            match (r.method().as_str(), r.uri().path()) {
                ("POST", p) => {
                    posts.push(p.to_string());
                    reply(200, r#"{"id":"op1"}"#)
                }
                ("GET", "/acme/_apis/projects/sandbox") if posts.is_empty() => reply(404, ""),
                ("GET", "/acme/_apis/projects/sandbox") => reply(200, r#"{"id":"p1"}"#),
                ("GET", "/acme/_apis/process/processes") => {
                    reply(200, r#"{"value":[{"id":"agile","isDefault":true}]}"#)
                }
                ("GET", "/acme/_apis/operations/op1") => {
                    reply(200, r#"{"id":"op1","status":"succeeded"}"#)
                }
                _ => reply(404, ""),
            }
        }));
        assert!(
            c.get("dotfiles").await.unwrap().is_none(),
            "before its project exists a repository is missing"
        );
        c.create("dotfiles", "", "", &[], "forgelab-managed")
            .await
            .unwrap();
        assert_eq!(
            *posts.lock().unwrap(),
            [
                "/acme/_apis/projects",
                "/acme/sandbox/_apis/git/repositories"
            ]
        );
    }

    /// A short page is not the last page: only an empty one is.
    #[tokio::test(start_paused = true)]
    async fn open_requests_page_until_empty() {
        let (c, t) = serve(Box::new(|r| {
            let q = r.uri().query().unwrap_or("");
            if q.contains("$skip=0") {
                return reply(
                    200,
                    r#"{"value":[{"pullRequestId":1,"title":"a"},{"pullRequestId":2,"title":"b"}]}"#,
                );
            }
            if q.contains("$skip=2") {
                return reply(200, r#"{"value":[{"pullRequestId":3,"title":"c"}]}"#);
            }
            reply(200, r#"{"value":[]}"#)
        }));
        let reqs = c.open_requests("svc").await.unwrap();
        assert_eq!(reqs.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(t.seen().len(), 3);
    }

    /// Refs follow the continuation token.
    #[tokio::test(start_paused = true)]
    async fn refs_follow_the_continuation_token() {
        let (c, _) = serve(Box::new(|r| {
            if r.uri().query().unwrap_or("").contains("continuationToken=") {
                return reply(200, r#"{"value":[{"name":"refs/heads/b","objectId":"2"}]}"#);
            }
            http::Response::builder()
                .status(200)
                .header("x-ms-continuationtoken", "t1")
                .body(Bytes::from(
                    r#"{"value":[{"name":"refs/heads/a","objectId":"1"}]}"#,
                ))
                .unwrap()
        }));
        let branches = c.branches("svc").await.unwrap();
        assert_eq!(
            branches,
            [
                Ref {
                    name: "a".into(),
                    sha: "1".into()
                },
                Ref {
                    name: "b".into(),
                    sha: "2".into()
                }
            ]
        );
    }

    /// An expired or wrong PAT is answered with a 203 and a sign-in page, not a 401.
    #[tokio::test(start_paused = true)]
    async fn a_sign_in_page_is_an_auth_failure() {
        let (c, _) = serve(Box::new(|_| reply(203, "<html>Sign in</html>")));
        let err = c.ensure_org().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "GET /_apis/projects: not authenticated: the token is wrong, expired, or not valid for organisation \"acme\""
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_org_names_a_missing_organisation() {
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        let err = c.ensure_org().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "organisation \"acme\" does not exist, or the token cannot see it"
        );
    }
}
