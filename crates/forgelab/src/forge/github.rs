//! `Forge` against the GitHub REST API (github.com and GitHub Enterprise Server).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::error::ForgeError;
use super::http::{HttpClient, RequestOpts, Transport, WriteLane};
use super::{
    Caps, Forge, ForgePolicy, GitAuth, GitRemote, NamespaceDepth, Ref, Removal, Repo, Request,
    Settings, classify, escape_ref, flat_name, path_escape,
};

/// Used when a sandbox names no base_url.
pub const DEFAULT_BASE_URL: &str = "https://github.com";

/// Talks to one organisation.
pub struct Client {
    /// https://api.github.com, or <base>/api/v3 on Enterprise Server.
    api_url: String,
    /// https://github.com
    git_url: String,
    org: String,
    token: SecretString,
    http: HttpClient,
}

impl Client {
    /// `base_url` is the web address, e.g. https://github.com.
    ///
    /// GitHub asks that mutating requests be made serially, not concurrently; bursts of them
    /// are what trips the secondary rate limit. Reads stay concurrent; writes go through a
    /// paced lane.
    pub fn new(
        base_url: &str,
        org: &str,
        token: SecretString,
        transport: Arc<dyn Transport>,
        cancel: CancellationToken,
    ) -> Result<Client, ForgeError> {
        let base_url = if base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            base_url
        };
        let u = url::Url::parse(base_url)
            .ok()
            .filter(|u| !u.scheme().is_empty() && u.host_str().is_some());
        let Some(u) = u else {
            return Err(ForgeError::msg(format!(
                "base URL {base_url:?} has no scheme or host"
            )));
        };
        let base = base_url.trim_end_matches('/').to_string();
        let api = if u.host_str() == Some("github.com") {
            "https://api.github.com".to_string()
        } else {
            format!("{base}/api/v3")
        };
        Ok(Self::with_api(api, base, org, token, transport, cancel))
    }

    fn with_api(
        api_url: String,
        git_url: String,
        org: &str,
        token: SecretString,
        transport: Arc<dyn Transport>,
        cancel: CancellationToken,
    ) -> Client {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
            .unwrap_or_else(|_| HeaderValue::from_static("Bearer"));
        auth.set_sensitive(true);
        headers.insert("authorization", auth);
        let http = HttpClient::new("GitHub", transport, classify::github, headers)
            .with_write_lane(WriteLane::new(Duration::from_secs(1)))
            .with_cancel(cancel);
        Client {
            api_url,
            git_url,
            org: org.to_string(),
            token,
            http,
        }
    }

    /// The API root in use, for tests.
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.api_url)
        }
    }

    fn repo_path(&self, name: &str) -> String {
        format!(
            "/repos/{}/{}",
            path_escape(&self.org),
            path_escape(&flat_name(name))
        )
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(HeaderMap, Option<T>), ForgeError> {
        self.http
            .json(method, &self.url(path), body, RequestOpts::default())
            .await
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

    /// Follows `Link: rel="next"` until there is none. Only the Link header is authoritative:
    /// a short page is not the last page, and a full page is not a promise of another.
    async fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, ForgeError> {
        let sep = if path.contains('?') { "&" } else { "?" };
        let mut target = format!("{path}{sep}per_page=100");
        let mut all = Vec::new();
        loop {
            let (headers, items): (_, Option<Vec<T>>) =
                self.json(Method::GET, &target, None).await?;
            all.extend(items.unwrap_or_default());
            match headers
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(next_link)
            {
                Some(next) => target = next,
                None => return Ok(all),
            }
        }
    }

    /// Reads the git database directly rather than the branch and tag listings: it is the
    /// source those are derived from, and it answers the same way for both kinds of ref.
    async fn refs(&self, name: &str, kind: &str) -> Result<Vec<Ref>, ForgeError> {
        #[derive(Deserialize)]
        struct Object {
            sha: String,
        }
        #[derive(Deserialize)]
        struct RawRef {
            #[serde(rename = "ref")]
            name: String,
            object: Object,
        }
        let prefix = format!("refs/{kind}/");
        let items: Vec<RawRef> = match self
            .list(&format!(
                "{}/git/matching-refs/{kind}/",
                self.repo_path(name)
            ))
            .await
        {
            Ok(items) => items,
            // A repository with no commits has no git database to read.
            Err(e) if e.is_status(&[409]) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(items
            .into_iter()
            .map(|r| Ref {
                name: r.name.strip_prefix(&prefix).unwrap_or(&r.name).to_string(),
                sha: r.object.sha,
            })
            .collect())
    }
}

/// The URL in a `Link` header carrying `rel="next"`, if any.
fn next_link(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim();
        let Some((url, params)) = part.split_once('>') else {
            continue;
        };
        let Some(url) = url.strip_prefix('<') else {
            continue;
        };
        if params.split(';').any(|p| p.trim() == "rel=\"next\"") {
            return Some(url.to_string());
        }
    }
    None
}

#[derive(Deserialize, Default)]
struct Protection {
    #[serde(default)]
    required_status_checks: Option<StatusChecks>,
    #[serde(default)]
    enforce_admins: Option<Enabled>,
    #[serde(default)]
    required_pull_request_reviews: Option<serde_json::Value>,
    #[serde(default)]
    restrictions: Option<Restrictions>,
    #[serde(default)]
    allow_force_pushes: Option<Enabled>,
}

#[derive(Deserialize, Default)]
struct Enabled {
    #[serde(default)]
    enabled: bool,
}

#[derive(Deserialize, Default)]
struct StatusChecks {
    #[serde(default)]
    strict: bool,
    #[serde(default, deserialize_with = "super::null_default")]
    contexts: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Restrictions {
    #[serde(default, deserialize_with = "super::null_default")]
    users: Vec<Login>,
    #[serde(default, deserialize_with = "super::null_default")]
    teams: Vec<Slug>,
    #[serde(default, deserialize_with = "super::null_default")]
    apps: Vec<Slug>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Slug {
    slug: String,
}

#[derive(Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "super::null_default")]
    ruleset_source_type: String,
    #[serde(default, deserialize_with = "super::null_default")]
    ruleset_source: String,
    #[serde(default)]
    ruleset_id: Option<i64>,
}

#[async_trait]
impl Forge for Client {
    fn name(&self) -> &'static str {
        "GitHub"
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
            default_concurrency: 6,
            namespace_gone_timeout: Duration::from_secs(300),
        }
    }

    /// Only checks: a GitHub organisation cannot be created through the API.
    async fn ensure_org(&self) -> Result<(), ForgeError> {
        match self
            .call(
                Method::GET,
                &format!("/orgs/{}", path_escape(&self.org)),
                None,
            )
            .await
        {
            Err(e) if e.is_not_found() => Err(ForgeError::msg(format!(
                "organisation {:?} does not exist, or the token cannot see it",
                self.org
            ))),
            other => other,
        }
    }

    async fn get(&self, name: &str) -> Result<Option<Repo>, ForgeError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default, deserialize_with = "super::null_default")]
            name: String,
            #[serde(default, deserialize_with = "super::null_default")]
            default_branch: String,
            #[serde(default)]
            private: bool,
            #[serde(default)]
            archived: bool,
            #[serde(default, deserialize_with = "super::null_default")]
            topics: Vec<String>,
        }
        let raw: Raw = match self.json(Method::GET, &self.repo_path(name), None).await {
            Ok((_, Some(raw))) => raw,
            Ok((_, None)) => return Ok(None),
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        // GitHub redirects a renamed repository's old name to it. That is not the repository
        // that was asked for.
        if !raw.name.eq_ignore_ascii_case(&flat_name(name)) {
            return Ok(None);
        }
        // The repository object does not say whether it has commits (`size` lags), so ask. A
        // 404 here, after the repository itself answered, is one deleted a moment ago and still
        // served from a cache: gone, not an error.
        let branches = match self.branches(name).await {
            Ok(b) => b,
            Err(e) if e.is_status(&[404]) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(Repo {
            name: raw.name,
            default_branch: raw.default_branch,
            visibility: if raw.private { "private" } else { "public" }.to_string(),
            archived: raw.archived,
            empty: branches.is_empty(),
            topics: raw.topics,
        }))
    }

    /// Ignores `default_branch`: GitHub takes none at creation. The first branch pushed
    /// becomes the default, and `update_settings` pins it afterwards.
    async fn create(
        &self,
        name: &str,
        visibility: &str,
        _default_branch: &str,
        topics: &[String],
    ) -> Result<(), ForgeError> {
        let body = serde_json::json!({
            "name": flat_name(name),
            "private": visibility == "private",
            "auto_init": false,
        });
        self.http
            .call(
                Method::POST,
                &self.url(&format!("/orgs/{}/repos", path_escape(&self.org))),
                Some(&body),
                RequestOpts {
                    idempotent: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        if let Err(e) = self.set_topics(name, topics).await {
            // Created a moment ago by this very call, so removing it loses nothing -- and
            // leaving it would strand an unmarked repository that apply refuses to adopt.
            if let Err(derr) = self.delete(name).await {
                return Err(ForgeError::msg(format!(
                    "set topics: {e} (and the new repository could not be removed: {derr})"
                )));
            }
            return Err(ForgeError::msg(format!("set topics: {e}")));
        }
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), ForgeError> {
        match self.call(Method::DELETE, &self.repo_path(name), None).await {
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) if e.is_status(&[403]) => Err(ForgeError::msg(format!(
                "{e} (deleting needs the delete_repo scope, or Administration: write on a fine-grained token)"
            ))),
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
        let body = serde_json::json!({ "names": topics });
        self.call(
            Method::PUT,
            &format!("{}/topics", self.repo_path(name)),
            Some(&body),
        )
        .await
    }

    async fn branches(&self, name: &str) -> Result<Vec<Ref>, ForgeError> {
        self.refs(name, "heads").await
    }

    async fn delete_branch(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!(
                "{}/git/refs/heads/{}",
                self.repo_path(name),
                escape_ref(branch)
            ),
            None,
        )
        .await
    }

    async fn delete_tag(&self, name: &str, tag: &str) -> Result<(), ForgeError> {
        self.call(
            Method::DELETE,
            &format!("{}/git/refs/tags/{}", self.repo_path(name), escape_ref(tag)),
            None,
        )
        .await
    }

    /// Lifts what stands in the way of a force-push, best effort: classic branch protection
    /// is re-put with `allow_force_pushes` on and everything else as it was, and a repository
    /// ruleset that forbids non-fast-forward pushes is disabled. An organisation ruleset is
    /// out of this token's reach and is reported.
    async fn allow_force_push(&self, name: &str, branch: &str) -> Result<(), ForgeError> {
        let repo = self.repo_path(name);
        let protection = format!("{repo}/branches/{}/protection", escape_ref(branch));
        match self
            .json::<Protection>(Method::GET, &protection, None)
            .await
        {
            Ok((_, Some(p))) => {
                let allowed = p
                    .allow_force_pushes
                    .as_ref()
                    .map(|e| e.enabled)
                    .unwrap_or(false);
                if !allowed {
                    let body = serde_json::json!({
                        "required_status_checks": p.required_status_checks.as_ref().map(|c| serde_json::json!({"strict": c.strict, "contexts": c.contexts})),
                        "enforce_admins": p.enforce_admins.as_ref().map(|e| e.enabled).unwrap_or(false),
                        "required_pull_request_reviews": p.required_pull_request_reviews.as_ref().map(reviews_for_put),
                        "restrictions": p.restrictions.as_ref().map(|r| serde_json::json!({
                            "users": r.users.iter().map(|u| u.login.clone()).collect::<Vec<_>>(),
                            "teams": r.teams.iter().map(|t| t.slug.clone()).collect::<Vec<_>>(),
                            "apps": r.apps.iter().map(|a| a.slug.clone()).collect::<Vec<_>>(),
                        })),
                        "allow_force_pushes": true,
                    });
                    self.call(Method::PUT, &protection, Some(&body)).await?;
                }
            }
            Ok((_, None)) => {}
            // Not protected -- or protection is not available on this plan, which comes to
            // the same thing.
            Err(e) if e.is_not_found() || e.is_status(&[403]) => {
                tracing::debug!("{name}: branch {branch} has no protection to lift: {e}");
            }
            Err(e) => return Err(e),
        }

        let rules: Vec<Rule> = match self
            .json(
                Method::GET,
                &format!("{repo}/rules/branches/{}", escape_ref(branch)),
                None,
            )
            .await
        {
            Ok((_, rules)) => rules.unwrap_or_default(),
            Err(e) if e.is_not_found() || e.is_status(&[403]) => Vec::new(),
            Err(e) => return Err(e),
        };
        for rule in rules.iter().filter(|r| r.kind == "non_fast_forward") {
            match (rule.ruleset_source_type.as_str(), rule.ruleset_id) {
                ("Repository", Some(id)) => {
                    self.call(
                        Method::PUT,
                        &format!("{repo}/rulesets/{id}"),
                        Some(&serde_json::json!({"enforcement": "disabled"})),
                    )
                    .await?;
                }
                _ => {
                    return Err(ForgeError::msg(format!(
                        "branch {branch} of {name} is protected by organisation ruleset {:?}: forgelab cannot lift it",
                        rule.ruleset_source
                    )));
                }
            }
        }
        Ok(())
    }

    async fn open_requests(&self, name: &str) -> Result<Vec<Request>, ForgeError> {
        #[derive(Deserialize)]
        struct Pull {
            number: i64,
            #[serde(default, deserialize_with = "super::null_default")]
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

    /// Closes; GitHub cannot delete a pull request, and its number is never reused.
    async fn close_request(&self, name: &str, number: i64) -> Result<(), ForgeError> {
        self.call(
            Method::PATCH,
            &format!("{}/pulls/{number}", self.repo_path(name)),
            Some(&serde_json::json!({"state": "closed"})),
        )
        .await
    }

    /// GitHub accepts a token as the password for any username; x-access-token is the
    /// conventional one. The token travels as a header, never in the URL.
    fn git_remote(&self, name: &str) -> Result<GitRemote, ForgeError> {
        let mut u = url::Url::parse(&self.git_url)
            .map_err(|e| ForgeError::msg(format!("parse base URL {:?}: {e}", self.git_url)))?;
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
                username: "x-access-token".into(),
                secret: self.token.clone(),
            }),
        })
    }
}

