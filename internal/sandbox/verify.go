package sandbox

import (
	"context"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
	"github.com/repoplane/forgelab/internal/seed"
)

// state is one declared repository compared against the lock.
type state struct {
	want fleet.LockRepo
	live forge.Repo

	// guards are reasons forgelab must not touch this repository. Exit 2.
	guards []string
	// drift is everything reset can repair. Exit 1.
	drift []string

	// What reset needs in order to repair it.
	refsDirty     bool
	extraBranches []string
	extraTags     []string
	requests      []forge.Request
}

// report is the whole fleet compared against the lock.
type report []*state

func (r report) guards() (out []string) {
	for _, s := range r {
		for _, g := range s.guards {
			out = append(out, s.want.Name+": "+g)
		}
	}
	return out
}

func (r report) drifted() (out []*state) {
	for _, s := range r {
		if len(s.drift) > 0 {
			out = append(out, s)
		}
	}
	return out
}

// err turns a report into the error a command should return.
func (r report) err(e *Env) error {
	if g := r.guards(); len(g) > 0 {
		return &GuardError{Msg: "guard failure:\n  " + strings.Join(g, "\n  ")}
	}
	if d := r.drifted(); len(d) > 0 {
		var lines []string
		for _, s := range d {
			lines = append(lines, s.want.Name+": "+strings.Join(s.drift, "; "))
		}
		return &DriftError{Msg: fmt.Sprintf("drift (run `forgelab reset --sandbox %s`):\n  %s",
			e.Sandbox.Name, strings.Join(lines, "\n  "))}
	}
	return nil
}

// Verify asserts the sandbox matches the lock. Read-only.
func (e *Env) Verify(ctx context.Context) error {
	lock, err := e.loadLock()
	if err != nil {
		return err
	}
	rep, err := e.compare(ctx, lock)
	if err != nil {
		return err
	}
	if err := rep.err(e); err != nil {
		return err
	}
	e.printf("ok: %d repositories match %s\n", len(lock.Repos), fleet.LockFile)
	return nil
}

// compare looks every declared repository up by name: about five reads each, no writes.
func (e *Env) compare(ctx context.Context, lock *fleet.Lock) (report, error) {
	rep := make(report, len(lock.Repos))
	for i, want := range lock.Repos {
		rep[i] = &state{want: want}
	}
	err := forEach(ctx, rep, func(ctx context.Context, s *state) error {
		if err := e.compareOne(ctx, s); err != nil {
			return fmt.Errorf("%s: %w", s.want.Name, err)
		}
		return nil
	})
	return rep, err
}

