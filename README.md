<h1 align="center">🧪 ForgeLab</h1>

<p align="center">
  <strong>Put a known set of repositories into a sandbox org. Run tests. Put them back, fast.</strong>
</p>

<p align="center">
  <a href="https://github.com/repoplane/forgelab/actions/workflows/ci.yml"><img src="https://github.com/repoplane/forgelab/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/repoplane/forgelab/releases/latest"><img src="https://img.shields.io/github/v/release/repoplane/forgelab?sort=semver" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
</p>

---

Testing a forge integration has two bad options. A **mock** proves only that your parser agrees
with your own assumptions. A **live organisation** is real, but not reproducible — it drifts
under you between runs.

ForgeLab is the third option: **a real forge, plus a committed record of exactly what is supposed
to be there.**

| | Forgejo | GitHub | GitLab | Azure DevOps |
|---|:---:|:---:|:---:|:---:|
| Supported | ✅ | ✅ | ✅ | ✅ |

Since v0.13.0 ForgeLab is written in Rust. It is a drop-in for the Go releases up to v0.12.0: the
same commands, flags, exit codes, `fleet.yaml`, `sandboxes.yaml` and a byte-identical
`fleet.lock.json`. What changed is [below](#-what-changed-in-v0130-the-rust-rewrite).

## ⚡ What it looks like

A test run opened a pull request, pushed straight to `main`, left a tag behind and edited some
topics. `verify` names every bit of it; `reset` puts it back in seconds:

```console
$ forgelab verify --sandbox local
forgelab: drift (run `forgelab reset --sandbox local`):
  archived: archived is false, want true
  billing-api: extra branch feature/run-42; 1 open request(s)
  compliant: extra tag stray; main is at 9478ed52d4cf, want bb39569c2c7b
  parser-svc: topics are [changed], want [service]
$ echo $?
1

$ forgelab reset --sandbox local
reset parser-svc (topics are [changed], want [service])
reset archived (archived is false, want true)
reset billing-api (extra branch feature/run-42; 1 open request(s))
reset compliant (extra tag stray; main is at 9478ed52d4cf, want bb39569c2c7b)
ok: 4 of 12 repositories reset

$ forgelab verify --sandbox local
ok: 12 repositories match fleet.lock.json
```

Creating repositories is expensive and rare; putting them back is cheap and constant. So the
commands are split along that line: `apply` when the fleet changes, `reset` around every test run.

| Command | What it does | Writes? |
|---|---|---|
| `forgelab plan` | show what `apply` would create (`+`) or update (`~`) | nothing |
| `forgelab apply` | create, seed and configure the declared repos; write `fleet.lock.json` | creates · updates |
| `forgelab verify` | assert the sandbox matches the lock | nothing |
| `forgelab reset` | put drifted repos back to baseline | drifted repos only |
| `forgelab destroy` | delete the declared repos, and nothing else | deletes |

Every command takes `--sandbox <name>`, plus `--fleet <dir>` (default `.`), `--config <file>`,
`--concurrency <n>`, `--yes` and `-v`.

## 📦 Install

A prebuilt binary, for Linux and macOS on x86_64 and arm64, into your own `~/.local/bin` — no
`sudo`, nothing outside your home directory:

```sh
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/repoplane/forgelab/releases/latest/download/forgelab_$(uname -s)_$(uname -m).tar.gz" \
  | tar -xz -C ~/.local/bin forgelab
```

To pin a version, as CI should, replace `latest/download` with `download/v0.13.0`.

Or build it from source with Rust 1.93 or newer:

```sh
cargo install --git https://github.com/repoplane/forgelab forgelab
```

ForgeLab needs `git` 2.31 or newer on the `PATH` at run time.

**Shell completion** — commands, flags, and the sandbox names from your `sandboxes.yaml`:

```sh
eval "$(forgelab completion zsh)"      # or bash
```

## 🚀 Try it in two minutes

Needs Rust, git and Docker.

```sh
make up                                # a local Forgejo on :3000; prints a token
export FORGELAB_LOCAL_TOKEN=…
cargo run -p forgelab -- apply  --sandbox local --fleet examples/fleet
cargo run -p forgelab -- verify --sandbox local --fleet examples/fleet
# open a pull request, push a branch, change a topic at http://localhost:3000 …
cargo run -p forgelab -- verify --sandbox local --fleet examples/fleet   # exit 1, names the drift
cargo run -p forgelab -- reset  --sandbox local --fleet examples/fleet
make down
```

Log in as `labadmin` / `labadmin-not-a-secret` to see the private repositories.

## 📁 A fleet

```text
my-fleet/
├── fleet.yaml          pinned git identity, defaults, per-repo overrides
├── repos/
│   ├── <name>/         one directory per repository — the tree IS the fleet
│   └── <namespace>/    a directory holding only directories; repositories nest below, any depth
├── sandboxes.yaml      where to apply it
└── fleet.lock.json     resolved settings + baseline commit SHAs; written by apply; commit it
```

`fleet.yaml` carries only what a directory cannot say. Every field is optional, and a key that
is not one of them is an error rather than silently ignored:

```yaml
version: 1
git:
  author: {name: "Forgelab Fixture", email: "fixture@forgelab.test"}
  timestamp: "2026-01-01T00:00:00Z"     # pinned clock => identical SHAs everywhere
defaults: {visibility: private, default_branch: main}
repos:
  master-branch: {default_branch: master, topics: [legacy]}
  archived:      {archived: true}
  no-commits:    {empty: true}
  tagged:        {tags: [v1, v2]}
  public:        {visibility: public}
```

A directory that holds a file is a repository; one that holds only directories is a
**namespace**. A repository is its path — `platform/core/api` in the lock, the reports and the
`repos:` overrides. Each forge lands the path where it can, and what it cannot hold is joined
with `-`:

| `repos/…` | GitLab | Azure DevOps | GitHub · Forgejo |
|---|---|---|---|
| `dotfiles` | `<group>/dotfiles` | `<project>/_git/dotfiles` | `dotfiles` |
| `services/api` | `<group>/services/api` | `services/_git/api` | `services-api` |
| `platform/core/api` | `<group>/platform/core/api` | `platform/_git/core-api` | `platform-core-api` |

`sandboxes.yaml` says where it goes. The org is only reachable through here — there is no
`--org` flag to mistype. `token_env` names an environment variable; the file never holds a
secret. `concurrency` is optional and overrides the forge's own default (Forgejo 8, GitHub 6,
GitLab 8, Azure DevOps 8):