/// The review settings as the PUT wants them, from the object the GET returned.
fn reviews_for_put(v: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for key in [
        "dismiss_stale_reviews",
        "require_code_owner_reviews",
        "required_approving_review_count",
        "require_last_push_approval",
    ] {
        if let Some(x) = v.get(key) {
            out.insert(key.into(), x.clone());
        }
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::http::ScriptedTransport;
    use bytes::Bytes;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    type Handler = Box<dyn Fn(&http::Request<Bytes>) -> http::Response<Bytes> + Send + Sync>;

    fn serve(h: Handler) -> (Client, Arc<ScriptedTransport>) {
        let t = ScriptedTransport::new(move |r| h(r));
        let c = Client::with_api(
            "http://api.test".into(),
            "https://github.com".into(),
            "acme-sandbox",
            "s3cret".to_string().into(),
            t.clone(),
            CancellationToken::new(),
        );
        (c, t)
    }

    fn reply(status: u16, body: &str) -> http::Response<Bytes> {
        ScriptedTransport::reply(status, body)
    }

    fn body_json(r: &http::Request<Bytes>) -> serde_json::Value {
        serde_json::from_slice(r.body()).unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn new_picks_the_api_root() {
        let t = ScriptedTransport::new(|_| reply(200, ""));
        for (base, want) in [
            ("", "https://api.github.com"),
            ("https://github.com/", "https://api.github.com"),
            (
                "https://ghe.example.test",
                "https://ghe.example.test/api/v3",
            ),
        ] {
            let c = Client::new(
                base,
                "o",
                "t".to_string().into(),
                t.clone(),
                CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(c.api_url(), want, "{base}");
        }
        assert!(
            Client::new(
                "github.com",
                "o",
                "t".to_string().into(),
                t,
                CancellationToken::new()
            )
            .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn get() {
        let (c, _) = serve(Box::new(|r| match r.uri().path() {
            "/repos/acme-sandbox/svc" => reply(
                200,
                r#"{"name":"svc","default_branch":"main","private":true,"archived":true,"topics":["a","forgelab-managed"]}"#,
            ),
            "/repos/acme-sandbox/svc/git/matching-refs/heads/" => reply(
                200,
                r#"[{"ref":"refs/heads/main","object":{"sha":"abc"}},{"ref":"refs/heads/feature/x","object":{"sha":"def"}}]"#,
            ),
            // the old name of a renamed repository answers with the repository it became
            "/repos/acme-sandbox/old-name" => {
                reply(200, r#"{"name":"new-name","default_branch":"main"}"#)
            }
            "/repos/acme-sandbox/bare" => reply(
                200,
                r#"{"name":"bare","default_branch":"main","private":false}"#,
            ),
            "/repos/acme-sandbox/bare/git/matching-refs/heads/" => {
                reply(409, r#"{"message":"Git Repository is empty."}"#)
            }
            _ => reply(404, "not found"),
        }));
        let r = c.get("svc").await.unwrap().expect("svc found");
        assert!(
            r.visibility == "private" && r.archived && !r.empty && r.topics.len() == 2,
            "{r:?}"
        );
        assert!(c.get("missing").await.unwrap().is_none());
        assert!(
            c.get("old-name").await.unwrap().is_none(),
            "a renamed repository must read as missing"
        );
        let r = c.get("bare").await.unwrap().expect("bare found");
        assert!(r.empty && r.visibility == "public", "{r:?}");
        let branches = c.branches("svc").await.unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches[1],
            Ref {
                name: "feature/x".into(),
                sha: "def".into()
            }
        );
    }

    /// Only the Link header says whether there is another page.
    #[tokio::test(start_paused = true)]
    async fn list_follows_link_header() {
        let (c, _) = serve(Box::new(|r| {
            if r.uri().query().unwrap_or("").contains("page=2") {
                return reply(200, r#"[{"number":3,"title":"c"}]"#);
            }
            http::Response::builder()
                .status(200)
                .header("Link", r#"<http://api.test/repos/acme-sandbox/svc/pulls?state=open&per_page=100&page=2>; rel="next", <x>; rel="last""#)
                .body(Bytes::from(r#"[{"number":1,"title":"a"},{"number":2,"title":"b"}]"#))
                .unwrap()
        }));
        let reqs = c.open_requests("svc").await.unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[2].number, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_secondary_limit_is_waited_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let (c, _) = serve(Box::new(move |_| {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                return http::Response::builder()
                    .status(403)
                    .header("Retry-After", "2")
                    .body(Bytes::from(
                        r#"{"message":"You have exceeded a secondary rate limit"}"#,
                    ))
                    .unwrap();
            }
            reply(200, "")
        }));
        let started = tokio::time::Instant::now();
        c.set_topics("svc", &[]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_hourly_limit_is_an_error_not_an_hours_sleep() {
        let reset = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 40 * 60)
            .to_string();
        let (c, t) = serve(Box::new(move |_| {
            http::Response::builder()
                .status(403)
                .header("X-RateLimit-Remaining", "0")
                .header("X-RateLimit-Reset", &reset)
                .body(Bytes::from(r#"{"message":"API rate limit exceeded"}"#))
                .unwrap()
        }));
        let err = c.set_topics("svc", &[]).await.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");
        assert_eq!(t.seen().len(), 1);
    }

    /// The regression, and the shape the headers cannot describe. A secondary limit often
    /// arrives with no Retry-After, and X-RateLimit-Remaining reports the *primary* budget,
    /// which it leaves untouched -- so by the headers alone this is indistinguishable from
    /// having no permission. Only the body says what it is. Reading it as a refusal is what
    /// stopped an apply 27 repositories into a fleet of 108.
    #[tokio::test(start_paused = true)]
    async fn a_secondary_limit_with_nothing_but_a_body_is_still_waited_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let (c, _) = serve(Box::new(move |_| {
            if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                return http::Response::builder()
                    .status(403)
                    .header("X-RateLimit-Remaining", "4264") // not zero: the hourly budget is fine
                    .body(Bytes::from(r#"{"message":"You have exceeded a secondary rate limit and have been temporarily blocked from content creation."}"#))
                    .unwrap();
            }
            reply(200, "")
        }));
        let started = tokio::time::Instant::now();
        c.set_topics("svc", &[])
            .await
            .expect("a secondary limit is a pause, not a refusal");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() >= Duration::from_secs(60),
            "want one minute-long pause and a retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_403_is_not_retried() {
        let (c, t) = serve(Box::new(|_| {
            reply(
                403,
                r#"{"message":"Must have admin rights to Repository."}"#,
            )
        }));
        let err = c.delete("svc").await.unwrap_err();
        assert_eq!(t.seen().len(), 1);
        assert!(err.to_string().contains("delete_repo"), "{err}");
    }

    /// Create has to leave the repository marked. GitHub takes no topics at creation, so
    /// they are set next -- and if that fails, what was just created is removed rather than
    /// stranded.
    #[tokio::test(start_paused = true)]
    async fn create_sets_topics_or_rolls_back() {
        let fail = Arc::new(AtomicBool::new(false));
        let f2 = fail.clone();
        let (c, t) = serve(Box::new(move |r| {
            if f2.load(Ordering::SeqCst) && r.uri().path().ends_with("/topics") {
                return reply(422, r#"{"message":"nope"}"#);
            }
            reply(200, "")
        }));
        c.create("svc", "private", "main", &["forgelab-managed".into()])
            .await
            .unwrap();
        assert_eq!(
            t.seen(),
            [
                "POST /orgs/acme-sandbox/repos",
                "PUT /repos/acme-sandbox/svc/topics"
            ]
        );

        fail.store(true, Ordering::SeqCst);
        t.reset();
        c.create("svc", "private", "main", &["forgelab-managed".into()])
            .await
            .expect_err("want an error when the topics cannot be set");
        assert_eq!(
            t.seen().last().unwrap(),
            "DELETE /repos/acme-sandbox/svc",
            "the unmarked repository must be removed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn requests_and_git_remote() {
        #[derive(Default)]
        struct Got {
            method: String,
            path: String,
            auth: String,
            body: serde_json::Value,
        }
        let got = Arc::new(Mutex::new(Got::default()));
        let g2 = got.clone();
        let (c, _) = serve(Box::new(move |r| {
            let mut g = g2.lock().unwrap();
            g.method = r.method().to_string();
            g.path = r.uri().path().to_string();
            g.auth = r
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            g.body = body_json(r);
            reply(200, "")
        }));

        c.update_settings(
            "svc",
            Settings {
                visibility: Some("private".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        {
            let g = got.lock().unwrap();
            assert!(
                g.method == "PATCH"
                    && g.path == "/repos/acme-sandbox/svc"
                    && g.body["private"] == true
                    && g.auth == "Bearer s3cret",
                "{} {} {}",
                g.method,
                g.path,
                g.body
            );
        }
        c.set_topics("svc", &["a".into()]).await.unwrap();
        {
            let g = got.lock().unwrap();
            assert!(
                g.method == "PUT"
                    && g.path == "/repos/acme-sandbox/svc/topics"
                    && g.body["names"].is_array()
            );
        }
        c.delete_branch("svc", "feature/run 42").await.unwrap();
        {
            let g = got.lock().unwrap();
            assert!(
                g.method == "DELETE"
                    && g.path == "/repos/acme-sandbox/svc/git/refs/heads/feature/run%2042",
                "{}",
                g.path
            );
        }
        c.close_request("svc", 7).await.unwrap();
        {
            let g = got.lock().unwrap();
            assert!(
                g.method == "PATCH"
                    && g.path == "/repos/acme-sandbox/svc/pulls/7"
                    && g.body["state"] == "closed"
            );
        }

        let remote = c.git_remote("svc").unwrap();
        assert_eq!(
            remote.url.as_str(),
            "https://github.com/acme-sandbox/svc.git"
        );
        let auth = remote.auth.unwrap();
        assert_eq!(auth.username, "x-access-token");
        assert_eq!(auth.secret.expose_secret(), "s3cret");

        // There is nowhere to put a namespace, so it is joined into the name.
        c.set_topics("platform/core/api", &[]).await.unwrap();
        assert_eq!(
            got.lock().unwrap().path,
            "/repos/acme-sandbox/platform-core-api/topics"
        );
        assert!(
            c.git_remote("platform/core/api")
                .unwrap()
                .url
                .as_str()
                .ends_with("/acme-sandbox/platform-core-api.git")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delete_of_a_missing_repository_is_not_an_error() {
        let (c, _) = serve(Box::new(|_| reply(404, r#"{"message":"Not Found"}"#)));
        c.delete("gone").await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_org_names_a_missing_organisation() {
        let (c, _) = serve(Box::new(|_| reply(404, "")));
        let err = c.ensure_org().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "organisation \"acme-sandbox\" does not exist, or the token cannot see it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn allow_force_push_lifts_classic_protection_and_repo_rulesets() {
        let puts = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let p2 = puts.clone();
        let (c, t) = serve(Box::new(move |r| {
            let path = r.uri().path().to_string();
            match (r.method().as_str(), path.as_str()) {
                ("GET", "/repos/acme-sandbox/svc/branches/main/protection") => reply(
                    200,
                    r#"{"required_status_checks":{"strict":true,"contexts":["ci"]},"enforce_admins":{"enabled":true},"required_pull_request_reviews":{"dismiss_stale_reviews":true,"required_approving_review_count":1},"restrictions":null,"allow_force_pushes":{"enabled":false}}"#,
                ),
                ("GET", "/repos/acme-sandbox/svc/rules/branches/main") => reply(
                    200,
                    r#"[{"type":"deletion","ruleset_source_type":"Repository","ruleset_id":1},{"type":"non_fast_forward","ruleset_source_type":"Repository","ruleset_source":"svc","ruleset_id":5}]"#,
                ),
                ("PUT", _) => {
                    p2.lock().unwrap().push((path, body_json(r)));
                    reply(200, "")
                }
                _ => reply(404, ""),
            }
        }));
        c.allow_force_push("svc", "main").await.unwrap();
        let puts = puts.lock().unwrap();
        assert_eq!(puts.len(), 2, "{:?}", t.seen());
        assert_eq!(
            puts[0].0,
            "/repos/acme-sandbox/svc/branches/main/protection"
        );
        assert_eq!(puts[0].1["allow_force_pushes"], true);
        assert_eq!(puts[0].1["enforce_admins"], true);
        assert_eq!(puts[0].1["required_status_checks"]["contexts"][0], "ci");
        assert_eq!(
            puts[0].1["required_pull_request_reviews"]["required_approving_review_count"],
            1
        );
        assert!(puts[0].1["restrictions"].is_null());
        assert_eq!(puts[1].0, "/repos/acme-sandbox/svc/rulesets/5");
        assert_eq!(puts[1].1["enforcement"], "disabled");
    }

    #[tokio::test(start_paused = true)]
    async fn allow_force_push_is_a_no_op_without_protection_and_refuses_org_rulesets() {
        let (c, t) = serve(Box::new(|r| match r.uri().path() {
            "/repos/acme-sandbox/svc/rules/branches/main" => reply(200, "[]"),
            _ => reply(404, r#"{"message":"Branch not protected"}"#),
        }));
        c.allow_force_push("svc", "main").await.unwrap();
        assert!(
            t.seen().iter().all(|s| s.starts_with("GET ")),
            "{:?}",
            t.seen()
        );

        let (c, _) = serve(Box::new(|r| match r.uri().path() {
            "/repos/acme-sandbox/svc/branches/main/protection" => {
                reply(200, r#"{"allow_force_pushes":{"enabled":true}}"#)
            }
            "/repos/acme-sandbox/svc/rules/branches/main" => reply(
                200,
                r#"[{"type":"non_fast_forward","ruleset_source_type":"Organization","ruleset_source":"acme","ruleset_id":9}]"#,
            ),
            _ => reply(404, ""),
        }));
        let err = c.allow_force_push("svc", "main").await.unwrap_err();
        assert!(
            err.to_string().contains("organisation ruleset \"acme\""),
            "{err}"
        );
    }

    #[test]
    fn next_link_parses() {
        assert_eq!(
            next_link(r#"<https://a/b?page=2>; rel="next", <https://a/b?page=5>; rel="last""#),
            Some("https://a/b?page=2".into())
        );
        assert_eq!(
            next_link(r#"<https://a/b?page=1>; rel="prev", <https://a/b?page=1>; rel="first""#),
            None
        );
        assert_eq!(next_link(""), None);
    }
}
