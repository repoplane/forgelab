package sandbox

import (
	"strings"
	"testing"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
)

func spec(names ...string) *fleet.Spec {
	s := &fleet.Spec{}
	for _, n := range names {
		s.Repos = append(s.Repos, fleet.Repo{Name: n})
	}
	return s
}

func show(ns []string) string {
	out := make([]string, len(ns))
	for i, n := range ns {
		if n == "" {
			out[i] = "<default project>"
			continue
		}
		out[i] = n
	}
	return strings.Join(out, ", ")
}

// The regression. A fleet that lives entirely under namespaces never put anything in the home a
// forge gives to repositories without one, so destroy must not remove it. On Azure DevOps that
// home is the default project, which forgelab made for another fleet and marked as its own --
// destroy would have taken it, and every repository in it, on the word of a fleet that never
// touched it.
func TestDoomedNamespacesSparesAnUnusedDefaultProject(t *testing.T) {
	s := spec("acme/payments/api", "acme/platform/observability/collector", "acme/handbook")
	got := doomedNamespaces(s, 1, "fleet")
	for _, ns := range got {
		if ns == "" {
			t.Fatalf("the default project is not this fleet's to remove: %s", show(got))
		}
	}
	if len(got) != 1 || got[0] != "acme" {
		t.Errorf("want only the acme namespace, got: %s", show(got))
	}
}

// The other direction, which must keep working: a repository with no namespace does live in that
// home, so an interrupted destroy can still finish the job by removing it.
func TestDoomedNamespacesRemovesAUsedDefaultProject(t *testing.T) {
	got := doomedNamespaces(spec("dotfiles", "services/api"), 1, "fleet")
	if len(got) != 2 || got[0] != "services" || got[1] != "" {
		t.Fatalf("want [services, <default project>], got: %s", show(got))
	}
}

// A forge with no default project has no such home, so there is nothing extra to remove however
// the fleet is laid out.
func TestDoomedNamespacesWithoutADefaultProject(t *testing.T) {
	for _, s := range []*fleet.Spec{spec("dotfiles"), spec("acme/payments/api")} {
		if got := doomedNamespaces(s, forge.AnyDepth, ""); len(got) == 1 && got[0] == "" {
			t.Errorf("nothing to remove without a default project, got: %s", show(got))
		}
	}
}

// Depth decides which namespaces the forge holds as things of its own. Azure DevOps keeps only
// the first segment; GitLab keeps every one; GitHub and Forgejo keep none, a namespace being only
// a prefix of the name there.
func TestDoomedNamespacesFollowsDepth(t *testing.T) {
	s := spec("acme/payments/gateway/api", "acme/handbook")
	for _, tc := range []struct {
		name  string
		depth int
		want  string
	}{
		{"azure devops", 1, "acme"},
		{"gitlab", forge.AnyDepth, "acme, acme/payments, acme/payments/gateway"},
		{"github and forgejo", 0, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := show(doomedNamespaces(s, tc.depth, "")); got != tc.want {
				t.Errorf("got %q, want %q", got, tc.want)
			}
		})
	}
}
