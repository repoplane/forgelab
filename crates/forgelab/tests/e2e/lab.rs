//! One Forgejo for the whole test binary, and a `Lab` per test: a private copy of the example
//! fleet and an organisation of its own.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use forgelab::forge::{ReqwestTransport, Transport, flat_name};
use forgelab::sandbox::{CommandError, Env, Options, Output};
use forgelab_faultproxy::{FaultEngine, FaultTransport, Rules};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// Forgejo 15.0.8 (LTS), pinned by digest rather than by tag so that an upstream retag
/// cannot silently change what the tests run against. Keep in step with compose.yaml.
const IMAGE: &str = "codeberg.org/forgejo/forgejo@sha256";
const DIGEST: &str = "0a2e377fd3c5af3451bfa1f44e6f198f322b6d5e03f04a028b8e672f1ccddc9f";

const ADMIN_USER: &str = "labadmin";
const ADMIN_PASSWORD: &str = "labadmin-not-a-secret";
const ADMIN_EMAIL: &str = "admin@forgelab.test";
const TOKEN_ENV: &str = "FORGELAB_TEST_TOKEN";

/// Keeps start-up cheap: the repository indexer in particular would otherwise index every
/// seeded repository.
///
/// The two SQLite settings are not tuning. Forgejo records a pushed branch from a
/// post-receive hook, and with the default rollback journal and 500ms busy timeout, eight
/// concurrent pushes can lose that write to "database is locked" -- after which the branch
/// never appears in the API, however long you wait. Keep in step with compose.yaml.
const CONFIG: &[(&str, &str)] = &[
    ("FORGEJO__database__DB_TYPE", "sqlite3"),
    ("FORGEJO__database__SQLITE_JOURNAL_MODE", "WAL"),
    ("FORGEJO__database__SQLITE_TIMEOUT", "20000"),
    ("FORGEJO__security__INSTALL_LOCK", "true"),
    (
        "FORGEJO__security__SECRET_KEY",
        "forgelab-test-not-a-secret",
    ),
    ("FORGEJO__repository__DEFAULT_BRANCH", "main"),
    ("FORGEJO__indexer__REPO_INDEXER_ENABLED", "false"),
    ("FORGEJO__actions__ENABLED", "false"),
    ("FORGEJO__cron__ENABLED", "false"),
    ("FORGEJO__mailer__ENABLED", "false"),
    ("FORGEJO__log__LEVEL", "warn"),
    ("FORGEJO__server__OFFLINE_MODE", "true"),
    ("FORGEJO__service__DISABLE_REGISTRATION", "true"),
];

pub struct Forgejo {
    pub base_url: String,
    pub token: String,
    _container: Container<GenericImage>,
}

static FORGEJO: OnceLock<Forgejo> = OnceLock::new();
static LAB_SEQ: AtomicU32 = AtomicU32::new(0);

/// Whether the end-to-end tests run at all.
pub fn e2e_enabled() -> bool {
    if std::env::var("FORGELAB_E2E")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return true;
    }
    eprintln!("skipping end-to-end tests: set FORGELAB_E2E=1 (they need Docker)");
    false
}

/// The one Forgejo for this test binary, started on first use. The blocking runner brings its
/// own runtime, so it is started on a plain thread rather than inside a test's.
pub fn forgejo() -> &'static Forgejo {
    FORGEJO.get_or_init(|| {
        std::thread::spawn(start_forgejo)
            .join()
            .expect("start forgejo")
    })
}

fn start_forgejo() -> Forgejo {
    let mut image = GenericImage::new(IMAGE, DIGEST)
        .with_exposed_port(3000.tcp())
        .with_wait_for(WaitFor::http(
            HttpWaitStrategy::new("/api/healthz")
                .with_port(3000.tcp())
                .with_expected_status_code(200u16),
        ))
        .with_startup_timeout(std::time::Duration::from_secs(120));
    for (k, v) in CONFIG {
        image = image.with_env_var(*k, *v);
    }
    let container = image.start().expect("start forgejo container");
    let port = container
        .get_host_port_ipv4(3000.tcp())
        .expect("mapped port");
    let base_url = format!("http://127.0.0.1:{port}");

    let mut res = container
        .exec(ExecCommand::new([
            "su",
            "git",
            "-c",
            &format!(
                "forgejo admin user create --username {ADMIN_USER} --password {ADMIN_PASSWORD} --email {ADMIN_EMAIL} --admin --must-change-password=false"
            ),
        ]))
        .expect("create admin");
    // The output is drained first: the exit code is only known once the command has ended.
    let out = String::from_utf8_lossy(&res.stdout_to_vec().unwrap_or_default()).to_string();
    let err = String::from_utf8_lossy(&res.stderr_to_vec().unwrap_or_default()).to_string();
    let code = res.exit_code().expect("exit code");
    if matches!(code, Some(c) if c != 0) || !out.contains("successfully created") {
        panic!("create admin: exit {code:?}\n{out}\n{err}");
    }

    let token = mint_token(&base_url);
    Forgejo {
        base_url,
        token,
        _container: container,
    }
}

