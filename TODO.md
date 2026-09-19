# TODO

Ordered. Each item says what would make it worth doing — nothing here is scheduled by date.

**Status: stop adding features.** forgelab covers what a pull-request-driven consumer leaves
behind — branches, open requests, merged requests, rollbacks — on Forgejo, GitHub and GitLab, all
verified against the live forges. Azure DevOps too, flat: one sandbox is one project. The next useful thing is to wire it into a real consumer's test
suite and let that decide what, if anything, below is needed.

## 1. Pull request history piles up

`reset` rewinds `main`, closes open requests and deletes branches, but no forge lets a pull
request be deleted: every run leaves its requests behind as closed or merged (a merged one whose
merge commit is no longer on `main`), and numbering keeps climbing. A consumer with an org-wide
view of "all requests / all campaigns" will see every previous run.

- **Default answer, no code:** the consumer tags what it creates with a run id it controls — a
  branch prefix, a label — and filters on it. That is also just realistic: a customer's org is
  full of old and unrelated requests, and telling "this campaign's" apart from the rest is the
  consumer's job. Never assert on a number or on a count.
- **When a suite needs a clean slate:** `destroy` + `apply` (about 50 s on GitHub, 20 s on GitLab,
  at twelve repositories). History gone, numbering back at #1 — and repository ids change.
- Possible later, only if that becomes routine: `reset --recreate <name>`, deleting and re-creating
  one repository. It breaks the rule that reset never creates or deletes, so it needs a real need.

## 2. Merge requirements (was: branch protection)

Demoted. The earlier design made `protected: true` mean "no force-push, no deletion" — behaviour
a consumer that only works through pull requests never exercises. What such a consumer *does*
meet at a customer is a merge it cannot complete: required reviews, required status checks. That
is a different feature (it needs a second identity to approve, or a check to report), and it
waits for a consumer test that needs "merge blocked" as a state.

forgelab's own need is already met: `AllowForcePush` lifts whatever protects a default branch
right before `reset` rewinds it.

Facts established along the way, kept because they were expensive to learn:

- **GitHub Free refuses branch protection on private repositories** (live, free org, 2026-09-19):
  the classic endpoint and rulesets — even listing, even DELETE — answer 403 "Upgrade to GitHub
  Pro or make this repository public". Public repositories accept both. So on a free org anything
  protection-based needs `visibility: public` or a paid plan, and a blind "unprotect" would break
  every reset of a private repository: only unprotect what reads as protected.
- On a public repository with classic protection (`enforce_admins: true`, force-pushes off), an
  **admin** token is refused a force-push, a direct commit to `main` is still accepted, and
  removing the protection lets the force-push through.
- A bare GitLab protection rule also restricts who may push and merge (Maintainers). Matching the
  other forges means setting push and merge access levels explicitly.
- Whatever is added must be diffed by `plan`/`apply` like any other setting, or turning it on for
  an existing fixture plans "nothing to do" and then fails its own trailing verify.

## 3. Namespaces: GitLab subgroups and Azure DevOps projects, designed once

Enterprise GitLab is deep group trees; enterprise Azure DevOps is dozens of projects. Both are the
same gap -- a fleet that lands flat does not look like a customer -- and they sit at opposite
ends (any depth and cheap, versus exactly one level and heavyweight), which is what makes them
the right pair to design a hierarchy from. One design, not GitLab's first and a rework later.

Working vocabulary, to be fixed in `fleet.yaml` only when this is built: **sandbox root** (a
GitHub org, a GitLab group, an Azure DevOps org + default project), **namespace** (a path under
it), **repo**. A namespace becomes subgroups on GitLab, its first segment a project on Azure
DevOps, and a `-`-joined name prefix on GitHub and Forgejo.

Facts already in hand for Azure DevOps (live, 2026-09-19): a repository carries no topics,
description or properties, so nothing can hold a marker; deletion is soft with a purgeable
recycle bin and the name is free at once; a disabled repository is listed but answers 404 to
every read *and to its own deletion*; pull request ids are project-wide and survive purged
repositories; whoever owns the PAT may force-push by default; and both the direct GET and the
project listing are cached for about a second, in opposite directions (the GET keeps serving a
deleted repository, the listing lags on new ones and on a flag just changed). Project creation is
asynchronous and deletion is soft for 28 days, which suits "never delete a namespace".

The GitLab half, as thought through so far:

### GitLab subgroups

**Gap.** Not in the adapter — `org` already accepts a nested group path. The limit is the model:
one sandbox = one group, so the fleet lands flat and the sandbox does not look like a real GitLab
tree. What a consumer trips on: depth (`include_subgroups`, three-segment `path_with_namespace`),
and **the same project name in two subgroups** — impossible on GitHub, and the classic
"keyed by name" bug.

**Now, no code: one sandbox per subgroup.** `gl-services` → `group/services`, `gl-core` →
`group/platform/teams/core`, same fleet applied to both. A scan of the root sees projects at two
depths with every name duplicated. Cost: N× the projects, one command per sandbox, subgroups
created by hand once.

**Prerequisite:** two sandboxes on the same forge is exactly the trigger for the typed
sandbox-name confirmation on `destroy` (section 4) — `gl-core` for `gl-services` passes every
guard. Do that first.

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

## 4. Smaller, each waiting for its trigger

- **`visibility: internal`** (GitLab, GitHub Enterprise) — when a consumer needs the third value.
  Reject it clearly on forges that lack it rather than degrade silently.
- **`verify --strict`** — one read-only listing of the org that fails if undeclared repositories
  exist. Trigger: a consumer's whole-org assertions get polluted by leftovers. Honest cost: it
  puts a `List` on `forge.Forge`, which today deliberately has none — the guarantee weakens from
  "cannot see what it did not declare" to "never writes to it", and the token needs to be able to
  list the org.
- **`forgelab lock`** — refresh `fleet.lock.json` with git alone, no forge. Trigger: booting a
  Forgejo just to refresh a file becomes annoying. (`make lock` covers the example fleet today.)
- **`apply --prune`** — delete repositories that were in the previous lock and are no longer
  declared. Trigger: removing fixtures becomes routine.
- **Batched lookups on GitHub (GraphQL)** — trigger: a fleet large enough that the ~3 API reads
  per repository per `verify` (plus one `git ls-remote`, which is not an API request) matter
  against the hourly budget. At 5,000/hour that is a fleet in the hundreds.
- **Opt-in live tests in CI** — run the walk against real GitHub/GitLab sandboxes when tokens are
  present as secrets. Trigger: a regression that the Forgejo e2e suite could not have caught.
- **Typed sandbox-name confirmation on `destroy`** — trigger: two cloud sandboxes on the same
  forge. The prompt must then *not* print the expected answer.
- **GitHub App instead of a PAT** — short-lived tokens, installed on the sandbox org only.
  Trigger: PAT rotation becomes a chore, or more than one person runs forgelab.
