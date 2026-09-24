package gitlab

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/repoplane/forgelab/internal/forge"
)

var ctx = context.Background()

// serve starts a fake API for the nested group acme-sandbox/services.
func serve(t *testing.T, h http.HandlerFunc) (*Client, *[]string) {
	t.Helper()
	var seen []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = append(seen, r.Method+" "+r.URL.EscapedPath())
		h(w, r)
	}))
	t.Cleanup(srv.Close)
	c, err := New(srv.URL, "acme-sandbox/services", "s3cret", nil)
	if err != nil {
		t.Fatal(err)
	}
	c.sleep = func(context.Context, time.Duration) error { return nil }
	return c, &seen
}

const svc = "/api/v4/projects/acme-sandbox%2Fservices%2Fsvc"

func TestGet(t *testing.T) {
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("PRIVATE-TOKEN") != "s3cret" {
			t.Errorf("no token on %s", r.URL.Path)
		}
		switch r.URL.EscapedPath() {
		case svc:
			fmt.Fprint(w, `{"path_with_namespace":"acme-sandbox/services/svc","default_branch":"main","visibility":"private","archived":true,"empty_repo":false,"topics":["a"]}`)
		// just pushed: empty_repo has not caught up, the repository has
		case "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh":
			fmt.Fprint(w, `{"path_with_namespace":"acme-sandbox/services/fresh","visibility":"public","empty_repo":true}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh/repository/branches":
			fmt.Fprint(w, `[{"name":"main","commit":{"id":"abc"}}]`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fbare":
			fmt.Fprint(w, `{"path_with_namespace":"acme-sandbox/services/bare","visibility":"private","empty_repo":true}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fbare/repository/branches":
			fmt.Fprint(w, `[]`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fold-name":
			fmt.Fprint(w, `{"path_with_namespace":"acme-sandbox/services/new-name"}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fdoomed":
			fmt.Fprint(w, `{"path_with_namespace":"acme-sandbox/services/doomed","marked_for_deletion_on":"2026-09-26"}`)
		// a namespace is more of the same path; its leaf alone says nothing
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fcore%2Fapi":
			fmt.Fprint(w, `{"path":"api","path_with_namespace":"acme-sandbox/services/core/api","visibility":"private"}`)
		default:
			http.NotFound(w, r)
		}
	})

	if r, found, err := c.Get(ctx, "core/api"); err != nil || !found || r.Name != "core/api" {
		t.Errorf("core/api: %+v found=%t err=%v", r, found, err)
	}
	*seen = nil
	r, found, err := c.Get(ctx, "svc")
	if err != nil || !found || r.Visibility != "private" || !r.Archived || r.Empty || len(r.Topics) != 1 {
		t.Errorf("svc: %+v found=%t err=%v", r, found, err)
	}
	if (*seen)[0] != "GET "+svc {
		t.Errorf("the nested project path must travel as one %%2F-encoded segment, got %q", (*seen)[0])
	}
	if r, _, _ := c.Get(ctx, "fresh"); r.Empty {
		t.Error("fresh: a lagging empty_repo flag must not read as unseeded")
	}
	if r, _, _ := c.Get(ctx, "bare"); !r.Empty {
		t.Error("bare: want empty")
	}
	for _, name := range []string{"missing", "old-name", "doomed"} {
		if _, found, err := c.Get(ctx, name); found || err != nil {
			t.Errorf("%s must read as missing: found=%t err=%v", name, found, err)
		}
	}
}

func TestListFollowsNextPage(t *testing.T) {
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("page") == "1" {
			w.Header().Set("X-Next-Page", "2")
			fmt.Fprint(w, `[{"iid":1,"title":"a"},{"iid":2,"title":"b"}]`)
			return
		}
		fmt.Fprint(w, `[{"iid":3,"title":"c"}]`)
	})
	reqs, err := c.OpenRequests(ctx, "svc")
	if err != nil || len(reqs) != 3 || reqs[2].Number != 3 {
		t.Errorf("reqs=%+v err=%v", reqs, err)
	}
}

// Order matters: an archived project rejects every other write, and the default branch is
// unprotected because reset has to force-push it.
func TestUpdateSettingsOrder(t *testing.T) {
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if strings.Contains(r.URL.EscapedPath(), "/protected_branches/") {
			http.NotFound(w, r) // not protected is fine
		}
	})
	branch, vis, yes, no := "release/1", "private", true, false

	if err := c.UpdateSettings(ctx, "svc", forge.Settings{DefaultBranch: &branch, Visibility: &vis, Archived: &no}); err != nil {
		t.Fatal(err)
	}
	if err := c.UpdateSettings(ctx, "svc", forge.Settings{Archived: &yes}); err != nil {
		t.Fatal(err)
	}
	want := []string{
		"POST " + svc + "/unarchive",
		"PUT " + svc,
		"DELETE " + svc + "/protected_branches/release%2F1",
		"POST " + svc + "/protected_branches",
		"POST " + svc + "/archive",
	}
	if strings.Join(*seen, "\n") != strings.Join(want, "\n") {
		t.Errorf("got:\n%s\nwant:\n%s", strings.Join(*seen, "\n"), strings.Join(want, "\n"))
	}
}

// The project's own rule is removed and replaced by one that permits force-push.
func TestAllowForcePush(t *testing.T) {
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	})
	if err := c.AllowForcePush(ctx, "svc", "release/1"); err != nil {
		t.Fatal(err)
	}
	want := []string{
		"DELETE " + svc + "/protected_branches/release%2F1",
		"POST " + svc + "/protected_branches",
	}
	if strings.Join(*seen, "\n") != strings.Join(want, "\n") {
		t.Errorf("got:\n%s\nwant:\n%s", strings.Join(*seen, "\n"), strings.Join(want, "\n"))
	}
}

// Protection inherited from the group never appears as a project rule: the branch reports
// protected while the rule list is empty, so the delete 404s and lifts nothing. The
// replacement is what actually permits the push, which makes the 404 the case that must not
// stop it -- one project in a 101-repository apply was protected exactly this way, and the
// delete-only version reported success and then failed on the push.
func TestAllowForcePushWhenProtectionIsInherited(t *testing.T) {
	var rule map[string]any
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodDelete {
			http.NotFound(w, r)
			return
		}
		_ = json.NewDecoder(r.Body).Decode(&rule)
		w.WriteHeader(http.StatusCreated)
	})
	if err := c.AllowForcePush(ctx, "svc", "main"); err != nil {
		t.Fatal(err)
	}
	if len(*seen) != 2 || (*seen)[1] != "POST "+svc+"/protected_branches" {
		t.Fatalf("a 404 on the delete must not skip the replacement: %v", *seen)
	}
	if rule["name"] != "main" || rule["allow_force_push"] != true {
		t.Errorf("the replacement rule does not permit force-push: %v", rule)
	}
}

func TestAllowForcePushRefused(t *testing.T) {
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusForbidden)
	})
	if err := c.AllowForcePush(ctx, "svc", "main"); err == nil {
		t.Error("a refused unprotect must surface")
	}
}

// gitlab.com only schedules a deletion; the second call makes it real.
func TestDeleteIsPermanent(t *testing.T) {
	var queries []string
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodDelete:
			queries = append(queries, r.URL.Query().Get("full_path"))
		case len(queries) == 0:
			fmt.Fprint(w, `{"id":42,"path_with_namespace":"acme-sandbox/services/svc"}`)
		default:
			fmt.Fprint(w, `{"id":42,"path_with_namespace":"acme-sandbox/services/svc-deleted-42","marked_for_deletion_on":"2026-09-26"}`)
		}
	})
	if err := c.Delete(ctx, "svc"); err != nil {
		t.Fatal(err)
	}
	want := "GET " + svc + "\nDELETE /api/v4/projects/42\nGET /api/v4/projects/42\nDELETE /api/v4/projects/42"
	if got := strings.Join(*seen, "\n"); got != want {
		t.Errorf("got:\n%s\nwant:\n%s", got, want)
	}
	if len(queries) != 2 || queries[0] != "" || queries[1] != "acme-sandbox/services/svc-deleted-42" {
		t.Errorf("the second DELETE must name the renamed path: %q", queries)
	}
}

func TestCreateAndRequests(t *testing.T) {
	var body map[string]any
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/api/v4/groups/acme-sandbox/services" {
			fmt.Fprint(w, `{"id":7}`)
			return
		}
		body = nil
		json.NewDecoder(r.Body).Decode(&body)
	})

	if err := c.Create(ctx, "svc", "private", "master", []string{"forgelab-managed"}); err != nil {
		t.Fatal(err)
	}
	if body["namespace_id"] != float64(7) || body["path"] != "svc" || body["visibility"] != "private" ||
		body["initialize_with_readme"] != false {
		t.Errorf("create: %+v", body)
	}
	c.Create(ctx, "other", "public", "main", nil)
	if n := strings.Count(strings.Join(*seen, "\n"), "/groups/"); n != 1 {
		t.Errorf("the group id must be looked up once, was %d times", n)
	}

	c.CloseRequest(ctx, "svc", 7)
	if body["state_event"] != "close" {
		t.Errorf("close: %+v", body)
	}
	c.DeleteBranch(ctx, "svc", "feature/run-42")
	if last := (*seen)[len(*seen)-1]; last != "DELETE "+svc+"/repository/branches/feature%2Frun-42" {
		t.Errorf("delete branch: %s", last)
	}

	u, _ := c.GitURL("svc")
	if !strings.HasPrefix(u, "http://oauth2:s3cret@") || !strings.HasSuffix(u, "/acme-sandbox/services/svc.git") {
		t.Errorf("git url: %q", u)
	}
}

func TestRateLimitIsWaitedOut(t *testing.T) {
	calls := 0
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if calls++; calls == 1 {
			w.Header().Set("Retry-After", "3")
			http.Error(w, "slow down", http.StatusTooManyRequests)
		}
	})
	if err := c.SetTopics(ctx, "svc", nil); err != nil || calls != 2 {
		t.Errorf("calls=%d err=%v", calls, err)
	}
}

// A namespace is a chain of subgroups: made on the way to the first project that needs
// them, each as visible as its parent, and never looked up twice.
func TestCreateMakesSubgroups(t *testing.T) {
	var posts []map[string]any
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodGet && r.URL.Path == "/api/v4/groups/acme-sandbox/services":
			fmt.Fprint(w, `{"id":7,"visibility":"public"}`)
		case r.Method == http.MethodGet:
			http.NotFound(w, r)
		default:
			var body map[string]any
			json.NewDecoder(r.Body).Decode(&body)
			posts = append(posts, body)
			fmt.Fprintf(w, `{"id":%d,"visibility":"public"}`, 7+len(posts))
		}
	})
	if err := c.Create(ctx, "platform/core/api", "private", "main", nil); err != nil {
		t.Fatal(err)
	}
	if err := c.Create(ctx, "platform/core/cli", "private", "main", nil); err != nil {
		t.Fatal(err)
	}
	want := []map[string]any{
		{"name": "platform", "path": "platform", "parent_id": float64(7), "visibility": "public", "description": forge.NamespaceMarker},
		{"name": "core", "path": "core", "parent_id": float64(8), "visibility": "public", "description": forge.NamespaceMarker},
	}
	if len(posts) != 4 || fmt.Sprint(posts[:2]) != fmt.Sprint(want) {
		t.Errorf("subgroups: %v", posts)
	}
	if posts[2]["path"] != "api" || posts[2]["namespace_id"] != float64(9) || posts[3]["namespace_id"] != float64(9) {
		t.Errorf("projects: %v", posts[2:])
	}
	if n := strings.Count(strings.Join(*seen, "\n"), "GET "); n != 3 {
		t.Errorf("want one lookup per group, got %d:\n%s", n, strings.Join(*seen, "\n"))
	}
}

// The marker alone decides: what a marked subgroup holds goes with it, as a marked
// repository's branches and requests do.
func TestDeleteNamespace(t *testing.T) {
	const core = "/api/v4/groups/acme-sandbox%2Fservices%2Fcore"
	for name, tc := range map[string]struct {
		group   string
		removed bool
		kept    string
		deletes int
	}{
		"ours":             {group: `{"id":9,"description":"forgelab-managed"}`, removed: true, deletes: 2},
		"somebody else's":  {group: `{"id":9,"description":"Platform team"}`, kept: "not created by forgelab"},
		"scheduled before": {group: `{"id":9,"description":"forgelab-managed","marked_for_deletion_on":"2026-09-26"}`, removed: true, deletes: 1},
	} {
		deletes := 0
		c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
			switch {
			case r.Method == http.MethodDelete:
				deletes++
			case r.URL.EscapedPath() == core:
				fmt.Fprint(w, tc.group)
			case deletes < tc.deletes: // scheduled only, and renamed: the second DELETE names that path
				fmt.Fprint(w, `{"full_path":"acme-sandbox/services/core-deletion_scheduled-9","marked_for_deletion_on":"2026-09-26"}`)
			default:
				http.NotFound(w, r)
			}
		})
		removed, kept, err := c.DeleteNamespace(ctx, "core")
		if err != nil || removed != tc.removed || kept != tc.kept || deletes != tc.deletes {
			t.Errorf("%s: removed=%t kept=%q deletes=%d err=%v", name, removed, kept, deletes, err)
		}
		if strings.Contains(strings.Join(*seen, "\n"), "/projects") {
			t.Errorf("%s: what the subgroup holds is nobody's business: %v", name, *seen)
		}
	}

	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) { http.NotFound(w, r) })
	for _, ns := range []string{"gone", ""} {
		if removed, kept, err := c.DeleteNamespace(ctx, ns); removed || kept != "" || err != nil {
			t.Errorf("%q is neither removed nor kept: removed=%t kept=%q err=%v", ns, removed, kept, err)
		}
	}
	if len(*seen) != 1 {
		t.Errorf("the sandbox's own group is not even looked at: %v", *seen)
	}
}

// A subgroup that GitLab has only scheduled for deletion still answers, and keeps its path.
func TestCreateRefusesSubgroupPendingDeletion(t *testing.T) {
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, `{"id":9,"full_path":"acme-sandbox/services/core","marked_for_deletion_on":"2026-09-26"}`)
	})
	err := c.Create(ctx, "core/api", "private", "main", nil)
	if err == nil || !strings.Contains(err.Error(), "pending deletion") || len(*seen) != 1 {
		t.Errorf("err=%v seen=%v", err, *seen)
	}
}
