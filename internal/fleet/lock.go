package fleet

import (
	"encoding/json"
	"fmt"
	"os"
)

// LockFile is the lock's fixed name inside a fleet directory.
const LockFile = "fleet.lock.json"

// Lock is the resolved fleet: every declared value after defaults are applied, plus the
// baseline SHAs that only building the commits can reveal. It is a pure function of the
// fleet, so it is byte-stable, committed, and the same on every forge.
type Lock struct {
	Version     int        `json:"version"`
	FleetDigest string     `json:"fleet_digest"`
	Repos       []LockRepo `json:"repos"`
}

// LockRepo is one repository as it is supposed to exist on a forge.
type LockRepo struct {
	Name          string   `json:"name"`
	DefaultBranch string   `json:"default_branch"`
	Visibility    string   `json:"visibility"`
	Topics        []string `json:"topics"`
	Archived      bool     `json:"archived"`
	Empty         bool     `json:"empty"`
	Tags          []string `json:"tags,omitempty"`
	// Baseline is the seed commit. Empty for an Empty repository.
	Baseline string `json:"baseline"`
}

// NewLock resolves a spec into a lock. baselines maps repository name to seed commit SHA.
func NewLock(spec *Spec, digest string, baselines map[string]string) *Lock {
	l := &Lock{Version: SpecVersion, FleetDigest: digest}
	for _, r := range spec.Repos {
		l.Repos = append(l.Repos, LockRepo{
			Name:          r.Name,
			DefaultBranch: r.DefaultBranch,
			Visibility:    r.Visibility,
			Topics:        r.Topics,
			Archived:      r.Archived,
			Empty:         r.Empty,
			Tags:          r.Tags,
			Baseline:      baselines[r.Name],
		})
	}
	return l
}

// Repo returns the entry for a repository name.
func (l *Lock) Repo(name string) (LockRepo, bool) {
	for _, r := range l.Repos {
		if r.Name == name {
			return r, true
		}
	}
	return LockRepo{}, false
}

// Marshal renders the lock as formatted JSON with a trailing newline, so that it is
// diff-friendly and stable under `git diff`.
func (l *Lock) Marshal() ([]byte, error) {
	raw, err := json.MarshalIndent(l, "", "  ")
	if err != nil {
		return nil, err
	}
	return append(raw, '\n'), nil
}

// WriteFile writes the lock to path.
func (l *Lock) WriteFile(path string) error {
	raw, err := l.Marshal()
	if err != nil {
		return err
	}
	return os.WriteFile(path, raw, 0o644)
}

// ReadLock reads and parses a lock file.
func ReadLock(path string) (*Lock, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return ParseLock(raw)
}

// ParseLock decodes a lock from JSON.
//
// An empty lock is rejected rather than returned. Everything downstream iterates over this
// list, so an empty one would make every check run zero times and report success against a
// completely unseeded sandbox.
func ParseLock(raw []byte) (*Lock, error) {
	var l Lock
	if err := json.Unmarshal(raw, &l); err != nil {
		return nil, fmt.Errorf("parse lock: %w", err)
	}
	if l.Version != SpecVersion {
		return nil, fmt.Errorf("parse lock: version is %d, want %d", l.Version, SpecVersion)
	}
	if len(l.Repos) == 0 {
		return nil, fmt.Errorf("parse lock: no repositories; the lock is empty or truncated")
	}
	return &l, nil
}
