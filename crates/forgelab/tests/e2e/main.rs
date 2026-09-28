//! forgelab end to end against a throwaway Forgejo container. This is the only place
//! testcontainers is used, and only from tests, so the CLI never links a container runtime.
//!
//! Gated: the tests run when `FORGELAB_E2E=1` (they need Docker) and print a skip line
//! otherwise, so that `cargo test` stays usable without Docker.

mod lab;
mod scale_faults;

use std::time::Duration;

use forgelab::fleet;
use forgelab::sandbox::CommandError;
use forgelab::seed;

use lab::{Lab, e2e_enabled, exit_kind};

fn want_guard(err: Result<(), CommandError>, contains: &str) {
    match &err {
        Err(CommandError::Guard(m)) if m.contains(contains) => {}
        other => panic!("want a guard failure containing {contains:?}, got: {other:?}"),
    }
}

/// The whole loop: plan, apply, verify, drift, verify, reset, verify, destroy, apply -- and
/// the lock comes out byte-identical, to itself and to the committed example.
#[tokio::test]
async fn walk() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    let lock_path = l.dir().join(fleet::LOCK_FILE);
    std::fs::remove_file(&lock_path).unwrap(); // the example ships one; start without it

    l.env().plan().await.unwrap();
    let out = l.out();
    assert_eq!(
        out.matches("  + ").count(),
        15,
        "plan: want 15 creates:\n{out}"
    );
    assert!(
        !lock_path.exists(),
        "plan wrote the lock; it must not write anything"
    );
    want_guard(l.env().verify().await, "no fleet.lock.json");

    l.must_apply().await;
    let first = std::fs::read(&lock_path).unwrap();
    l.env().verify().await.expect("verify after apply");
    l.env().plan().await.unwrap();
    assert!(
        l.out().contains("nothing to do"),
        "plan after apply:\n{}",
        l.out()
    );

    l.drift().await;
    let err = l.env().verify().await;
    assert_eq!(exit_kind(&err), "drift", "verify after drift: {err:?}");
    let msg = err.unwrap_err().to_string();
    for want in [
        "billing-api: extra branch feature/run-42; 1 open request(s)",
        "compliant: extra tag stray; main is at",
        "parser-svc: topics are [changed], want [service]",
    ] {
        assert!(
            msg.contains(want),
            "verify did not name the drift {want:?}:\n{msg}"
        );
    }

    l.env().reset().await.expect("reset");
    assert!(
        l.out().contains("3 of 15 repositories reset"),
        "reset output:\n{}",
        l.out()
    );
    l.env().verify().await.expect("verify after reset");

    l.env().destroy().await.expect("destroy");
    l.must_apply().await;
    let second = std::fs::read(&lock_path).unwrap();
    assert_eq!(
        first, second,
        "the lock changed across destroy + apply: seeding is not deterministic"
    );
    let committed = std::fs::read(lab::examples_fleet().join(fleet::LOCK_FILE)).unwrap();
    assert_eq!(
        committed, second,
        "examples/fleet/fleet.lock.json is stale: run `make lock` and commit it"
    );
}

