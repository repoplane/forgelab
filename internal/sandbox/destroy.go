package sandbox

import (
	"context"
	"fmt"
	"slices"
	"strings"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
)

// Destroy deletes the declared repositories that carry the marker topic, and nothing else:
// not undeclared repositories, not a same-named repository forgelab did not create, and
// not the organisation. The namespaces they sat in go too, with whatever else is in them by
// then, but only those forgelab made: the marker means the same on a namespace as on a
// repository.
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
	// The namespaces this forge holds as such, rather than as a prefix of the name.
	var namespaces []string
	if depth := e.Forge.Caps().NamespaceDepth; depth != 0 {
		for _, ns := range spec.Namespaces() {
			if depth == forge.AnyDepth || strings.Count(ns, "/") < depth {
				namespaces = append(namespaces, ns)
			}
		}
	}
	// Where a forge makes the home of the repositories without a namespace, that goes last.
	if e.Sandbox.DefaultProject != "" {
		namespaces = append(namespaces, "")
	}
	label := func(ns string) string {
		if ns == "" {
			return e.Sandbox.DefaultProject + "/"
		}
		return ns + "/"
	}
	// No question without a declared repository to lose. What is left to remove then is
	// namespaces of forgelab's own making, which is how an interrupted destroy is finished.
	if len(doomed) > 0 {
		for _, ns := range namespaces {
			e.printf("  - %-28s namespace, with all it holds: only if forgelab made it\n", label(ns))
		}
		e.printf("\n  Deletion cannot be undone: pull requests, issue numbers and history go with them.\n")
		if len(namespaces) > 0 {
			e.printf("  Only these %d and those namespaces are deleted; anything else in %s is left alone.\n", len(doomed), e.Sandbox.Org)
		} else {
			e.printf("  Only these %d are deleted; anything else in %s is left alone.\n", len(doomed), e.Sandbox.Org)
		}
		if err := e.confirm(); err != nil {
			return err
		}
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
	// Outermost first: one that forgelab made takes everything inside with it, and one it did
	// not make is kept while forgelab's own inside it still go.
	removed := 0
	for _, ns := range namespaces {
		gone, kept, err := e.Forge.DeleteNamespace(ctx, ns)
		if err != nil {
			return fmt.Errorf("delete namespace %s: %w", ns, err)
		}
		if gone {
			removed++
			e.printf("  - %-28s namespace removed\n", label(ns))
		}
		if kept != "" {
			e.printf("  ! %-28s kept: %s\n", label(ns), kept)
		}
	}
	switch {
	case len(doomed) == 0 && removed == 0:
		e.printf("  nothing to delete\n\n")
	case removed == 0:
		e.printf("ok: %d repositories deleted\n", len(doomed))
	case removed == 1:
		e.printf("ok: %d repositories deleted, 1 namespace removed\n", len(doomed))
	default:
		e.printf("ok: %d repositories deleted, %d namespaces removed\n", len(doomed), removed)
	}
	return nil
}
