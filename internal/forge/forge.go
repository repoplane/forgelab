// Package forge is the small surface forgelab needs from a forge. Implementations are
// hand-rolled HTTP clients, deliberately not a consumer's SDK or adapter: a harness that
// verified a sandbox with the client under test would share its bugs.
package forge

import "context"

// Repo is a repository as the forge reports it.
type Repo struct {
	Name          string
	DefaultBranch string
	Visibility    string // "private" | "public"
	Archived      bool
	// Empty means the forge holds no commits for it.
	Empty  bool
	Topics []string
}

// Ref is a branch or a tag and the commit it points at.
type Ref struct {
	Name string
	SHA  string
}

// Request is an open pull or merge request.
type Request struct {
	Number int
	Title  string
}

// Caps says what a forge can express. forgelab skips -- out loud, in the plan -- what a forge
// cannot hold, rather than demand a different fleet for it: one fleet, one lock, every forge.
type Caps struct {
	// Topics: repositories carry topics. Without them there is nowhere to put the marker
	// either, so the "never adopt a repository forgelab did not create" guard is off: there,
	// a repository with a declared name is forgelab's, and only the sandbox's own
	// configuration and the reach of its token keep it in the right place.
	Topics bool
	// Visibility: visibility is set per repository (not per project).
	Visibility bool
	// ArchivedUnreadable: an archived repository cannot be read at all -- not its refs, not
	// its requests. forgelab can then only check the flag itself.
	ArchivedUnreadable bool
}

// Settings is a partial update: nil fields are left alone.
type Settings struct {
	DefaultBranch *string
	Visibility    *string
	Archived      *bool
}

// Forge is bound to one organisation. Every method addresses a repository by name inside
// it. There is deliberately no List: forgelab looks declared repositories up by name and
// never enumerates the organisation, so what it did not declare it cannot see.
type Forge interface {
	Caps() Caps

	// EnsureOrg creates the organisation where a forge lets a token do that, and
	// otherwise checks it exists.
	EnsureOrg(ctx context.Context) error

	// Get reports found=false for a missing repository AND for one that answers under a
	// different name (forges redirect the old name of a renamed repository).
	Get(ctx context.Context, name string) (r Repo, found bool, err error)
	// Create must not return success with the topics unset: they carry the marker that
	// tells forgelab the repository is its own, and an unmarked repository is one that a
	// re-run of apply will refuse to touch. A forge that cannot set them in the same call
	// sets them next, and deletes what it just created if that fails.
	Create(ctx context.Context, name, visibility, defaultBranch string, topics []string) error
	Delete(ctx context.Context, name string) error

	UpdateSettings(ctx context.Context, name string, s Settings) error
	SetTopics(ctx context.Context, name string, topics []string) error

	// Branches is the forge's own listing. It is only used to wait until the forge has
	// caught up with a push, which is what a consumer reading the API will see; the
	// comparison against the lock reads refs from git instead (seed.LsRemote).
	Branches(ctx context.Context, name string) ([]Ref, error)
	DeleteBranch(ctx context.Context, name, branch string) error
	DeleteTag(ctx context.Context, name, tag string) error

	// AllowForcePush makes sure branch can be force-pushed, lifting whatever the forge or a
	// test put in the way. forgelab calls it immediately before every force-push rather than
	// trusting that an earlier step already did: a branch can be protected at any time, by
	// the forge itself (GitLab protects a default branch on first push, possibly a moment
	// after the push returns), by an interrupted apply, or by the test that just ran.
	AllowForcePush(ctx context.Context, name, branch string) error

	OpenRequests(ctx context.Context, name string) ([]Request, error)
	CloseRequest(ctx context.Context, name string, number int) error

	// GitURL is an authenticated HTTP remote. Treat it as a secret.
	GitURL(name string) (string, error)
}
