package sandbox

import (
	"context"
	"fmt"
	"strings"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
	"github.com/repoplane/forgelab/internal/seed"
)

// Reset puts every drifted repository back to its baseline.
//
// It cannot create or delete a repository -- not "does not", cannot: it has no code path
// that does either. Reset runs constantly and unattended, and a missing repository means
// the fleet drifted or the tool is pointed at the wrong place. Both want a human, so that
// is a guard failure telling them to run apply.
//
// It writes only to repositories that differ from the lock. Every step is declarative --
// "make it equal X", never "apply this delta" -- so a reset that dies halfway is safely
// re-runnable.
func (e *Env) Reset(ctx context.Context) error {
	lock, err := e.loadLock()
	if err != nil {
		return err
	}
	rep, err := e.compare(ctx, lock)
	if err != nil {
		return err
	}
	if g := rep.guards(); len(g) > 0 {
		return &GuardError{Msg: "guard failure, nothing was reset:\n  " + strings.Join(g, "\n  ")}
	}

	touched := rep.drifted()
	if len(touched) == 0 {
		e.printf("ok: nothing to reset\n")
		return nil
	}
	err = forEach(ctx, touched, func(ctx context.Context, s *state) error {
		if err := e.resetOne(ctx, s); err != nil {
			return fmt.Errorf("%s: %w", s.want.Name, err)
		}
		e.printf("reset %s (%s)\n", s.want.Name, strings.Join(s.drift, "; "))
		return nil
	})
	if err != nil {
		return err
	}

	var wants []fleet.LockRepo
	for _, s := range touched {
		wants = append(wants, s.want)
	}
	if err := e.waitReady(ctx, wants); err != nil {
		return err
	}
	rep, err = e.compare(ctx, lock)
	if err != nil {
		return err
	}
	if err := rep.err(e); err != nil {
		return err
	}
	e.printf("ok: %d of %d repositories reset\n", len(touched), len(lock.Repos))
	return nil
}

// resetOne's order is load-bearing: an archived repository refuses every write, the
// default branch must exist before it can be the default, and a branch cannot be deleted
// while it is the default or while an open request still points at it.
func (e *Env) resetOne(ctx context.Context, s *state) error {
	want, name := s.want, s.want.Name

	if s.live.Archived {
		if err := e.Forge.UpdateSettings(ctx, name, forge.Settings{Archived: ptr(false)}); err != nil {
			return fmt.Errorf("unarchive: %w", err)
		}
	}

	if s.refsDirty {
		url, err := e.Forge.GitURL(name)
		if err != nil {
			return err
		}
		if err := seed.ResetToBaseline(ctx, url, want.DefaultBranch, want.Tags); err != nil {
			return err
		}
	}

	settings := forge.Settings{Visibility: ptr(want.Visibility)}
	if !want.Empty {
		settings.DefaultBranch = ptr(want.DefaultBranch)
	}
	if err := e.Forge.UpdateSettings(ctx, name, settings); err != nil {
		return fmt.Errorf("settings: %w", err)
	}

	for _, r := range s.requests {
		if err := e.Forge.CloseRequest(ctx, name, r.Number); err != nil {
			return fmt.Errorf("close request #%d: %w", r.Number, err)
		}
	}
	for _, b := range s.extraBranches {
		if err := e.Forge.DeleteBranch(ctx, name, b); err != nil {
			return fmt.Errorf("delete branch %s: %w", b, err)
		}
	}
	for _, t := range s.extraTags {
		if err := e.Forge.DeleteTag(ctx, name, t); err != nil {
			return fmt.Errorf("delete tag %s: %w", t, err)
		}
	}

	if err := e.Forge.SetTopics(ctx, name, withMarker(want.Topics, e.Sandbox.MarkerTopic)); err != nil {
		return fmt.Errorf("set topics: %w", err)
	}
	if want.Archived {
		if err := e.Forge.UpdateSettings(ctx, name, forge.Settings{Archived: ptr(true)}); err != nil {
			return fmt.Errorf("archive: %w", err)
		}
	}
	return nil
}