/// The path a pull-request-driven consumer actually takes: open a pull request, merge it,
/// then roll it back with a second merged pull request. Nobody force-pushed or touched main
/// directly, yet main is two merges ahead -- and reset has to rewind it.
#[tokio::test]
async fn merged_pull_requests_are_rewound() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    let repo = l.repo("billing-api");

    // merge retries: a forge computes mergeability after the pull request is opened, and
    // answers "try again later" until it has.
    let merge = |number: i64| {
        let l = &l;
        let repo = repo.clone();
        async move {
            let mut code = 0;
            for _ in 0..40 {
                code = l
                    .api(
                        "POST",
                        &format!("{repo}/pulls/{number}/merge"),
                        r#"{"Do":"merge"}"#,
                    )
                    .await;
                if code == 200 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("merge #{number}: HTTP {code}");
        }
    };
    let open = |branch: &str, title: &str| {
        let l = &l;
        let repo = repo.clone();
        let branch = branch.to_string();
        let body = format!(r#"{{"head":{branch:?},"base":"main","title":{title:?}}}"#);
        async move {
            let (code, v) = l.json("POST", &format!("{repo}/pulls"), &body).await;
            assert_eq!(code, 201, "open {branch}");
            v["number"].as_i64().unwrap()
        }
    };

    // the change
    l.must_api(
        "POST",
        &format!("{repo}/contents/POLICY.md"),
        r#"{"content":"cG9saWN5Cg==","message":"add policy","new_branch":"campaign/run-42"}"#,
    )
    .await;
    let change = open("campaign/run-42", "campaign: add policy").await;
    merge(change).await;

    // the rollback: a second pull request undoing the first
    let (_, file) = l
        .json("GET", &format!("{repo}/contents/POLICY.md"), "")
        .await;
    let sha = file["sha"].as_str().unwrap();
    l.must_api("DELETE", &format!("{repo}/contents/POLICY.md"), &format!(r#"{{"sha":{sha:?},"message":"revert: add policy","new_branch":"campaign/run-42-rollback"}}"#)).await;
    let rollback = open("campaign/run-42-rollback", "revert: add policy").await;
    merge(rollback).await;

    let err = l.env().verify().await;
    let msg = err.as_ref().unwrap_err().to_string();
    assert!(
        exit_kind(&err) == "drift" && msg.contains("billing-api: ") && msg.contains("main is at"),
        "verify after two merges: want drift on main, got {msg}"
    );
    assert!(
        !msg.contains("open request"),
        "merged pull requests are not open ones: {msg}"
    );

    l.env().reset().await.expect("reset");
    l.env().verify().await.expect("verify after reset");

    // What reset cannot do, pinned so that nobody is surprised: the pull requests are still
    // there, still merged, and the next one will be #3.
    for number in [change, rollback] {
        let (code, pr) = l.json("GET", &format!("{repo}/pulls/{number}"), "").await;
        assert!(
            code == 200 && pr["merged"] == true,
            "pull request #{number} after reset: HTTP {code} merged={}",
            pr["merged"]
        );
    }
}

/// A repository the fleet does not declare is never compared, never requested, and survives
/// destroy.
#[tokio::test]
async fn undeclared_repos_are_invisible() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    l.must_api(
        "POST",
        &format!("/orgs/{}/repos", l.org()),
        r#"{"name":"my-scratch","auto_init":true}"#,
    )
    .await;

    l.reset_seen();
    l.env()
        .verify()
        .await
        .expect("verify with an undeclared repository present");
    l.drift().await;
    l.env().reset().await.expect("reset");
    l.env().destroy().await.expect("destroy");
    for req in l.seen() {
        assert!(
            !req.contains("my-scratch") && !req.ends_with(&format!("/orgs/{}/repos", l.org())),
            "forgelab looked at what it did not declare: {req}"
        );
    }
    assert_eq!(
        l.api("GET", &l.repo("my-scratch"), "").await,
        200,
        "the undeclared repository did not survive destroy"
    );
    assert_eq!(
        l.api("GET", &l.repo("compliant"), "").await,
        404,
        "destroy left a declared repository behind"
    );
}

/// Reset reads the whole fleet but writes only to what drifted.
#[tokio::test]
async fn reset_writes_only_to_touched() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    l.must_api(
        "POST",
        &format!("{}/contents/NEW.md", l.repo("billing-api")),
        r#"{"content":"aGVsbG8K","message":"change","new_branch":"feature/x"}"#,
    )
    .await;

    l.reset_seen();
    l.env().reset().await.expect("reset");
    let writes: Vec<String> = l
        .seen()
        .into_iter()
        .filter(|r| !r.starts_with("GET "))
        .collect();
    assert!(!writes.is_empty(), "reset made no writes at all");
    for w in &writes {
        assert!(
            w.contains("/billing-api"),
            "reset wrote to a repository that had not drifted: {w}"
        );
    }
}

/// Reset cannot create a repository: a missing one is a guard failure, not something to
/// helpfully put back.
#[tokio::test]
async fn reset_cannot_create() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    l.must_api("DELETE", &l.repo("compliant"), "").await;
    l.drift2("billing-api").await;

    want_guard(l.env().reset().await, "compliant: missing");
    assert_eq!(
        l.api("GET", &l.repo("compliant"), "").await,
        404,
        "reset recreated the repository"
    );
    // and it refused as a whole: the drifted repository was not touched either
    assert_eq!(exit_kind(&l.env().verify().await), "guard");
}

