<h1 align="center">🧪 ForgeLab</h1>

<p align="center">
  <strong>Put a known set of repositories into a sandbox org. Run tests. Put them back, fast.</strong>
</p>

<p align="center">
  <a href="https://github.com/repoplane/forgelab/actions/workflows/ci.yml"><img src="https://github.com/repoplane/forgelab/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/repoplane/forgelab/releases/latest"><img src="https://img.shields.io/github/v/release/repoplane/forgelab?sort=semver" alt="Release"></a>
  <a href="go.mod"><img src="https://img.shields.io/github/go-mod/go-version/repoplane/forgelab" alt="Go version"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
</p>

<p align="center">
  <a href="#-install">Install</a> &bull;
  <a href="#-try-it-in-two-minutes">Try it</a> &bull;
  <a href="#-a-fleet">A fleet</a> &bull;
  <a href="#-how-it-behaves">How it behaves</a> &bull;
  <a href="examples/fleet">Example</a>
</p>

---

Testing a forge integration has two bad options. A **mock** proves only that your parser agrees
with your own assumptions. A **live organisation** is real, but not reproducible — it drifts
under you between runs.

ForgeLab is the third option: **a real forge, plus a committed record of exactly what is supposed
to be there.**

| | Forgejo | GitHub | GitLab |
|---|:---:|:---:|:---:|
| Supported | ✅ | ✅ | ✅ |

## ⚡ What it looks like

A test run opened a pull request, pushed straight to `main`, left a tag behind and edited some
topics. `verify` names every bit of it; `reset` puts it back in half a second:

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
`--yes` and `-v`.

## 📦 Install

A prebuilt binary, for Linux and macOS on x86_64 and arm64:

```sh
curl -fsSL "https://github.com/repoplane/forgelab/releases/latest/download/forgelab_$(uname -s)_$(uname -m).tar.gz" \
  | sudo tar -xz -C /usr/local/bin forgelab
```

To pin a version, as CI should, replace `latest/download` with `download/v0.1.0`.

Or build it from source with Go:

```sh
go install github.com/repoplane/forgelab/cmd/forgelab@latest
```

ForgeLab needs `git` on the `PATH` at run time.

## 🚀 Try it in two minutes

Needs Go, git and Docker.

```sh
make up                                # a local Forgejo on :3000; prints a token
export FORGELAB_LOCAL_TOKEN=…
go run ./cmd/forgelab apply  --sandbox local --fleet examples/fleet
go run ./cmd/forgelab verify --sandbox local --fleet examples/fleet
# open a pull request, push a branch, change a topic at http://localhost:3000 …
go run ./cmd/forgelab verify --sandbox local --fleet examples/fleet   # exit 1, names the drift
go run ./cmd/forgelab reset  --sandbox local --fleet examples/fleet
make down
```

Log in as `labadmin` / `labadmin-not-a-secret` to see the private repositories.

## 📁 A fleet

```text
my-fleet/
├── fleet.yaml          pinned git identity, defaults, per-repo overrides
├── repos/
│   └── <name>/         one directory per repository — the listing IS the fleet
├── sandboxes.yaml      where to apply it
└── fleet.lock.json     resolved settings + baseline commit SHAs; written by apply; commit it
```

`fleet.yaml` carries only what a directory cannot say. Every field is optional:

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

`sandboxes.yaml` says where it goes. The org is only reachable through here — there is no
`--org` flag to mistype:

```yaml
version: 1
org_allowlist: '^forgelab-sandbox'      # every non-loopback org must match
sandboxes:
  local:
    forge: forgejo
    base_url: http://localhost:3000
    org: forgelab-sandbox
    token_env: FORGELAB_LOCAL_TOKEN     # the NAME of a variable, never a secret
```

A GitHub sandbox needs no `base_url` (set it only for Enterprise Server):

```yaml
  gh:
    forge: github
    org: your-sandbox-org               # must match org_allowlist
    token_env: FORGELAB_GH_TOKEN
```

Use a **fine-grained personal access token** whose *resource owner* is the sandbox org: it cannot
reach anything else, so a leak or a mistake stays inside disposable fixtures. Give it *All
repositories*, and read/write on **Administration**, **Contents** and **Pull requests**. (A classic
PAT with `repo` + `delete_repo` also works, but it can touch every repository you can.) Add
*Workflows* only if a fixture carries `.github/workflows/`.

