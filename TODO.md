# TODO

Ordered. Each item says what would make it worth doing — nothing here is scheduled by date.

**Status: stop adding features.** forgelab covers what a pull-request-driven consumer leaves
behind — branches, open requests, merged requests, rollbacks — on Forgejo, GitHub and GitLab, all
verified against the live forges, and on Azure DevOps. Namespaces (section 3) are built and
verified live on GitHub, GitLab and Azure DevOps. The next useful thing is to wire it into a real consumer's test
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

## 3. Namespaces: GitLab subgroups and Azure DevOps projects

**Built**, as one design for both: a directory under `repos/` that holds only directories is a
namespace, and a repository is its path (`platform/core/api`) everywhere -- overrides, lock,
reports, the `Forge` interface. No new key in `fleet.yaml`, nothing new in `sandboxes.yaml`. Each
forge lands the path where it can: subgroups on GitLab (any depth), the first segment a project
on Azure DevOps and the rest `-`-joined, all of it `-`-joined on GitHub and Forgejo. `apply`
creates the namespaces inside `Create` -- the Azure DevOps `default_project` included -- with
`forgelab-managed` as their description; `destroy`
removes those that carry the marker and are left with nothing at all in them (`DeleteNamespace`),
never the sandbox root. The same leaf name in two namespaces -- the classic
"keyed by name" bug -- is in the example fleet.

**Verified live on Azure DevOps (2026-09-20):** two projects created inside a 15-second `apply`
of fifteen repositories; `destroy` removed both in six seconds; the names were free for a
re-create at once, soft delete or not; a project holding an undeclared repository was kept, and
removed by the next `destroy` once that repository was gone; the born-with repository is left
alone and does not keep a project.

**Verified live on gitlab.com (2026-09-20):** three subgroups created, each as public as the
group above it, inside a 15-second `apply`; `destroy` removed repositories and subgroups in 21
seconds and the re-seed landed on the same paths at once, so `permanently_remove` does free a
subgroup's path the same day; a subgroup holding an undeclared project was kept, **and still
kept while that project was only pending deletion** (GitLab renames it
`scratch-deletion_scheduled-<id>` and goes on listing it), then removed by the next `destroy`
once the project was gone for good. A fine-grained token needs, beyond the README's list, the
user-level *Group* permission (or `POST /groups` answers 403) and the project permission *Code*,
which is separate from the repository ones (or every git operation answers 403).

**Verified live on github.com (2026-09-20):** the three namespaced fixtures land as
`platform-core-api`, `platform-tooling` and `services-api`, reports keep the fleet path
(`platform/core/api: extra branch stray`), drift on one is reset, and `destroy` asks nothing about
namespaces there.

**After review, verified live on both (2026-09-20):** a hand-made `platform` subgroup or project
is used by `apply` and kept by `destroy` ("not created by forgelab") while forgelab's own
namespaces inside and beside it go; a clean sandbox answers "nothing to delete" without a
prompt. GitLab renames a group scheduled for deletion (`platform-deletion_scheduled-<id>`), as it
does a project, so the second DELETE must name the path read back, not the original one.

Facts already in hand for Azure DevOps (live, 2026-09-19): a repository carries no topics,
description or properties, so nothing can hold a marker; deletion is soft with a purgeable
recycle bin and the name is free at once; a disabled repository is listed but answers 404 to
every read *and to its own deletion*; pull request ids are project-wide and survive purged
repositories; whoever owns the PAT may force-push by default; and both the direct GET and the
project listing are cached for about a second, in opposite directions (the GET keeps serving a
deleted repository, the listing lags on new ones and on a flag just changed).

**Not doing.** Length checks on joined names (the forge's own error says it better); a scope map
in `sandboxes.yaml`; a GitLab-only example fleet or format dialect -- one fleet producing a
byte-identical lock on every forge is the property worth protecting. Also out of scope for good:
members and permissions, shared groups, group settings, group tokens, project settings --
forgelab is about repositories.

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