#[tokio::test]
async fn guards() {
    if !e2e_enabled() {
        return;
    }
    // apply never adopts a same-named repository -- and when two are in the way, both are named
    {
        let mut l = Lab::new().await;
        let org = format!("{}-adopt", l.org());
        l.set_org(&org);
        l.must_api("POST", "/orgs", &format!(r#"{{"username":{org:?}}}"#))
            .await;
        l.must_api(
            "POST",
            &format!("/orgs/{org}/repos"),
            r#"{"name":"compliant","auto_init":true}"#,
        )
        .await;
        l.must_api(
            "POST",
            &format!("/orgs/{org}/repos"),
            r#"{"name":"billing-api","auto_init":true}"#,
        )
        .await;

        let err = l.env().apply().await;
        want_guard(err.clone(), "compliant already exists without");
        want_guard(err, "billing-api already exists without");
        assert_eq!(
            l.api("GET", &l.repo("parser-svc"), "").await,
            404,
            "apply created repositories despite refusing"
        );
        // destroy leaves them alone too
        l.env().destroy().await.unwrap();
        assert_eq!(
            l.api("GET", &l.repo("compliant"), "").await,
            200,
            "destroy deleted a repository that is not forgelab's"
        );
    }

    let l = Lab::new().await;
    l.must_apply().await;

    // a moved baseline tag
    {
        l.drift2("scaffold").await;
        l.must_api(
            "DELETE",
            &format!("{}/tags/{}", l.repo("scaffold"), seed::BASELINE_TAG),
            "",
        )
        .await;
        l.must_api(
            "POST",
            &format!("{}/tags", l.repo("scaffold")),
            &format!(r#"{{"tag_name":{:?},"target":"main"}}"#, seed::BASELINE_TAG),
        )
        .await;
        want_guard(
            l.env().verify().await,
            "scaffold: tag forgelab-baseline is missing or is not",
        );
        want_guard(l.env().reset().await, "nothing was reset");
        l.must_apply().await; // apply is the way out
    }

    // a renamed repository is missing, not followed
    {
        l.must_api("PATCH", &l.repo("public"), r#"{"name":"public-renamed"}"#)
            .await;
        want_guard(l.env().verify().await, "public: missing");
        l.must_api("PATCH", &l.repo("public-renamed"), r#"{"name":"public"}"#)
            .await;
    }

    // an empty repository that was pushed to
    {
        l.must_api(
            "POST",
            &format!("{}/contents/X.md", l.repo("no-commits")),
            r#"{"content":"eAo=","message":"x"}"#,
        )
        .await;
        want_guard(l.env().verify().await, "no-commits: is no longer empty");
        l.must_api("DELETE", &l.repo("no-commits"), "").await;
        l.must_apply().await;
    }

    // a fixture edited without re-applying
    {
        let readme = l.dir().join("repos/scaffold/README.md");
        std::fs::write(&readme, "# edited\n").unwrap();
        want_guard(l.env().verify().await, "the fleet changed since");
        want_guard(l.env().reset().await, "the fleet changed since");

        l.env().plan().await.unwrap();
        let out = l.out();
        assert!(
            out.contains("~ scaffold") && out.contains("content: re-seed"),
            "plan:\n{out}"
        );
        l.must_apply().await;
        l.env().verify().await.expect("verify after re-apply");
    }

    // archived repositories can be re-seeded and reset
    {
        l.must_api("PATCH", &l.repo("archived"), r#"{"archived":false}"#)
            .await;
        l.drift2("archived").await;
        l.must_api("PATCH", &l.repo("archived"), r#"{"archived":true}"#)
            .await;
        l.env().reset().await.expect("reset");
        l.env().verify().await.expect("verify");
    }
}

/// apply converges on a changed tag set and a changed default branch, removing what it made
/// before and the fleet no longer declares -- and nothing else.
#[tokio::test]
async fn apply_converges_tags_and_default_branch() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;

    // tagged: [v1, v2] -> [v2, v3]; master-branch: master -> trunk
    let fleet_yaml = l.dir().join(fleet::SPEC_FILE);
    let raw = std::fs::read_to_string(&fleet_yaml).unwrap();
    let raw = raw
        .replace("tags: [v1, v2]", "tags: [v2, v3]")
        .replace("default_branch: master", "default_branch: trunk");
    std::fs::write(&fleet_yaml, raw).unwrap();

    l.env().plan().await.unwrap();
    let out = l.out();
    assert!(
        out.contains("~ tagged") && out.contains("remove: refs/tags/v1"),
        "plan:\n{out}"
    );
    assert!(
        out.contains("~ master-branch") && out.contains("default branch: master → trunk"),
        "plan:\n{out}"
    );

    l.must_apply().await;
    l.env().verify().await.expect("verify after converging");
    assert_eq!(
        l.api("GET", &format!("{}/tags/v1", l.repo("tagged")), "")
            .await,
        404,
        "v1 should be gone"
    );
    assert_eq!(
        l.api("GET", &format!("{}/tags/v3", l.repo("tagged")), "")
            .await,
        200,
        "v3 should exist"
    );
    assert_eq!(
        l.api(
            "GET",
            &format!("{}/branches/master", l.repo("master-branch")),
            ""
        )
        .await,
        404,
        "the old default branch should be gone"
    );
    let (_, r) = l.json("GET", &l.repo("master-branch"), "").await;
    assert_eq!(r["default_branch"], "trunk");
}

/// A test that protected the default branch does not leave it stuck: reset lifts the
/// protection before force-pushing.
#[tokio::test]
async fn reset_lifts_branch_protection() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    let repo = l.repo("compliant");
    l.drift2("compliant").await;
    let code = l
        .api(
            "POST",
            &format!("{repo}/branch_protections"),
            r#"{"rule_name":"main","branch_name":"main","enable_push":true}"#,
        )
        .await;
    assert!(code == 201 || code == 200, "protect main: HTTP {code}");

    l.env()
        .reset()
        .await
        .expect("reset with a protected default branch");
    l.env().verify().await.expect("verify after reset");
}

/// Several repositories failing are all named, not just the first.
#[tokio::test]
async fn every_failure_is_reported() {
    if !e2e_enabled() {
        return;
    }
    let l = Lab::new().await;
    l.must_apply().await;
    // Two repositories the forge will refuse to push to: archived twice over, by hand, after
    // the plan is computed... simpler: make two declared repositories vanish and ask reset.
    l.must_api("DELETE", &l.repo("compliant"), "").await;
    l.must_api("DELETE", &l.repo("scaffold"), "").await;
    let err = l.env().verify().await.unwrap_err().to_string();
    assert!(
        err.contains("compliant: missing") && err.contains("scaffold: missing"),
        "{err}"
    );
}
