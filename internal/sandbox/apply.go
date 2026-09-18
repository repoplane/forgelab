package sandbox

import (
	"context"
	"fmt"
	"slices"
	"strings"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
	"github.com/repoplane/forgelab/internal/seed"
)

// change is what apply would do to one declared repository.
type change struct {
	repo  fleet.Repo
	built *seed.Built // nil for an empty repository
	live  forge.Repo

	create bool
	push   bool     // content or baseline tag differs from the lock
	notes  []string // human-readable settings differences
}

func (c *change) needed() bool { return c.create || c.push || len(c.notes) > 0 }

// Plan prints what apply would do. No writes, to the forge or to disk.
func (e *Env) Plan(ctx context.Context) error {
	_, changes, err := e.plan(ctx)
	closeAll(changes)
	if err != nil {
		return err
	}
	e.printPlan("PLAN", changes)
	return nil
}

// Apply converges the sandbox on the fleet and writes the lock. It creates and updates;
// it never deletes, and it never adopts a repository it did not create.
//
// It is idempotent, so a run interrupted for any reason resumes by being run again.
func (e *Env) Apply(ctx context.Context) error {
	lock, changes, err := e.plan(ctx)
	defer closeAll(changes)
	if err != nil {
		return err
	}

	var todo []*change
	for _, c := range changes {
		if c.needed() {
			todo = append(todo, c)
		}
	}
	e.printPlan("APPLY", changes)
	// No prompt: apply never deletes, touches only declared repositories that carry the
	// marker, and has just printed exactly what it is about to do.
	if len(todo) > 0 {
		if err := e.Forge.EnsureOrg(ctx); err != nil {
			return fmt.Errorf("org %s: %w", e.Sandbox.Org, err)
		}
		err := forEach(ctx, todo, func(ctx context.Context, c *change) error {
			if err := e.applyOne(ctx, c); err != nil {
				return fmt.Errorf("%s: %w", c.repo.Name, err)
			}
			e.debugf("  applied %s\n", c.repo.Name)
			return nil
		})
		if err != nil {
			return err
		}
	}

	if err := lock.WriteFile(e.lockPath()); err != nil {
		return err
	}
	if err := e.waitReady(ctx, lock.Repos); err != nil {
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

// plan resolves the fleet into a lock -- building every commit locally, which needs git
// and nothing else -- and diffs it against the forge.
func (e *Env) plan(ctx context.Context) (*fleet.Lock, []*change, error) {
	spec, err := fleet.LoadSpec(e.fsys)
	if err != nil {
		return nil, nil, err
	}
	digest, err := fleet.Digest(e.fsys)
	if err != nil {
		return nil, nil, err
	}

	changes := make([]*change, len(spec.Repos))
	for i, r := range spec.Repos {
		// The marker belongs to the sandbox. A fleet that declared it too would make the
		// topic comparison unable to tell the two apart.
		if slices.Contains(r.Topics, e.Sandbox.MarkerTopic) {
			return nil, nil, fmt.Errorf("%s: topic %q is the sandbox marker and may not be declared",
				r.Name, e.Sandbox.MarkerTopic)
		}
		changes[i] = &change{repo: r}
	}

	err = forEach(ctx, changes, func(ctx context.Context, c *change) error {
		if c.repo.Empty {
			return nil
		}
		built, err := seed.Build(ctx, e.fsys, c.repo, spec.Git)
		c.built = built
		return err
	})
	if err != nil {
		return nil, changes, err
	}
	baselines := map[string]string{}
	for _, c := range changes {
		if c.built != nil {
			baselines[c.repo.Name] = c.built.SHA
		}
	}
	lock := fleet.NewLock(spec, digest, baselines)

	err = forEach(ctx, changes, func(ctx context.Context, c *change) error {
		if err := e.diffOne(ctx, c); err != nil {
			return fmt.Errorf("%s: %w", c.repo.Name, err)
		}
		return nil
	})
	return lock, changes, err
}

func (e *Env) diffOne(ctx context.Context, c *change) error {
	want := c.repo
	live, found, err := e.Forge.Get(ctx, want.Name)
	if err != nil {
		return err
	}
	if !found {
		c.create = true
		return nil
	}
	// A repository with this name that forgelab did not create is somebody else's.
	if !slices.Contains(live.Topics, e.Sandbox.MarkerTopic) {
		return &GuardError{Msg: fmt.Sprintf(
			"%s/%s already exists without the %q topic: it is not forgelab's, and apply never adopts. Rename the fixture or remove that repository",
			e.Sandbox.Org, want.Name, e.Sandbox.MarkerTopic)}
	}
	c.live = live

	if live.Visibility != want.Visibility {
		c.notes = append(c.notes, "visibility: "+live.Visibility+" → "+want.Visibility)
	}
	if live.Archived != want.Archived {
		c.notes = append(c.notes, fmt.Sprintf("archived: %t → %t", live.Archived, want.Archived))
	}
	if got := withoutMarker(live.Topics, e.Sandbox.MarkerTopic); !slices.Equal(got, want.Topics) {
		c.notes = append(c.notes, fmt.Sprintf("topics: %v → %v", got, want.Topics))
	}
	if want.Empty {
		if !live.Empty {
			return &GuardError{Msg: want.Name + " is no longer empty: delete it on the forge, then run apply again"}
		}
		return nil
	}
	if live.DefaultBranch != want.DefaultBranch {
		c.notes = append(c.notes, "default branch: "+live.DefaultBranch+" → "+want.DefaultBranch)
	}
	if live.Empty {
		c.push = true // created by an interrupted apply, never seeded
		return nil
	}
	tags, err := e.Forge.Tags(ctx, want.Name)
	if err != nil {
		return err
	}
	c.push = !slices.Contains(tags, forge.Ref{Name: seed.BaselineTag, SHA: c.built.SHA})
	return nil
}

func (e *Env) applyOne(ctx context.Context, c *change) error {
	want := c.repo
	name := want.Name

	if c.create {
		if err := e.Forge.Create(ctx, name, want.Visibility, want.DefaultBranch); err != nil {
			return fmt.Errorf("create: %w", err)
		}
		// The marker goes on immediately, before any content: an interrupted apply leaves
		// repositories behind, and the re-run has to recognise them as its own.
		if err := e.Forge.SetTopics(ctx, name, withMarker(want.Topics, e.Sandbox.MarkerTopic)); err != nil {
			return fmt.Errorf("set topics: %w", err)
		}
	}

	// An archived repository rejects every write, so it is lifted for the duration and
	// restored last.
	if c.live.Archived {
		if err := e.Forge.UpdateSettings(ctx, name, forge.Settings{Archived: ptr(false)}); err != nil {
			return fmt.Errorf("unarchive: %w", err)
		}
		c.live.Archived = false
	}

	if (c.create || c.push) && c.built != nil {
		url, err := e.Forge.GitURL(name)
		if err != nil {
			return err
		}
		if err := c.built.Push(ctx, url); err != nil {
			return err
		}
	}

	// Archived goes last, on its own: once set, nothing else can be.
	settings := forge.Settings{Visibility: ptr(want.Visibility)}
	if !want.Empty {
		// A repository created empty adopts the instance default branch, so a fixture
		// that wants `master` needs it set after the push has created the ref.
		settings.DefaultBranch = ptr(want.DefaultBranch)
	}
	if err := e.Forge.UpdateSettings(ctx, name, settings); err != nil {
		return fmt.Errorf("settings: %w", err)
	}
	if !c.create {
		if err := e.Forge.SetTopics(ctx, name, withMarker(want.Topics, e.Sandbox.MarkerTopic)); err != nil {
			return fmt.Errorf("set topics: %w", err)
		}
	}
	if want.Archived != c.live.Archived {
		if err := e.Forge.UpdateSettings(ctx, name, forge.Settings{Archived: ptr(want.Archived)}); err != nil {
			return fmt.Errorf("archive: %w", err)
		}
	}
	return nil
}

func (e *Env) printPlan(verb string, changes []*change) {
	e.header(verb, len(changes))
	n := 0
	for _, c := range changes {
		switch {
		case c.create:
			e.printf("  + %-28s create\n", c.repo.Name)
		case c.needed():
			notes := c.notes
			if c.push {
				notes = append([]string{"content: re-seed"}, notes...)
			}
			e.printf("  ~ %-28s %s\n", c.repo.Name, strings.Join(notes, ", "))
		default:
			continue
		}
		n++
	}
	if n == 0 {
		e.printf("  nothing to do\n")
	}
	e.printf("\n")
}

func closeAll(changes []*change) {
	for _, c := range changes {
		if c != nil {
			c.built.Close()
		}
	}
}