func (e *Env) compareOne(ctx context.Context, s *state) error {
	want := s.want
	live, found, err := e.Forge.Get(ctx, want.Name)
	if err != nil {
		return err
	}
	if !found {
		s.guards = append(s.guards, fmt.Sprintf("missing from %s: run `forgelab apply --sandbox %s`",
			e.Sandbox.Org, e.Sandbox.Name))
		return nil
	}
	s.live = live
	if !slices.Contains(live.Topics, e.Sandbox.MarkerTopic) {
		s.guards = append(s.guards, fmt.Sprintf("exists without the %q topic, so it is not forgelab's: refusing to touch it",
			e.Sandbox.MarkerTopic))
		return nil
	}

	if live.Visibility != want.Visibility {
		s.drift = append(s.drift, fmt.Sprintf("visibility is %s, want %s", live.Visibility, want.Visibility))
	}
	if live.Archived != want.Archived {
		s.drift = append(s.drift, fmt.Sprintf("archived is %t, want %t", live.Archived, want.Archived))
	}
	if got := withoutMarker(live.Topics, e.Sandbox.MarkerTopic); !slices.Equal(got, want.Topics) {
		s.drift = append(s.drift, fmt.Sprintf("topics are %v, want %v", got, want.Topics))
	}

	if want.Empty {
		// A forge will not delete a repository's last branch, so there is no way back to
		// "no commits" short of deleting the repository -- which reset may not do.
		if !live.Empty {
			s.guards = append(s.guards, "is no longer empty: delete it on the forge, then run `forgelab apply`")
		}
		return nil
	}
	if live.Empty {
		s.guards = append(s.guards, fmt.Sprintf("was never seeded: run `forgelab apply --sandbox %s`", e.Sandbox.Name))
		return nil
	}

	if live.DefaultBranch != want.DefaultBranch {
		s.drift = append(s.drift, fmt.Sprintf("default branch is %s, want %s", live.DefaultBranch, want.DefaultBranch))
	}

	tags, err := e.Forge.Tags(ctx, want.Name)
	if err != nil {
		return err
	}
	baselineOK := false
	for _, t := range tags {
		switch {
		case t.Name == seed.BaselineTag:
			baselineOK = t.SHA == want.Baseline
		case !slices.Contains(want.Tags, t.Name):
			s.extraTags = append(s.extraTags, t.Name)
			s.drift = append(s.drift, "extra tag "+t.Name)
		case t.SHA != want.Baseline:
			s.refsDirty = true
			s.drift = append(s.drift, "tag "+t.Name+" moved")
		}
	}
	if !baselineOK {
		s.guards = append(s.guards, fmt.Sprintf("tag %s is missing or is not %s: run `forgelab apply --sandbox %s`",
			seed.BaselineTag, short(want.Baseline), e.Sandbox.Name))
		return nil
	}
	for _, t := range want.Tags {
		if !slices.ContainsFunc(tags, func(r forge.Ref) bool { return r.Name == t }) {
			s.refsDirty = true
			s.drift = append(s.drift, "tag "+t+" is missing")
		}
	}

	branches, err := e.Forge.Branches(ctx, want.Name)
	if err != nil {
		return err
	}
	headOK := false
	for _, b := range branches {
		if b.Name != want.DefaultBranch {
			s.extraBranches = append(s.extraBranches, b.Name)
			s.drift = append(s.drift, "extra branch "+b.Name)
			continue
		}
		headOK = b.SHA == want.Baseline
		if !headOK {
			s.drift = append(s.drift, fmt.Sprintf("%s is at %s, want %s", b.Name, short(b.SHA), short(want.Baseline)))
		}
	}
	if !headOK {
		s.refsDirty = true
		if !slices.ContainsFunc(branches, func(r forge.Ref) bool { return r.Name == want.DefaultBranch }) {
			s.drift = append(s.drift, "branch "+want.DefaultBranch+" is missing")
		}
	}

	s.requests, err = e.Forge.OpenRequests(ctx, want.Name)
	if err != nil {
		return err
	}
	// Open, never "how many exist": request numbers are monotonic and closed requests are
	// permanent, so a count can never be reset.
	if n := len(s.requests); n > 0 {
		s.drift = append(s.drift, fmt.Sprintf("%d open request(s)", n))
	}
	return nil
}

// waitReady blocks until each repository reports its default branch at the baseline.
//
// Health is not readiness: a push returns before the forge has finished reflecting it, and
// for a short window afterwards a branch listing answers 200 with a null body -- not an
// error, just silently empty. Read naively that is "this repository has no branches".
func (e *Env) waitReady(ctx context.Context, repos []fleet.LockRepo) error {
	deadline := time.Now().Add(60 * time.Second)
	return forEach(ctx, repos, func(ctx context.Context, want fleet.LockRepo) error {
		if want.Empty {
			return nil
		}
		for {
			branches, err := e.Forge.Branches(ctx, want.Name)
			if err == nil && slices.Contains(branches, forge.Ref{Name: want.DefaultBranch, SHA: want.Baseline}) {
				return nil
			}
			if time.Now().After(deadline) {
				if err != nil {
					return fmt.Errorf("%s not ready: %w", want.Name, err)
				}
				return fmt.Errorf("%s never reported %s at %s", want.Name, want.DefaultBranch, short(want.Baseline))
			}
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(25 * time.Millisecond):
			}
		}
	})
}

func withoutMarker(topics []string, marker string) []string {
	out := []string{}
	for _, t := range topics {
		if t != marker {
			out = append(out, t)
		}
	}
	slices.Sort(out)
	return out
}

func withMarker(topics []string, marker string) []string {
	return append(slices.Clone(topics), marker)
}

// short abbreviates for display without assuming the SHA is well-formed.
func short(sha string) string {
	if len(sha) > 12 {
		return sha[:12]
	}
	return sha
}