/// Creates an API token using basic auth, which is how the first token is obtained when none
/// exists yet.
fn mint_token(base_url: &str) -> String {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("{base_url}/api/v1/users/{ADMIN_USER}/tokens"))
        .basic_auth(ADMIN_USER, Some(ADMIN_PASSWORD))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"forgelab","scopes":["write:organization","write:repository","write:user"]}"#)
        .send()
        .expect("mint token");
    let status = resp.status();
    let v: serde_json::Value = resp.json().expect("token json");
    let sha1 = v["sha1"].as_str().unwrap_or_default();
    assert!(!sha1.is_empty(), "mint token: {status}: {v}");
    sha1.to_string()
}

pub fn examples_fleet() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/fleet")
}

/// A `Write` that keeps what was written where a test can read it.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One test's sandbox: a private copy of the example fleet and an org of its own.
pub struct Lab {
    org: String,
    dir: tempfile::TempDir,
    fleet_subdir: Option<String>,
    out: Buffer,
    /// Every API request forgelab makes goes through here, so a test can assert on them and
    /// so faults can be injected. git traffic is a subprocess and is not seen here.
    transport: Arc<FaultTransport<ReqwestTransport>>,
    base_url: String,
    http: reqwest::Client,
    concurrency: Option<usize>,
}

impl Lab {
    /// A lab on the example fleet, with no faults.
    pub async fn new() -> Lab {
        Lab::with_rules(
            &examples_fleet(),
            Rules {
                seed: None,
                rules: vec![],
            },
            None,
        )
        .await
    }

    /// A lab on `fleet_dir`, with the given fault rules on every API request (git traffic goes
    /// straight to the forge). `base_url` overrides where the sandbox points, for a proxy.
    pub async fn with_rules(fleet_dir: &Path, rules: Rules, base_url: Option<String>) -> Lab {
        let f = forgejo();
        let dir = tempfile::tempdir().unwrap();
        copy_dir(fleet_dir, dir.path());
        let seed = std::env::var("FORGELAB_FAULT_SEED")
            .ok()
            .and_then(|s| s.parse().ok());
        let engine = FaultEngine::new(rules, seed);
        eprintln!("lab: FORGELAB_FAULT_SEED={}", engine.seed());
        let transport = FaultTransport::new(ReqwestTransport::new().unwrap(), engine);
        let base_url = base_url.unwrap_or_else(|| f.base_url.clone());
        let lab = Lab {
            // Numbered, not named after the test: Forgejo caps an org name at 40 characters.
            org: format!(
                "forgelab-sandbox-{}",
                LAB_SEQ.fetch_add(1, Ordering::SeqCst) + 1
            ),
            dir,
            fleet_subdir: None,
            out: Buffer::default(),
            transport,
            base_url,
            http: reqwest::Client::new(),
            concurrency: None,
        };
        lab.write_config();
        lab
    }

    #[allow(dead_code)]
    pub fn set_concurrency(&mut self, n: usize) {
        self.concurrency = Some(n);
    }

    fn write_config(&self) {
        let cfg = format!(
            "version: 1\nsandboxes:\n  test:\n    forge: forgejo\n    base_url: {}\n    org: {}\n    token_env: {TOKEN_ENV}\n",
            self.base_url, self.org
        );
        std::fs::write(self.dir.path().join("sandboxes.yaml"), cfg).unwrap();
    }

    /// Points the lab's config at another organisation.
    pub fn set_org(&mut self, org: &str) {
        self.org = org.to_string();
        self.write_config();
    }

    pub fn org(&self) -> &str {
        &self.org
    }

    /// The fleet directory.
    pub fn dir(&self) -> PathBuf {
        match &self.fleet_subdir {
            Some(s) => self.dir.path().join(s),
            None => self.dir.path().to_path_buf(),
        }
    }

