# ForgeLab

[![CI](https://github.com/repoplane/forgelab/actions/workflows/ci.yml/badge.svg)](https://github.com/repoplane/forgelab/actions/workflows/ci.yml)

**Put a known set of repositories into a sandbox org. Run tests. Put them back, fast.**

Testing a forge integration against a mock proves only that your parser agrees with your own
assumptions. Testing against a live organisation is real, but not reproducible. forgelab is the
third option: a real forge, plus a committed record of exactly what is supposed to be there.

Forgejo is supported today. GitHub and GitLab are next.

```sh
forgelab plan    --sandbox local    # what apply would create (+) or update (~); no writes
forgelab apply   --sandbox local    # create, seed, configure; writes fleet.lock.json
forgelab verify  --sandbox local    # exit 0 ok · 1 drift · 2 guard failure
forgelab reset   --sandbox local    # back to baseline in under a second
forgelab destroy --sandbox local    # delete the declared repos, nothing else
```

## Try it

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

## A fleet

```text
my-fleet/
  fleet.yaml          pinned git identity, defaults, per-repo overrides
  repos/<name>/       one directory per repository — the listing IS the fleet
  sandboxes.yaml      where to apply it
  fleet.lock.json     resolved settings + baseline commit SHAs; written by apply; commit it
```

See [`examples/fleet`](examples/fleet) for a working one: twelve tiny repositories, each a shape
that forge integrations trip on — a `master` default branch, an archived repo, one with no
commits, tags, a public one, a dot-directory.

Per-repo fields in `fleet.yaml`, all optional: `default_branch`, `visibility` (`private` |
`public`), `topics`, `archived`, `empty`, `tags`.

## How it behaves

- **Declared repos only.** forgelab looks each declared repository up by name and never lists the
  org. Anything else in there is invisible to it — never compared, reported or touched — so you
  can use the sandbox org by hand. The flip side: it guarantees the state of *its* repos, not the
  contents of the org. If your tests assert on a whole-org listing, filter on the
  `forgelab-managed` topic or keep hand-made repos out of that org.
- **Deterministic.** Content is pushed with git under a pinned author and clock, so commit SHAs
  are identical on every machine and every forge. `fleet.lock.json` is byte-stable, and your tests
  can assert against it.
- **`reset` is cheap and narrow.** It writes only to repositories that drifted, moves refs without
  transferring objects, and cannot create or delete a repository.
- **Safe by construction.** A non-loopback org must match `org_allowlist`. Every repo forgelab
  creates carries a marker topic; a same-named repo without it is never adopted, reset or deleted.
  `apply` and `destroy` make you type the sandbox name. Tokens are read from an environment
  variable named in `sandboxes.yaml` — never from a file or a flag.

| Exit | Meaning | Do |
|---|---|---|
| 0 | matches the lock | proceed |
| 1 | drift: commits, branches, tags, open pull requests, settings | `reset`, retry once |
| 2 | guard failure: repo missing, not forgelab's, baseline or fleet changed | stop, look |

What `reset` cannot restore: closed pull requests and their numbers (never assert on a number),
and an `empty` repo that was pushed to (delete it on the forge, then `apply`).

## Development

```sh
make unit     # no Docker
make test     # end-to-end against a throwaway Forgejo container
```
