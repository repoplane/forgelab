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
	// EnsureOrg creates the organisation where a forge lets a token do that, and
	// otherwise checks it exists.
	EnsureOrg(ctx context.Context) error

	// Get reports found=false for a missing repository AND for one that answers under a
	// different name (forges redirect the old name of a renamed repository).
	Get(ctx context.Context, name string) (r Repo, found bool, err error)
	Create(ctx context.Context, name, visibility, defaultBranch string) error
	Delete(ctx context.Context, name string) error

	UpdateSettings(ctx context.Context, name string, s Settings) error
	SetTopics(ctx context.Context, name string, topics []string) error

	Branches(ctx context.Context, name string) ([]Ref, error)
	Tags(ctx context.Context, name string) ([]Ref, error)
	DeleteBranch(ctx context.Context, name, branch string) error
	DeleteTag(ctx context.Context, name, tag string) error

	OpenRequests(ctx context.Context, name string) ([]Request, error)
	CloseRequest(ctx context.Context, name string, number int) error

	// GitURL is an authenticated HTTP remote. Treat it as a secret.
	GitURL(name string) (string, error)
}
