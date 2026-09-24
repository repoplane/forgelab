// Package fleet loads a fleet declaration and the lock that records what it resolves to.
// It knows nothing about forges.
package fleet

import (
	"fmt"
	"io/fs"
	"path"
	"regexp"
	"sort"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// SpecVersion is the fleet.yaml schema version this package understands. It is checked
// rather than ignored so that a future schema cannot be silently read under today's rules.
const SpecVersion = 1

const (
	// SpecFile and ReposDir are fixed names inside a fleet directory.
	SpecFile = "fleet.yaml"
	ReposDir = "repos"

	VisibilityPrivate = "private"
	VisibilityPublic  = "public"

	defaultBranch = "main"
)

// Spec is a fleet: what to create, and the pinned identity to create it with.
type Spec struct {
	Version int
	Git     GitIdentity
	Repos   []Repo
}

// Namespaces lists every namespace the repositories sit in, ancestors included, each one
// before what is inside it.
func (s *Spec) Namespaces() []string {
	seen := map[string]bool{}
	var out []string
	for _, r := range s.Repos {
		for ns := path.Dir(r.Name); ns != "." && !seen[ns]; ns = path.Dir(ns) {
			seen[ns] = true
			out = append(out, ns)
		}
	}
	sort.Strings(out)
	return out
}

// GitIdentity pins the author, committer and clock used for every seeded commit, which
// is what makes the resulting commit SHAs identical across machines and across runs.
type GitIdentity struct {
	Author    Author    `yaml:"author"`
	Timestamp time.Time `yaml:"timestamp"`
}

// Author is a git identity.
type Author struct {
	Name  string `yaml:"name"`
	Email string `yaml:"email"`
}

// Repo is one declared repository. Name and Dir come from the directory tree; the
// remaining fields come from the defaults and the optional overrides in fleet.yaml.
type Repo struct {
	// Name is the path under repos/: "dotfiles", or "platform/core/api" inside namespaces.
	Name          string
	Dir           string
	DefaultBranch string
	Visibility    string
	Topics        []string
	Archived      bool
	// Empty repositories are created and never pushed to: no commits, no branches.
	Empty bool
	// Tags all point at the seed commit.
	Tags []string
}

// override is the fleet.yaml entry for one repository. Every field is optional, so a
// repository that needs nothing special can be absent from the file entirely.
type override struct {
	DefaultBranch string   `yaml:"default_branch"`
	Visibility    string   `yaml:"visibility"`
	Topics        []string `yaml:"topics"`
	Archived      bool     `yaml:"archived"`
	Empty         bool     `yaml:"empty"`
	Tags          []string `yaml:"tags"`
}

type fleetFile struct {
	Version  int         `yaml:"version"`
	Git      GitIdentity `yaml:"git"`
	Defaults struct {
		DefaultBranch string `yaml:"default_branch"`
		Visibility    string `yaml:"visibility"`
	} `yaml:"defaults"`
	Repos map[string]override `yaml:"repos"`
}

var repoName = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]*$`)

// LoadSpec reads fleet.yaml from the root of fsys and walks repos/ for fixture directories.
// The directory tree is authoritative: an override for a repository that does not exist on
// disk is an error, because it is almost always a typo or a stale entry.
func LoadSpec(fsys fs.FS) (*Spec, error) {
	raw, err := fs.ReadFile(fsys, SpecFile)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", SpecFile, err)
	}
	var ff fleetFile
	if err := yaml.Unmarshal(raw, &ff); err != nil {
		return nil, fmt.Errorf("parse %s: %w", SpecFile, err)
	}
	if ff.Version != SpecVersion {
		return nil, fmt.Errorf("%s: version is %d, want %d", SpecFile, ff.Version, SpecVersion)
	}
	if ff.Git.Author.Name == "" || ff.Git.Author.Email == "" {
		return nil, fmt.Errorf("%s: git.author.name and git.author.email are required", SpecFile)
	}
	if ff.Git.Timestamp.IsZero() {
		return nil, fmt.Errorf("%s: git.timestamp is required and pins the commit clock", SpecFile)
	}
	if ff.Defaults.DefaultBranch == "" {
		ff.Defaults.DefaultBranch = defaultBranch
	}
	if ff.Defaults.Visibility == "" {
		ff.Defaults.Visibility = VisibilityPrivate
	}

	repos, err := walkRepos(fsys)
	if err != nil {
		return nil, err
	}
	if len(repos) == 0 {
		return nil, fmt.Errorf("no repositories found under %s/", ReposDir)
	}

	known := map[string]bool{}
	flat := map[string]string{}
	for _, r := range repos {
		known[r.Name] = true
		// A forge without namespaces joins the path with "-". Two names that join to the same
		// one are refused everywhere, so that a fleet never works on one forge only. Forges
		// match names ignoring case, so Api and api are the same one too.
		f := strings.ReplaceAll(r.Name, "/", "-")
		if other, taken := flat[strings.ToLower(f)]; taken {
			return nil, fmt.Errorf("%s/: %s and %s are both %q on a forge without namespaces",
				ReposDir, other, r.Name, f)
		}
		flat[strings.ToLower(f)] = r.Name
	}
	for key := range ff.Repos {
		if !known[key] {
			return nil, fmt.Errorf("%s: override for %q but no such directory under %s/",
				SpecFile, key, ReposDir)
		}
	}

	for i := range repos {
		r := &repos[i]
		o := ff.Repos[r.Name]
		r.DefaultBranch = firstNonEmpty(o.DefaultBranch, ff.Defaults.DefaultBranch)
		r.Visibility = firstNonEmpty(o.Visibility, ff.Defaults.Visibility)
		r.Topics = normaliseTopics(o.Topics)
		r.Archived = o.Archived
		r.Empty = o.Empty
		r.Tags = append([]string(nil), o.Tags...)
		sort.Strings(r.Tags)

		if r.Visibility != VisibilityPrivate && r.Visibility != VisibilityPublic {
			return nil, fmt.Errorf("%s: %s: visibility %q, want private or public",
				SpecFile, r.Name, r.Visibility)
		}
		if r.Empty && len(r.Tags) > 0 {
			return nil, fmt.Errorf("%s: %s: an empty repository has no commit to tag", SpecFile, r.Name)
		}
	}

	return &Spec{Version: ff.Version, Git: ff.Git, Repos: repos}, nil
}

// walkRepos finds every repository under repos/, sorted by name. The tree says which
// directory is which: one that holds a file is a repository, and one that holds only
// directories is a namespace, whose path becomes part of the names below it.
func walkRepos(fsys fs.FS) ([]Repo, error) {
	out, err := walkDir(fsys, ReposDir)
	if err != nil {
		return nil, err
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

func walkDir(fsys fs.FS, dir string) ([]Repo, error) {
	entries, err := fs.ReadDir(fsys, dir)
	if err != nil {
		return nil, fmt.Errorf("read %s/: %w", dir, err)
	}
	var out []Repo
	for _, e := range entries {
		if !e.IsDir() || strings.HasPrefix(e.Name(), ".") {
			continue
		}
		p := path.Join(dir, e.Name())
		if !repoName.MatchString(e.Name()) {
			return nil, fmt.Errorf("%s: not a valid repository name", p)
		}
		isRepo, err := holdsFile(fsys, p)
		if err != nil {
			return nil, err
		}
		if isRepo {
			out = append(out, Repo{Name: strings.TrimPrefix(p, ReposDir+"/"), Dir: p})
			continue
		}
		nested, err := walkDir(fsys, p)
		if err != nil {
			return nil, err
		}
		if len(nested) == 0 {
			return nil, fmt.Errorf("%s: holds no file, so it is a namespace, yet no repository is under it", p)
		}
		out = append(out, nested...)
	}
	return out, nil
}

func holdsFile(fsys fs.FS, dir string) (bool, error) {
	entries, err := fs.ReadDir(fsys, dir)
	if err != nil {
		return false, fmt.Errorf("read %s/: %w", dir, err)
	}
	for _, e := range entries {
		if !e.IsDir() && !IsOSJunk(e.Name()) {
			return true, nil
		}
	}
	return false, nil
}

// normaliseTopics lowercases, dedupes and sorts. Never nil: a forge reports an empty list
// as [], and a lock that wrote null would diverge from it.
func normaliseTopics(in []string) []string {
	seen := map[string]bool{}
	out := []string{}
	for _, t := range in {
		t = strings.ToLower(strings.TrimSpace(t))
		if t != "" && !seen[t] {
			seen[t] = true
			out = append(out, t)
		}
	}
	sort.Strings(out)
	return out
}

func firstNonEmpty(a, b string) string {
	if a != "" {
		return a
	}
	return b
}
