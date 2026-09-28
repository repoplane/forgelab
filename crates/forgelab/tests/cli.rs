//! The binary as a shell sees it: the completion script in a real bash, and the hidden
//! `__sandboxes` command it relies on.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_forgelab");

fn examples_fleet() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/fleet")
        .canonicalize()
        .unwrap()
}

fn have_bash() -> bool {
    Command::new("bash")
        .arg("-c")
        .arg("true")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs the real script in a real bash: it sets the words the shell would, calls the
/// function, and returns what it offered. The binary is reached by path, as
/// `./bin/forgelab <TAB>` would: nothing called "forgelab" is on PATH, so the script must ask
/// the binary being completed.
fn complete(words: &[&str]) -> String {
    let mut words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    words[0] = BIN.to_string();
    let quoted: Vec<String> = words.iter().map(|w| format!("'{w}'")).collect();
    let script = format!(
        "eval \"$('{BIN}' completion bash)\"\nCOMP_WORDS=({})\nCOMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 ))\n_forgelab\nprintf '%s\\n' \"${{COMPREPLY[@]}}\"",
        quoted.join(" ")
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash");
    assert!(
        out.status.success(),
        "bash: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn completion() {
    if !have_bash() {
        eprintln!("no bash; skipping");
        return;
    }
    let fleet = examples_fleet();
    let fleet_s = fleet.to_str().unwrap();
    let config = fleet.join("sandboxes.yaml");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["forgelab", ""],
            "plan apply verify reset destroy version completion",
        ),
        (vec!["forgelab", "re"], "reset"),
        (vec!["forgelab", "verify", "--s"], "--sandbox"),
        (vec!["forgelab", "completion", ""], "bash zsh"),
        // sandbox names come from the config, found through a --fleet typed earlier on the line
        (
            vec!["forgelab", "verify", "--fleet", fleet_s, "--sandbox", ""],
            "local",
        ),
        (
            vec![
                "forgelab",
                "verify",
                "--fleet",
                "/nonexistent",
                "--sandbox",
                "",
            ],
            "",
        ),
        // --flag=value, which bash splits around the "="
        (
            vec![
                "forgelab",
                "verify",
                "--fleet",
                "=",
                fleet_s,
                "--sandbox",
                "=",
                "lo",
            ],
            "local",
        ),
        (
            vec![
                "forgelab",
                "verify",
                "--config",
                config.to_str().unwrap(),
                "--sandbox",
                "",
            ],
            "local",
        ),
        // position, not the previous word, decides: a value that happens to be "completion"
        (
            vec!["forgelab", "verify", "--fleet", "completion", ""],
            "--sandbox --fleet --config --concurrency --yes -v",
        ),
        (vec!["forgelab", "completion", "bash", ""], ""),
        (vec!["forgelab", "version", ""], ""),
    ];
    for (words, want) in cases {
        assert_eq!(complete(&words), want, "{words:?}");
    }
}

/// A sandboxes.yaml can arrive with somebody else's repository. Pressing TAB must never run
/// what it contains: `compgen -W` expands its word list, command substitutions included.
#[test]
fn completion_does_not_execute_config() {
    if !have_bash() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let canary = dir.path().join("PWNED");
    let config = format!(
        "version: 1\nsandboxes:\n  '$(touch {c})':\n    {{forge: github, org: x, token_env: T}}\n  '`touch {c}`':\n    {{forge: github, org: x, token_env: T}}\n",
        c = canary.display()
    );
    std::fs::write(dir.path().join("sandboxes.yaml"), config).unwrap();
    let got = complete(&[
        "forgelab",
        "verify",
        "--fleet",
        dir.path().to_str().unwrap(),
        "--sandbox",
        "",
    ]);
    assert_eq!(
        got, "",
        "offered {got:?} for a config with hostile names, want nothing"
    );
    assert!(
        !canary.exists(),
        "completion executed a command taken from sandboxes.yaml"
    );
}

#[test]
fn version_and_usage() {
    let out = Command::new(BIN).arg("version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("forgelab "));
    let out = Command::new(BIN).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage:"));
    // a typo is reported as a typo, before any sandbox is opened
    let out = Command::new(BIN)
        .args(["verfy", "--sandbox", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command \"verfy\""));
}

/// A CLI that drags in a container runtime is a CLI nobody installs. testcontainers and the
/// fault layer are for the tests only.
#[test]
fn cli_does_not_link_test_only_crates() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .expect("cargo tree");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deps = String::from_utf8_lossy(&out.stdout);
    for bad in ["testcontainers", "bollard", "forgelab-faultproxy"] {
        assert!(!deps.contains(bad), "the CLI links {bad}");
    }
}