```yaml
version: 1
sandboxes:
  local:
    forge: forgejo
    base_url: http://localhost:3000
    org: forgelab-sandbox
    token_env: FORGELAB_LOCAL_TOKEN
  gh:
    forge: github
    org: your-sandbox-org
    token_env: FORGELAB_GH_TOKEN
  gl:
    forge: gitlab
    org: your-sandbox-group             # or a nested path: your-group/sandbox
    token_env: FORGELAB_GL_TOKEN
  ado:
    forge: azuredevops
    org: your-org
    default_project: fleet
    token_env: FORGELAB_ADO_TOKEN
    concurrency: 2
```

The tokens, the per-forge caveats (a GitLab project cannot be more visible than its group; an
Azure DevOps repository has no topics and no visibility of its own, and `archived` there means
`disabled`) and the safety model are unchanged since the Go releases; the
[v0.12.0 README](https://github.com/repoplane/forgelab/tree/v0.12.0#readme) remains the reference
for them.

## 🧭 How it behaves

**👀 Declared repos only.** ForgeLab looks each declared repository up by name and never lists the
org. Anything else in there is invisible to it — never compared, reported or touched.

**🎯 Deterministic.** Content is pushed with git under a pinned author and clock, so commit SHAs
are identical on every machine and every forge. `fleet.lock.json` is byte-stable, and identical
to the one the Go releases wrote: the committed locks of `repoplane/fleets` are the test.

**🪶 `reset` is cheap and narrow.** It writes only to repositories that drifted, moves refs without
transferring objects, and *cannot* create or delete a repository — a missing one is exit 2.

**🔒 Safe by construction.** Every repo ForgeLab creates carries a marker topic; a same-named repo
without it is never adopted, reset or deleted. `destroy` asks before deleting (`--yes` skips it).
Credentials never appear on a command line: git receives them as an `Authorization` header
scoped to the forge's origin.

### Exit codes

| Exit | Meaning | Do |
|:---:|---|---|
| `0` | matches the lock | proceed |
| `1` | **drift** — commits, branches, tags, open pull requests, settings | `reset`, retry once |
| `2` | **guard failure** or error — repo missing, not ForgeLab's, baseline or fleet changed, a forge that would not answer | stop, look |

## 🔁 What changed in v0.13.0, the Rust rewrite

Behaviour that is different on purpose from the Go releases. Everything else, output lines
included, is the same.

- **Every failure is reported.** A run over a hundred repositories names each one that failed
  and why, not the first it happened to notice. Guard failures still win: nothing is retried
  past one.
- **Transient failures are retried, within a time budget.** 5xx answers, dropped connections and
  timeouts are retried with backoff; rate limits are waited out as often as fits in ten minutes
  rather than four times. A limit that asks for more than that is an error, as before. GitHub
  mutations are serialised and paced at one per second on top of that.
- **`--concurrency`**, and a `concurrency:` key per sandbox, with defaults per forge.
- **`apply` converges on tag and default-branch changes.** A declared tag that moved or is
  missing is pushed; a tag or default branch the *previous* lock declared and the fleet no
  longer does is removed. Refs a test created stay `reset`'s business.
- **GitLab: a namespace pending deletion is waited for**, up to five minutes, both when
  `destroy` removes it and when the next `apply` needs it back. One that never goes is an error
  (exit 2) where it used to be reported as kept and exit 0.
- **Azure DevOps:** a repository already gone when `destroy` deletes it is not an error; pull
  requests are paged until an empty page; a disabled repository the fleet wants enabled is
  planned as "enable, then check", instead of failing.
- **Branch protection is lifted on Forgejo and GitHub too** before a force-push, as the
  interface always promised. On GitHub this covers classic protection and repository rulesets;
  an organisation ruleset is reported, not lifted.
- **Strict YAML.** A misspelt key in `fleet.yaml` or `sandboxes.yaml` is an error. `marker_topic`
  is lowercased and validated; a declared tag is checked as a ref name and may not be
  `forgelab-baseline`.
- **The lock is written atomically**, a typo in the command is reported before any sandbox is
  opened, and Ctrl-C or SIGTERM stops every wait and every git subprocess and removes the
  scratch directories.
- **git gets its credential as a header**, not in the URL, so it never shows in `ps`. Proxy and
  CA variables (`HTTPS_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `GIT_SSL_CAINFO`, …) reach git. git
  2.31 or newer is required for this.

## 🛠 Development

```sh
make unit     # no Docker: unit tests, the golden locks, the CLI and its completion script
make test     # plus the end-to-end suite against a throwaway Forgejo container
make scale    # the 108-repository fleet from ../fleets through the fault layer (minutes)
make ci       # exactly what CI runs on a pull request: lint, then test
make dist     # the release archives into ./dist (Linux needs cargo-zigbuild and zig)
```

The golden-lock and scale tests read the `repoplane/fleets` checkout at `../fleets`, or wherever
`FORGELAB_FLEETS_DIR` points.

### The fault layer

`crates/faultproxy` puts back what a local Forgejo does not do and the cloud forges do: rate
limits with and without `Retry-After`, GitHub's secondary-limit body, 5xx, dropped connections,
latency, and reads served stale for a few seconds after a write. It runs in-process in the
end-to-end tests and as a reverse proxy in front of the forge, where git traffic is faulted too:

```sh
make up && make proxy                  # Forgejo on :3000, the proxy on :3001
# point a sandbox's base_url at http://127.0.0.1:3001 and run forgelab against it
```

Rules live in `faults/*.yaml`. Every decision is a function of a seed and the request's position
in its own path's history, so a failing run is reproduced with `FORGELAB_FAULT_SEED=<n>`; the
tests print the seed.

Pushing a `v*` tag publishes a release: `git tag v0.1.0 && git push origin v0.1.0`.

## 📄 License

[MIT](LICENSE)
