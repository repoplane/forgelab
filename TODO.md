# TODO

Ordered. Each item says what would make it worth doing — nothing here is scheduled by date.

## 1. Branch protection — next

**Why.** Real projects protect their default branch; on GitLab it is protected the moment it is
first pushed. forgelab currently *strips* that protection at `apply` so that `reset` can
force-push, which means a sandbox project does not behave like a customer's: a direct or forced
push to `main` succeeds in the sandbox and fails in the field. This is a fidelity gap on every
forge, not a GitLab special case — which is what makes it the right kind of fleet evolution.

**Shape.**

- `fleet.yaml`: `protected: true` per repo, default `false`. Applies to the default branch only.
  Invalid together with `empty: true` (no branch to protect).
- Meaning, identical on all three forges: the default branch carries a protection rule that
  forbids **force-push** and **deletion**, for everyone including admins. Nothing else — no
  required reviews, no status checks. The uniform observable is the branch API reporting
  `protected: true`.
- Default stays `false` on every forge, GitLab included, so that one fleet still produces one
  lock. "Whatever the forge does by default" would make the lock forge-dependent.
- Lock: a `protected` boolean per repo. Changes the lock bytes; consumers re-run `apply`.
- `forge.Forge`: two methods, `Protected(name, branch) (bool, error)` and
  `SetProtected(name, branch, on) error`. The GitLab adapter stops unprotecting inside
  `UpdateSettings`.
- `apply`: set protection last among the settings (before archive). Around a re-seed, which is a
  force-push: unprotect → push → protect.
- `verify`: one more read per repo. A mismatch is **drift** (exit 1) — `reset` can repair it.
- `reset`, for a touched repo whose refs moved: unarchive → **unprotect** → force-push → default
  branch → close requests → delete extra branches/tags → topics → **re-protect** → re-archive.
  Still declarative: a reset that dies between unprotect and re-protect leaves "unprotected, want
  protected", which the next `verify` reports and the next `reset` fixes.

**Decide before coding.**

1. Is "no force-push, no deletion" the right meaning, or should `protected` also block direct
   pushes (require a pull/merge request)? The second is closer to real-world setups but its
   effect depends on the role of the *consumer's* token, so it is harder to make uniform.
2. **GitHub Free does not allow branch protection on private repositories** — *confirmed against
   the live API on a free org (2026-09-19)*. On a private repo both the classic protection
   endpoint and the rulesets endpoint (even listing) answer 403 "Upgrade to GitHub Pro or make
   this repository public to enable this feature"; on a public repo both work. So on a free org
   a protected fixture must be `visibility: public`, or the org needs a paid plan. `apply` must
   turn that 403 into a sentence saying exactly this.

   Also confirmed on the public probe, with classic protection (`enforce_admins: true`,
   `allow_force_pushes: false`): an **admin** token is refused a force-push ("Cannot
   force-push to this branch"), a direct commit to `main` is still accepted, and after removing
   the protection the force-push goes through. That is precisely the proposed meaning, and it
   proves `reset` needs the unprotect → push → re-protect dance on GitHub too. Use the classic
   endpoint rather than rulesets: one call, and the branch API reports `protected: true`.
3. Protection rules a *test* adds on other branches (GitLab wildcards, extra GitHub rules) block
   `reset` from deleting those branches. v1: fail with a clear message, or remove every rule
   except the default branch's?
4. Which example fixtures become `protected: true`. Probably most of them, since that is what
   real projects look like — subject to (2).

**Verify with.** Forgejo e2e (force-push to a protected `main` is refused; `reset` still restores
it), then a live run on GitHub and GitLab.

## 2. GitLab subgroups

**Gap.** Not in the adapter — `org` already accepts a nested group path. The limit is the model:
one sandbox = one group, so the fleet lands flat and the sandbox does not look like a real GitLab
tree. What a consumer trips on: depth (`include_subgroups`, three-segment `path_with_namespace`),
and **the same project name in two subgroups** — impossible on GitHub, and the classic
"keyed by name" bug.

**Now, no code: one sandbox per subgroup.** `gl-services` → `group/services`, `gl-core` →
`group/platform/teams/core`, same fleet applied to both. A scan of the root sees projects at two
depths with every name duplicated. Cost: N× the projects, one command per sandbox, subgroups
created by hand once.

Small helpers if that gets tedious: `--sandbox a,b`; let the GitLab adapter create a missing
*subgroup* (allowed by the API, unlike a top-level group).

**Later, only on a concrete need: nested `repos/` directories.** `repos/services/billing-api/`
becomes subgroup `services` under the sandbox's group — relative paths, no scope map. Trigger: a
consumer needs *one* fleet with different content at different depths. Known costs: flat forges
must use the leaf name and reject collisions, so a fleet using duplicate names stops being
portable; subgroups can be created but never safely deleted (groups have no topics to carry the
marker); the token needs group-creation rights.

**Not doing.** A separate GitLab-only example fleet or format dialect: one fleet producing a
byte-identical lock on three forges is the property worth protecting, and a GitLab-only example
could not be tested on every commit the way the Forgejo one is. Also out of scope for good:
members and permissions, shared groups, group settings, group tokens — forgelab is about
repositories.

## 3. Smaller, each waiting for its trigger

- **`visibility: internal`** (GitLab, GitHub Enterprise) — when a consumer needs the third value.
  Reject it clearly on forges that lack it rather than degrade silently.
- **`verify --strict`** — one read-only listing of the org that fails if undeclared repositories
  exist. Trigger: a consumer's whole-org assertions get polluted by leftovers. Reads only; the
  "never write to what was not declared" rule stays.
- **`forgelab lock`** — refresh `fleet.lock.json` with git alone, no forge. Trigger: booting a
  Forgejo just to refresh a file becomes annoying. (`make lock` covers the example fleet today.)
- **`apply --prune`** — delete repositories that were in the previous lock and are no longer
  declared. Trigger: removing fixtures becomes routine.
- **Batched lookups on GitHub (GraphQL)** — trigger: a fleet large enough that ~6 reads per
  repository per `verify` matters against the hourly budget.
- **Opt-in live tests in CI** — run the walk against real GitHub/GitLab sandboxes when tokens are
  present as secrets. Trigger: a regression that the Forgejo e2e suite could not have caught.
- **Typed sandbox-name confirmation on `destroy`** — trigger: two cloud sandboxes on the same
  forge. The prompt must then *not* print the expected answer.
- **GitHub App instead of a PAT** — short-lived tokens, installed on the sandbox org only.
  Trigger: PAT rotation becomes a chore, or more than one person runs forgelab.
