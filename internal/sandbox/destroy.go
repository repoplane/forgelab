package sandbox

import (
	"context"
	"fmt"
	"slices"

	"github.com/repoplane/forgelab/internal/fleet"
)

// Destroy deletes the declared repositories that carry the marker topic, and nothing else:
// not undeclared repositories, not a same-named repository forgelab did not create, and
// not the organisation. The namespaces they sat in go too, deepest first, but only those
// left with nothing at all in them.
func (e *Env) Destroy(ctx context.Context) error {
	spec, err := fleet.LoadSpec(e.fsys)
	if err != nil {
		return err
	}

	type target struct {
		name   string
		delete bool
		skip   string
	}
	targets := make([]*target, len(spec.Repos))
	for i, r := range spec.Repos {
		targets[i] = &target{name: r.Name}
	}
	err = forEach(ctx, targets, func(ctx context.Context, t *target) error {
		live, found, err := e.Forge.Get(ctx, t.name)
		switch {
		case err != nil:
			return fmt.Errorf("%s: %w", t.name, err)
		case !found:
		case e.Forge.Caps().Topics && !slices.Contains(live.Topics, e.Sandbox.MarkerTopic):
			t.skip = fmt.Sprintf("has no %q topic, so it is not forgelab's", e.Sandbox.MarkerTopic)
		default:
			t.delete = true
		}
		return nil
	})
	if err != nil {
		return err
	}

	e.header("DESTROY", len(targets))
	var doomed []*target
	for _, t := range targets {
		switch {
		case t.delete:
			e.printf("  - %s\n", t.name)
			doomed = append(doomed, t)
		case t.skip != "":
			e.printf("  ! %-28s left alone: %s\n", t.name, t.skip)
		}
	}
	// Asked for even when no repository is left, so that an interrupted destroy is finished
	// by running it again.
	var namespaces []string
	if e.Forge.Caps().Namespaces {
		namespaces = spec.Namespaces()
	}
	if len(doomed) == 0 && len(namespaces) == 0 {
		e.printf("  nothing to delete\n\n")
		return nil
	}
	for _, ns := range namespaces {
		e.printf("  - %-28s namespace, removed only if left empty\n", ns+"/")
	}
	if len(doomed) == 0 {
		e.printf("\n  No declared repository is left. Anything else in %s is left alone.\n", e.Sandbox.Scope())
	} else {
		e.printf("\n  Deletion cannot be undone: pull requests, issue numbers and history go with them.\n")
		e.printf("  Only these %d are deleted; anything else in %s is left alone.\n", len(doomed), e.Sandbox.Scope())
	}
	if err := e.confirm(); err != nil {
		return err
	}

	err = forEach(ctx, doomed, func(ctx context.Context, t *target) error {
		if err := e.Forge.Delete(ctx, t.name); err != nil {
			return fmt.Errorf("delete %s: %w", t.name, err)
		}
		return nil
	})
	if err != nil {
		return err
	}
	// One at a time: a namespace is empty only once the ones below it are gone.
	for _, ns := range namespaces {
		kept, err := e.Forge.DeleteNamespace(ctx, ns)
		if err != nil {
			return fmt.Errorf("delete namespace %s: %w", ns, err)
		}
		if kept {
			e.printf("  ! %-28s kept: not empty\n", ns+"/")
		}
	}
	e.printf("ok: %d repositories deleted\n", len(doomed))
	return nil
}