ForgeLab only ever reads the variable named by `token_env`, so keep the token wherever you keep
secrets — it never needs to be in a file, a flag or your `gh` login. On macOS, the Keychain:

```sh
security add-generic-password -a "$USER" -s forgelab-gh -w        # prompts; paste the token
export FORGELAB_GH_TOKEN=$(security find-generic-password -s forgelab-gh -w)
```

Use an org that holds nothing else you care about, and remember that a `visibility: public`
fixture really is public there.

A GitLab sandbox is a group, addressed by its full path (`base_url` only for self-managed):

```yaml
  gl:
    forge: gitlab
    org: your-sandbox-group             # or a nested path: your-group/sandbox
    token_env: FORGELAB_GL_TOKEN
```

Make the group **public** if any fixture is — a GitLab project cannot be more visible than its
group; private projects inside a public group stay private. For the token, a fine-grained personal
access token limited to that group, with read/write on its *Projects* and *Repository* resources
(code, branches, tags, protected branches, merge requests) and read on *Groups* — plus the
**user-level** permission *Project: Create*, because GitLab creates projects through a global
endpoint. A classic token with `api` + `write_repository` also works. Since a GitLab token is only
as narrow as its account, a dedicated account that belongs to nothing but the sandbox group is the
safest owner for it.

[`examples/fleet`](examples/fleet) is a working one: twelve tiny repositories, each a shape that
forge integrations trip on.

| Fixture | Shape |
|---|---|
| `compliant` | the control — nothing unusual |
| `master-branch` | a default branch that is not `main` |
| `archived` | archived: readable, and rejects every write |
| `no-commits` | no commits at all — the null default ref |
| `tagged` | carries tags `v1` and `v2` |
| `scaffold` | a README and nothing else |
| `public` | the one public repository |
| `dotfiles` | everything under dot-prefixed paths |
| `billing-api` · `ledger-worker` · `node-gateway` · `parser-svc` | plain services, with topics |

Twelve is chosen for its divisors: list the fleet with a page size of 12, 6, 5, 4, 3 or 1 and you
get an exact single page, exact multiples, a short tail and a deep cursor chain.

## 🧭 How it behaves

**👀 Declared repos only.** ForgeLab looks each declared repository up by name and never lists the
org. Anything else in there is invisible to it — never compared, reported or touched — so you can
use the sandbox org by hand. The flip side: ForgeLab guarantees the state of *its* repos, not the
contents of the org. If your tests assert on a whole-org listing, filter on the `forgelab-managed`
topic or keep hand-made repos out of that org.

**🎯 Deterministic.** Content is pushed with git under a pinned author and clock, so commit SHAs
are identical on every machine and every forge. `fleet.lock.json` is byte-stable, and your tests
can assert against it.

**🪶 `reset` is cheap and narrow.** It writes only to repositories that drifted, moves refs without
transferring objects, and *cannot* create or delete a repository — a missing one is exit 2, not
something to helpfully put back.

**🔒 Safe by construction.** A non-loopback org must match `org_allowlist`. Every repo ForgeLab
creates carries a marker topic; a same-named repo without it is never adopted, reset or deleted.
`destroy` asks before deleting (`--yes` skips it). Tokens are read from an environment variable
named in `sandboxes.yaml` — never from a file or a flag.

### Exit codes

`verify` is built to be a CI gate, so it keeps apart two failures that want opposite responses:

| Exit | Meaning | Do |
|:---:|---|---|
| `0` | matches the lock | proceed |
| `1` | **drift** — commits, branches, tags, open pull requests, settings | `reset`, retry once |
| `2` | **guard failure** — repo missing, not ForgeLab's, baseline or fleet changed | stop, look |

What `reset` cannot restore: pull requests themselves. It rewinds `main` after a merge, closes
what is open and deletes the branches, but no forge deletes a pull request — each run leaves its
requests behind as closed or merged, and numbering keeps climbing. Tag what your tests create with
a run id you control (a branch prefix, a label) and filter on it; never assert on a number or a
count. For a truly clean slate, `destroy` then `apply`. It also cannot empty a `no-commits` repo
that was pushed to: delete it on the forge, then `apply`.

## 🛠 Development

```sh
make unit     # no Docker
make test     # end-to-end against a throwaway Forgejo container
make ci       # exactly what CI runs: lint, then test
make dist     # cross-compile the release archives into ./dist
```

Pushing a `v*` tag publishes a release: `git tag v0.1.0 && git push origin v0.1.0`.

## 📄 License

[MIT](LICENSE)
