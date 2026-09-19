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
			fmt.Fprint(w, `{"path":"svc","default_branch":"main","visibility":"private","archived":true,"empty_repo":false,"topics":["a"]}`)
		// just pushed: empty_repo has not caught up, the repository has
		case "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh":
			fmt.Fprint(w, `{"path":"fresh","visibility":"public","empty_repo":true}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Ffresh/repository/branches":
			fmt.Fprint(w, `[{"name":"main","commit":{"id":"abc"}}]`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fbare":
			fmt.Fprint(w, `{"path":"bare","visibility":"private","empty_repo":true}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fbare/repository/branches":
			fmt.Fprint(w, `[]`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fold-name":
			fmt.Fprint(w, `{"path":"new-name"}`)
		case "/api/v4/projects/acme-sandbox%2Fservices%2Fdoomed":
			fmt.Fprint(w, `{"path":"doomed","marked_for_deletion_on":"2026-09-26"}`)
		default:
			http.NotFound(w, r)
		}
	})

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
		"POST " + svc + "/archive",
	}
	if strings.Join(*seen, "\n") != strings.Join(want, "\n") {
		t.Errorf("got:\n%s\nwant:\n%s", strings.Join(*seen, "\n"), strings.Join(want, "\n"))
	}
}

func TestAllowForcePush(t *testing.T) {
	status := http.StatusNoContent
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(status) })

	if err := c.AllowForcePush(ctx, "svc", "release/1"); err != nil {
		t.Fatal(err)
	}
	if (*seen)[0] != "DELETE "+svc+"/protected_branches/release%2F1" {
		t.Errorf("got %s", (*seen)[0])
	}
	status = http.StatusNotFound // no rule: already force-pushable
	if err := c.AllowForcePush(ctx, "svc", "main"); err != nil {
		t.Errorf("a missing rule is not an error: %v", err)
	}
	status = http.StatusForbidden
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