    /// Opens the sandbox afresh, as each CLI invocation would.
    pub fn env(&self) -> Env {
        self.out.0.lock().unwrap().clear();
        let out: Output = Arc::new(Mutex::new(self.out.clone()));
        let transport: Arc<dyn Transport> = self.transport.clone();
        Env::open(Options {
            fleet_dir: self.dir().to_string_lossy().into_owned(),
            config_path: self
                .dir
                .path()
                .join("sandboxes.yaml")
                .to_string_lossy()
                .into_owned(),
            sandbox: "test".into(),
            yes: true,
            concurrency: self.concurrency,
            out: Some(out),
            transport: Some(transport),
            token: Some(forgejo().token.clone().into()),
            ..Options::default()
        })
        .expect("open")
    }

    /// What the last command printed.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.out.0.lock().unwrap()).into_owned()
    }

    pub async fn must_apply(&self) {
        if let Err(e) = self.env().apply().await {
            panic!("apply: {e}\n--- output:\n{}", self.out());
        }
    }

    /// Every API request forgelab made since the last `reset_seen`, as `METHOD /path`.
    pub fn seen(&self) -> Vec<String> {
        self.transport.seen()
    }

    pub fn reset_seen(&self) {
        self.transport.reset_seen();
    }

    #[allow(dead_code)]
    pub fn engine(&self) -> &Arc<FaultEngine> {
        self.transport.engine()
    }

    /// Calls the forge directly, as a test under way (or a person) would, and returns the
    /// status code. Always straight to the forge, never through a proxy.
    pub async fn api(&self, method: &str, path: &str, body: &str) -> u16 {
        self.json(method, path, body).await.0
    }

    /// Calls the forge and decodes the answer, returning the status code and the JSON (Null
    /// when there is none).
    pub async fn json(&self, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
        let f = forgejo();
        let req = self
            .http
            .request(
                method.parse().unwrap(),
                format!("{}/api/v1{path}", f.base_url),
            )
            .header("Authorization", format!("token {}", f.token))
            .header("Content-Type", "application/json")
            .body(body.to_string());
        let resp = req.send().await.expect("forge request");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let v = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    pub async fn must_api(&self, method: &str, path: &str, body: &str) {
        let (code, v) = self.json(method, path, body).await;
        assert!(
            (200..300).contains(&code),
            "{method} {path}: HTTP {code}: {v}"
        );
    }

    pub fn repo(&self, name: &str) -> String {
        format!("/repos/{}/{}", self.org, flat_name(name))
    }

    /// Makes the kinds of mess a test run leaves behind, across three repositories.
    pub async fn drift(&self) {
        // a feature branch with a commit, and an open pull request from it
        self.must_api(
            "POST",
            &format!("{}/contents/NEW.md", self.repo("billing-api")),
            r#"{"content":"aGVsbG8K","message":"change","new_branch":"feature/run-42"}"#,
        )
        .await;
        self.must_api(
            "POST",
            &format!("{}/pulls", self.repo("billing-api")),
            r#"{"head":"feature/run-42","base":"main","title":"run 42"}"#,
        )
        .await;
        // a direct commit to the default branch, and a stray tag
        self.must_api(
            "POST",
            &format!("{}/contents/HACK.md", self.repo("compliant")),
            r#"{"content":"aGFjawo=","message":"direct"}"#,
        )
        .await;
        self.must_api(
            "POST",
            &format!("{}/tags", self.repo("compliant")),
            r#"{"tag_name":"stray","target":"main"}"#,
        )
        .await;
        // settings
        self.must_api(
            "PUT",
            &format!("{}/topics", self.repo("parser-svc")),
            &format!(
                r#"{{"topics":[{:?},"changed"]}}"#,
                forgelab::sandbox::DEFAULT_MARKER_TOPIC
            ),
        )
        .await;
    }

    pub async fn drift2(&self, name: &str) {
        self.must_api(
            "POST",
            &format!("{}/contents/X.md", self.repo(name)),
            r#"{"content":"eAo=","message":"x"}"#,
        )
        .await;
    }
}

/// The distinction the CLI's exit code carries: reset-and-retry on drift, stop on a guard
/// failure.
pub fn exit_kind(r: &Result<(), CommandError>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(CommandError::Guard(_)) => "guard",
        Err(CommandError::Drift(_)) => "drift",
        Err(_) => "error",
    }
}

pub fn copy_dir(src: &Path, dst: &Path) {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
