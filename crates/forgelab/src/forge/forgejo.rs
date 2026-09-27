//! `Forge` against the Forgejo (and Gitea) API.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::error::ForgeError;
use super::http::{HttpClient, RequestOpts, Transport};
use super::{
    Caps, Forge, ForgePolicy, GitAuth, GitRemote, NamespaceDepth, Ref, Removal, Repo, Request,
    Settings, classify, escape_ref, flat_name, path_escape,
};

/// Talks to one organisation on one Forgejo instance.
pub struct Client {
    base_url: String,
    org: String,
    token: SecretString,
    http: HttpClient,
}

impl Client {
    pub fn new(
        base_url: &str,
        org: &str,
        token: SecretString,
        transport: Arc<dyn Transport>,
        cancel: CancellationToken,
    ) -> Result<Client, ForgeError> {
        let u = url::Url::parse(base_url)
            .map_err(|_| ForgeError::msg(format!("base URL {base_url:?} has no scheme or host")))?;
        if u.host_str().is_none() {
            return Err(ForgeError::msg(format!(
                "base URL {base_url:?} has no scheme or host"
            )));
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("token {}", token.expose_secret()))
                .map_err(|_| ForgeError::msg("the token is not a valid header value"))?,
        );
        let http =
            HttpClient::new("Forgejo", transport, classify::forgejo, headers).with_cancel(cancel);
        Ok(Client {
            base_url: base_url.trim_end_matches('/').to_string(),
            org: org.to_string(),
            token,
            http,
        })
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base_url)
    }

    fn repo_path(&self, name: &str) -> String {
        format!(
            "/repos/{}/{}",
            path_escape(&self.org),
            path_escape(&flat_name(name))
        )
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, ForgeError> {
        let (_, v) = self
            .http
            .json::<T>(
                Method::GET,
                &self.api(path),
                None::<&()>,
                RequestOpts::default(),
            )
            .await?;
        Ok(v)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(), ForgeError> {
        self.http
            .call(method, &self.api(path), body, RequestOpts::default())
            .await
            .map(drop)
    }

    /// Pages through a collection. It stops on X-Total-Count or an empty page, never on a
    /// short page: the server clamps `limit` to its own maximum, so a page shorter than the
    /// one asked for is not evidence of being the last. A server that ignored `page` would
    /// answer the same page forever, so a page that repeats the previous one also ends it.
    async fn list<T: DeserializeOwned + serde::Serialize>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, ForgeError> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut all: Vec<T> = Vec::new();
        let mut previous_first: Option<String> = None;
        for page in 1..=10_000u32 {
            let url = self.api(&format!("{path}{sep}page={page}&limit=50"));
            let (hdr, items) = self
                .http
                .json::<Vec<T>>(Method::GET, &url, None::<&()>, RequestOpts::default())
                .await?;
            let items = items.unwrap_or_default();
            if items.is_empty() {
                return Ok(all);
            }
            let first = serde_json::to_string(&items[0]).unwrap_or_default();
            if previous_first.as_deref() == Some(first.as_str()) {
                return Ok(all); // the server ignored `page`
            }
            previous_first = Some(first);
            all.extend(items);
            if let Some(total) = hdr
                .get("x-total-count")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
                && all.len() >= total
            {
                return Ok(all);
            }
        }
        Ok(all)
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct BranchProtection {
    #[serde(default)]
    rule_name: String,
    #[serde(default)]
    branch_name: String,
    #[serde(default)]
    enable_push: bool,
    #[serde(default)]
    enable_force_push: bool,
}

impl BranchProtection {
    fn name(&self) -> &str {
        if self.rule_name.is_empty() {
            &self.branch_name
        } else {
            &self.rule_name
        }
    }

    /// Whether this rule governs `branch`. A rule name is a glob; `*` and `?` are what
    /// fixtures use.
    fn covers(&self, branch: &str) -> bool {
        glob_match(self.name(), branch)
    }
}

/// A small glob: `*` matches any run of characters (slashes included, as Forgejo's does),
/// `?` one character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[async_trait]
impl Forge for Client {
    fn name(&self) -> &'static str {
        "Forgejo"
    }

    /// Everything forgelab declares has a home here.
    fn caps(&self) -> Caps {
        Caps {
            topics: true,
            visibility: true,
            archived_unreadable: false,
            namespace_depth: NamespaceDepth::None,
        }
    }

    fn policy(&self) -> ForgePolicy {
        ForgePolicy {
            default_concurrency: 8,
            namespace_gone_timeout: Duration::from_secs(60),
        }
    }

    async fn ensure_org(&self) -> Result<(), ForgeError> {
        match self
            .call(
                Method::GET,
                &format!("/orgs/{}", path_escape(&self.org)),
                None,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if e.is_status(&[404]) => {
                self.call(
                    Method::POST,
                    "/orgs",
                    Some(&serde_json::json!({"username": self.org, "visibility": "public"})),
                )
                .await
            }
            Err(e) => Err(e),
        }
    }

    async fn get(&self, name: &str) -> Result<Option<Repo>, ForgeError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            name: String,
            #[serde(default)]
            default_branch: String,
            #[serde(default)]
            private: bool,
            #[serde(default)]
            archived: bool,
            #[serde(default)]
            empty: bool,
        }
        let raw: Raw = match self.get_json(&self.repo_path(name)).await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) if e.is_status(&[404]) => return Ok(None),
            Err(e) => return Err(e),
        };
        // A renamed repository's old name redirects to it. That is not the repository asked for.
        if !raw.name.eq_ignore_ascii_case(&flat_name(name)) {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Topics {
            #[serde(default)]
            topics: Vec<String>,
        }
        // A forge may answer the repository from a cache for a moment after it was deleted;
        // the second read then finds nothing. That is a repository that is gone, not an error.
        let topics: Topics = match self
            .get_json(&format!("{}/topics", self.repo_path(name)))
            .await
        {
            Ok(t) => t.unwrap_or(Topics { topics: vec![] }),
            Err(e) if e.is_status(&[404]) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(Repo {
            name: raw.name,
            default_branch: raw.default_branch,
            visibility: if raw.private { "private" } else { "public" }.to_string(),
            archived: raw.archived,
            empty: raw.empty,
            topics: topics.topics,
        }))
    }

    async fn create(
        &self,
        name: &str,
        visibility: &str,
        default_branch: &str,
        topics: &[String],
    ) -> Result<(), ForgeError> {
        self.call(
            Method::POST,
            &format!("/orgs/{}/repos", path_escape(&self.org)),
            Some(&serde_json::json!({
                "name": flat_name(name),
                "auto_init": false,
                "default_branch": default_branch,
                "private": visibility == "private",
            })),
        )
        .await?;
        if let Err(e) = self.set_topics(name, topics).await {
            // Created a moment ago by this very call, so removing it loses nothing -- and leaving
            // it would strand an unmarked repository that apply refuses to adopt.
            return match self.delete(name).await {
                Ok(()) => Err(ForgeError::msg(format!("set topics: {e}"))),
                Err(derr) => Err(ForgeError::msg(format!(
                    "set topics: {e} (and the new repository could not be removed: {derr})"
                ))),
            };
        }
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), ForgeError> {
        match self.call(Method::DELETE, &self.repo_path(name), None).await {
            Err(e) if e.is_status(&[404]) => Ok(()),
            other => other,
        }
    }

    /// Nothing to remove: a namespace is only a prefix of the name here.
    async fn delete_namespace(&self, _ns: &str) -> Result<Removal, ForgeError> {
        Ok(Removal::Absent)
    }

    async fn update_settings(&self, name: &str, s: Settings) -> Result<(), ForgeError> {
        let mut fields = serde_json::Map::new();
        if let Some(b) = s.default_branch {
            fields.insert("default_branch".into(), b.into());
        }
        if let Some(v) = s.visibility {
            fields.insert("private".into(), (v == "private").into());
        }
        if let Some(a) = s.archived {
            fields.insert("archived".into(), a.into());
        }
        if fields.is_empty() {
            return Ok(());
        }
        self.call(
            Method::PATCH,
            &self.repo_path(name),
            Some(&serde_json::Value::Object(fields)),
        )
        .await
    }

    async fn set_topics(&self, name: &str, topics: &[String]) -> Result<(), ForgeError> {
        self.call(
            Method::PUT,
            &format!("{}/topics", self.repo_path(name)),
            Some(&serde_json::json!({"topics": topics})),
        )
        .await
    }

    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError> {
        #[derive(Deserialize, serde::Serialize)]
        struct Branch {
            name: String,
            commit: Commit,
        }
        #[derive(Deserialize, serde::Serialize)]
        struct Commit {
            id: String,
        }
        let items: Vec<Branch> = self
            .list(&format!("{}/branches", self.repo_path(name)))
            .await?;
        Ok(items
            .into_iter()
            .map(|b| Ref {
                name: b.name,
                sha: b.commit.id,
            })
            .collect())
    }

    async fn delete_branch(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!("{}/branches/{}", self.repo_path(name), escape_ref(branch)),
            None,
        )
        .await
    }

    async fn delete_tag(&self, name: &str, tag: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!("{}/tags/{}", self.repo_path(name), escape_ref(tag)),
            None,
        )
        .await
    }

    /// Lifts whatever branch protection covers `branch`. Forgejo's rules are named globs; a
    /// rule that denies force-pushes is asked to allow them, and one whose Forgejo is too old
    /// to know the setting is removed instead, since a rule that cannot be relaxed only stands
    /// in the way of a reset.
    async fn allow_force_push(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        let base = format!("{}/branch_protections", self.repo_path(name));
        let rules: Vec<BranchProtection> = match self.get_json(&base).await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(e) if e.is_status(&[404]) => return Ok(()),
            Err(e) => return Err(e),
        };
        for rule in rules.iter().filter(|r| r.covers(branch)) {
            if rule.enable_push && rule.enable_force_push {
                continue;
            }
            let path = format!("{base}/{}", path_escape(rule.name()));
            self.call(
                Method::PATCH,
                &path,
                Some(&serde_json::json!({"enable_push": true, "enable_force_push": true, "enable_force_push_allowlist": false})),
            )
            .await?;
            let after: Option<BranchProtection> = self.get_json(&path).await?;
            if !after
                .map(|r| r.enable_push && r.enable_force_push)
                .unwrap_or(true)
            {
                tracing::debug!(
                    rule = rule.name(),
                    "forgejo: rule cannot allow force-pushes, removing it"
                );
                self.call(Method::DELETE, &path, None).await?;
            }
        }
        Ok(())
    }

    async fn open_requests(&self, name: &str) -> Result<Vec<Request>, ForgeError> {
        #[derive(Deserialize, serde::Serialize)]
        struct Pull {
            number: i64,
            #[serde(default)]
            title: String,
        }
        let items: Vec<Pull> = self
            .list(&format!("{}/pulls?state=open", self.repo_path(name)))
            .await?;
        Ok(items
            .into_iter()
            .map(|p| Request {
                number: p.number,
                title: p.title,
            })
            .collect())
    }

    async fn close_request(&self, name: &str, number: i64) -> Result<(), ForgeError> {
        self.call(
            Method::PATCH,
            &format!("{}/pulls/{number}", self.repo_path(name)),
            Some(&serde_json::json!({"state": "closed"})),
        )
        .await
    }

    /// Forgejo identifies the user from the token, so the username is a placeholder.
    fn git_remote(&self, name: &str) -> Result<GitRemote, ForgeError> {
        let mut u = url::Url::parse(&self.base_url)
            .map_err(|e| ForgeError::msg(format!("parse base URL {:?}: {e}", self.base_url)))?;
        let path = format!(
            "{}/{}/{}.git",
            u.path().trim_end_matches('/'),
            self.org,
            flat_name(name)
        );
        u.set_path(&path);
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
    use crate::forge::ScriptedTransport;
    use bytes::Bytes;

    fn client(t: Arc<ScriptedTransport>) -> Client {
        Client::new(
            "http://forgejo.test",
            "acme",
            "s3cret".to_string().into(),
            t,
            CancellationToken::new(),
        )
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn get_reads_topics_and_spots_renames() {
        let t = ScriptedTransport::new(|r| {
            assert_eq!(r.headers()["authorization"], "token s3cret");
            match r.uri().path() {
                "/api/v1/repos/acme/svc" => ScriptedTransport::reply(
                    200,
                    r#"{"name":"svc","default_branch":"main","private":true,"archived":true,"empty":false}"#,
                ),
                "/api/v1/repos/acme/svc/topics" => {
                    ScriptedTransport::reply(200, r#"{"topics":["a","forgelab-managed"]}"#)
                }
                "/api/v1/repos/acme/old-name" => {
                    ScriptedTransport::reply(200, r#"{"name":"new-name"}"#)
                }
                "/api/v1/repos/acme/platform-core-api" => ScriptedTransport::reply(
                    200,
                    r#"{"name":"platform-core-api","private":false,"empty":true}"#,
                ),
                "/api/v1/repos/acme/platform-core-api/topics" => {
                    ScriptedTransport::reply(200, r#"{"topics":[]}"#)
                }
                _ => ScriptedTransport::reply(404, "{}"),
            }
        });
        let c = client(t);
        let r = c.get("svc").await.unwrap().unwrap();
        assert_eq!(
            (r.visibility.as_str(), r.archived, r.empty, r.topics.len()),
            ("private", true, false, 2)
        );
        assert!(c.get("missing").await.unwrap().is_none());
        assert!(
            c.get("old-name").await.unwrap().is_none(),
            "a renamed repository must read as missing"
        );
        let r = c.get("platform/core/api").await.unwrap().unwrap();
        assert!(r.empty && r.visibility == "public");
    }

    #[tokio::test(start_paused = true)]
    async fn list_stops_on_total_count_or_an_empty_page_never_on_a_short_one() {
        let t = ScriptedTransport::new(|r| {
            let q = r.uri().query().unwrap_or("");
            let page = q
                .split('&')
                .find_map(|kv| kv.strip_prefix("page="))
                .unwrap();
            let body = match page {
                "1" => r#"[{"name":"a","commit":{"id":"1"}}]"#,
                "2" => r#"[{"name":"b","commit":{"id":"2"}}]"#,
                _ => "[]",
            };
            ScriptedTransport::reply(200, body)
        });
        let c = client(t.clone());
        assert_eq!(c.branches("svc").await.unwrap().len(), 2);
        assert_eq!(
            t.seen().len(),
            3,
            "short pages are not the last page; only an empty one is"
        );

        let t = ScriptedTransport::new(|_| {
            http::Response::builder()
                .status(200)
                .header("X-Total-Count", "1")
                .body(Bytes::from(r#"[{"name":"a","commit":{"id":"1"}}]"#))
                .unwrap()
        });
        let c = client(t.clone());
        assert_eq!(c.branches("svc").await.unwrap().len(), 1);
        assert_eq!(t.seen().len(), 1, "X-Total-Count ends the listing");

        // A server that ignores `page` answers the same page forever.
        let t = ScriptedTransport::new(|_| {
            ScriptedTransport::reply(200, r#"[{"name":"a","commit":{"id":"1"}}]"#)
        });
        let c = client(t.clone());
        assert_eq!(c.branches("svc").await.unwrap().len(), 1);
        assert_eq!(t.seen().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn create_rolls_back_when_the_marker_cannot_be_set() {
        let t = ScriptedTransport::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("POST", "/api/v1/orgs/acme/repos") => ScriptedTransport::reply(201, "{}"),
            ("PUT", "/api/v1/repos/acme/svc/topics") => {
                ScriptedTransport::reply(422, r#"{"message":"topics"}"#)
            }
            ("DELETE", "/api/v1/repos/acme/svc") => ScriptedTransport::reply(204, ""),
            _ => ScriptedTransport::reply(404, ""),
        });
        let c = client(t.clone());
        let err = c
            .create("svc", "private", "main", &["forgelab-managed".into()])
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("set topics: "), "{err}");
        assert_eq!(
            t.seen(),
            [
                "POST /api/v1/orgs/acme/repos",
                "PUT /api/v1/repos/acme/svc/topics",
                "DELETE /api/v1/repos/acme/svc"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delete_of_a_missing_repository_is_not_an_error() {
        let t = ScriptedTransport::new(|_| ScriptedTransport::reply(404, ""));
        client(t).delete("gone").await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn allow_force_push_relaxes_the_covering_rule_or_removes_it() {
        let t = ScriptedTransport::new(|r| match (r.method().as_str(), r.uri().path()) {
            ("GET", "/api/v1/repos/acme/svc/branch_protections") => ScriptedTransport::reply(
                200,
                r#"[{"rule_name":"release/*","enable_push":true,"enable_force_push":false},{"rule_name":"main","enable_push":true,"enable_force_push":true}]"#,
            ),
            ("PATCH", "/api/v1/repos/acme/svc/branch_protections/release%2F*") => {
                ScriptedTransport::reply(200, "{}")
            }
            ("GET", "/api/v1/repos/acme/svc/branch_protections/release%2F*") => {
                ScriptedTransport::reply(
                    200,
                    r#"{"rule_name":"release/*","enable_push":true,"enable_force_push":true}"#,
                )
            }
            ("GET", "/api/v1/repos/acme/old/branch_protections") => {
                ScriptedTransport::reply(200, r#"[{"branch_name":"main","enable_push":true}]"#)
            }
            ("PATCH", "/api/v1/repos/acme/old/branch_protections/main") => {
                ScriptedTransport::reply(200, "{}")
            }
            ("GET", "/api/v1/repos/acme/old/branch_protections/main") => {
                ScriptedTransport::reply(200, r#"{"branch_name":"main","enable_push":true}"#)
            }
            ("DELETE", "/api/v1/repos/acme/old/branch_protections/main") => {
                ScriptedTransport::reply(204, "")
            }
            _ => ScriptedTransport::reply(404, ""),
        });
        let c = client(t.clone());
        c.allow_force_push("svc", "release/1").await.unwrap();
        c.allow_force_push("svc", "main").await.unwrap();
        c.allow_force_push("old", "main").await.unwrap();
        let seen = t.seen();
        assert!(
            seen.contains(
                &"PATCH /api/v1/repos/acme/svc/branch_protections/release%2F*".to_string()
            ),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"DELETE /api/v1/repos/acme/old/branch_protections/main".to_string()),
            "an old Forgejo's rule is removed: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|s| s.contains("branch_protections/main")
                    && s.starts_with("PATCH")
                    && s.contains("/svc/"))
                .count(),
            0
        );
    }

    #[test]
    fn globs() {
        assert!(glob_match("release/*", "release/1.2"));
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "maine"));
        assert!(glob_match("v?", "v1"));
    }

    #[test]
    fn remote_has_no_credential_in_the_url() {
        let t = ScriptedTransport::new(|_| ScriptedTransport::reply(404, ""));
        let r = client(t).git_remote("platform/core/api").unwrap();
        assert_eq!(
            r.url.as_str(),
            "http://forgejo.test/acme/platform-core-api.git"
        );
        assert_eq!(r.auth.as_ref().unwrap().username, "forgelab");
    }
}
