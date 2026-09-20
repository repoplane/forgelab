package azuredevops

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

const repos = "/acme/sandbox/_apis/git/repositories"

func serve(t *testing.T, h http.HandlerFunc) (*Client, *[]string) {
	t.Helper()
	var seen []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen = append(seen, r.Method+" "+r.URL.Path)
		if _, pass, _ := r.BasicAuth(); pass != "s3cret" {
			t.Errorf("no PAT on %s", r.URL.Path)
		}
		if r.URL.Query().Get("api-version") == "" {
			t.Errorf("no api-version on %s", r.URL.Path)
		}
		h(w, r)
	}))
	t.Cleanup(srv.Close)
	c, err := New(srv.URL, "acme", "sandbox", "s3cret", nil)
	if err != nil {
		t.Fatal(err)
	}
	c.sleep = func(context.Context, time.Duration) error { return nil }
	return c, &seen
}

// A direct GET comes first; on a 404 the listing tells a disabled repository from a missing
// one; and a repository whose refs are gone was deleted a moment ago, whatever the GET says.
func TestGet(t *testing.T) {
	lists := 0
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case repos + "/svc":
			fmt.Fprint(w, `{"id":"1","name":"svc","defaultBranch":"refs/heads/master"}`)
		case repos + "/svc/refs":
			fmt.Fprint(w, `{"value":[{"name":"refs/heads/master","objectId":"abc"},{"name":"refs/heads/feature/x","objectId":"def"}]}`)
		case repos + "/bare":
			fmt.Fprint(w, `{"id":"2","name":"bare"}`)
		case repos + "/bare/refs":
			fmt.Fprint(w, `{"value":[]}`)
		case repos + "/ghost": // deleted a second ago: the GET still answers, the refs do not
			fmt.Fprint(w, `{"id":"4","name":"ghost"}`)
		case repos:
			// "lagging" was disabled a moment ago and the listing has not caught up yet
			lists++
			fmt.Fprintf(w, `{"value":[{"id":"3","name":"Retired","isDisabled":true},{"id":"5","name":"lagging","isDisabled":%t}]}`, lists > 2)
		default:
			http.NotFound(w, r)
		}
	})

	r, found, err := c.Get(ctx, "svc")
	if err != nil || !found || r.DefaultBranch != "master" || r.Archived || r.Empty {
		t.Errorf("svc: %+v found=%t err=%v", r, found, err)
	}
	if r, found, _ := c.Get(ctx, "bare"); !found || !r.Empty {
		t.Errorf("bare: %+v found=%t", r, found)
	}
	if r, found, err := c.Get(ctx, "retired"); err != nil || !found || !r.Archived {
		t.Errorf("a disabled repository is found through the listing: %+v found=%t err=%v", r, found, err)
	}
	for _, name := range []string{"missing", "ghost"} {
		if _, found, err := c.Get(ctx, name); found || err != nil {
			t.Errorf("%s: found=%t err=%v", name, found, err)
		}
	}
	lists = 0
	if r, found, err := c.Get(ctx, "lagging"); err != nil || !found || !r.Archived || lists != 3 {
		t.Errorf("a stale listing is asked again: %+v found=%t err=%v lists=%d", r, found, err, lists)
	}
}

// A disabled repository refuses even its own deletion, and a deletion only reaches the
// recycle bin: enable, delete, purge.
func TestDeleteEnablesThenPurges(t *testing.T) {
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodGet && r.URL.Path == repos:
			fmt.Fprint(w, `{"value":[{"id":"42","name":"svc","isDisabled":true}]}`)
		case r.Method == http.MethodGet:
			http.NotFound(w, r) // disabled: unreadable
		}
	})
	if err := c.Delete(ctx, "svc"); err != nil {
		t.Fatal(err)
	}
	want := "GET " + repos + "/svc, GET " + repos + ", PATCH " + repos + "/42, DELETE " + repos + "/42, DELETE /acme/sandbox/_apis/git/recycleBin/repositories/42"
	if got := strings.Join(*seen, ", "); got != want {
		t.Errorf("got  %s\nwant %s", got, want)
	}
}

// Disabled rejects every other write: enable first, disable last, and skip what already holds.
func TestUpdateSettingsOrder(t *testing.T) {
	disabled := true
	var patches []string
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case http.MethodGet:
			if disabled && r.URL.Path != repos {
				http.NotFound(w, r)
				return
			}
			fmt.Fprintf(w, `{"value":[{"id":"1","name":"svc","defaultBranch":"refs/heads/main","isDisabled":%t}]}`, disabled)
		case http.MethodPatch:
			var body map[string]any
			json.NewDecoder(r.Body).Decode(&body)
			for k, v := range body {
				patches = append(patches, fmt.Sprintf("%s=%v", k, v))
			}
		}
	})
	master, no, yes := "master", false, true
	if err := c.UpdateSettings(ctx, "svc", forge.Settings{DefaultBranch: &master, Archived: &no}); err != nil {
		t.Fatal(err)
	}
	if err := c.UpdateSettings(ctx, "svc", forge.Settings{Archived: &yes}); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(patches, ", "); got != "isDisabled=false, defaultBranch=refs/heads/master" {
		t.Errorf("patches: %s (already disabled, so no third one)", got)
	}
}

func TestDeleteBranchAndCloseRequest(t *testing.T) {
	var body any
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodGet {
			// the filter is a prefix match: run-42-rollback must not be mistaken for run-42
			fmt.Fprint(w, `{"value":[{"name":"refs/heads/campaign/run-42-rollback","objectId":"bbb"},{"name":"refs/heads/campaign/run-42","objectId":"aaa"}]}`)
			return
		}
		json.NewDecoder(r.Body).Decode(&body)
		fmt.Fprint(w, `{"value":[{"success":true,"updateStatus":"succeeded"}]}`)
	})
	if err := c.DeleteBranch(ctx, "svc", "campaign/run-42"); err != nil {
		t.Fatal(err)
	}
	update := body.([]any)[0].(map[string]any)
	if update["name"] != "refs/heads/campaign/run-42" || update["oldObjectId"] != "aaa" || update["newObjectId"] != zeroSHA {
		t.Errorf("ref update: %+v", update)
	}

	c.CloseRequest(ctx, "svc", 7)
	if last := (*seen)[len(*seen)-1]; last != "PATCH "+repos+"/svc/pullrequests/7" || body.(map[string]any)["status"] != "abandoned" {
		t.Errorf("close: %s %+v", last, body)
	}
}

func TestCapsAndGitURL(t *testing.T) {
	c, _ := New("", "acme", "my sandbox", "s3cret", nil)
	if caps := c.Caps(); caps.Topics || caps.Visibility || !caps.ArchivedUnreadable {
		t.Errorf("caps: %+v", caps)
	}
	u, err := c.GitURL("svc")
	if err != nil || u != "https://forgelab:s3cret@dev.azure.com/acme/my%20sandbox/_git/svc" {
		t.Errorf("git url: %q err=%v", u, err)
	}
}

// A namespace's first segment is a project and the rest joins the name. The project is made
// on the way to the first repository that needs it, and that is waited for.
func TestCreateMakesProject(t *testing.T) {
	var bodies []map[string]any
	polls := 0
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodPost:
			var body map[string]any
			json.NewDecoder(r.Body).Decode(&body)
			bodies = append(bodies, body)
			fmt.Fprint(w, `{"id":"op1","status":"queued"}`)
		case r.URL.Path == "/acme/_apis/projects/platform" && len(bodies) == 0:
			http.NotFound(w, r)
		case r.URL.Path == "/acme/_apis/projects/platform":
			fmt.Fprint(w, `{"id":"p9"}`)
		case r.URL.Path == "/acme/_apis/process/processes":
			fmt.Fprint(w, `{"value":[{"id":"scrum"},{"id":"agile","isDefault":true}]}`)
		case r.URL.Path == "/acme/_apis/operations/op1":
			if polls++; polls == 1 {
				fmt.Fprint(w, `{"id":"op1","status":"inProgress"}`)
				return
			}
			fmt.Fprint(w, `{"id":"op1","status":"succeeded"}`)
		default:
			t.Errorf("unexpected %s %s", r.Method, r.URL.Path)
		}
	})
	if err := c.Create(ctx, "platform/core/api", "", "", nil); err != nil {
		t.Fatal(err)
	}
	if err := c.Create(ctx, "platform/tooling", "", "", nil); err != nil {
		t.Fatal(err)
	}
	if len(bodies) != 3 || polls != 2 {
		t.Fatalf("bodies=%v polls=%d\n%s", bodies, polls, strings.Join(*seen, "\n"))
	}
	caps := bodies[0]["capabilities"].(map[string]any)
	if bodies[0]["name"] != "platform" || bodies[0]["description"] != forge.NamespaceDescription || caps["processTemplate"].(map[string]any)["templateTypeId"] != "agile" ||
		caps["versioncontrol"].(map[string]any)["sourceControlType"] != "Git" {
		t.Errorf("project: %+v", bodies[0])
	}
	if bodies[1]["name"] != "core-api" || bodies[1]["project"].(map[string]any)["id"] != "p9" || bodies[2]["name"] != "tooling" {
		t.Errorf("repositories: %+v", bodies[1:])
	}
	if last := (*seen)[len(*seen)-1]; last != "POST /acme/platform/_apis/git/repositories" {
		t.Errorf("last: %s", last)
	}

	u, err := c.GitURL("platform/core/api")
	if err != nil || !strings.HasSuffix(u, "/acme/platform/_git/core-api") {
		t.Errorf("git url: %q err=%v", u, err)
	}
	// sandbox/x and x would be the same repository
	if _, _, err := c.Get(ctx, "Sandbox/x"); err == nil || !strings.Contains(err.Error(), "own project") {
		t.Errorf("want the namespace refused, got %v", err)
	}
}

// Before its project exists, a repository is simply missing.
func TestGetInMissingProject(t *testing.T) {
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) { http.NotFound(w, r) })
	if _, found, err := c.Get(ctx, "platform/api"); found || err != nil {
		t.Errorf("found=%t err=%v", found, err)
	}
}

func TestDeleteNamespace(t *testing.T) {
	const ours = `{"id":"p9","description":"forgelab-managed: created by ..."}`
	for name, tc := range map[string]struct {
		project, repos string
		removed        bool
		kept           string
	}{
		"born-with repository only": {project: ours, repos: `{"value":[{"id":"1","name":"Platform"}]}`, removed: true},
		"something else":            {project: ours, repos: `{"value":[{"id":"1","name":"platform"},{"id":"2","name":"scratch"}]}`, kept: "not empty"},
		"born-with, but pushed to":  {project: ours, repos: `{"value":[{"id":"1","name":"pushed"}]}`, kept: "not empty"},
		// boards, pipelines, a wiki: nothing forgelab can see, so nothing it may judge empty
		"somebody else's": {project: `{"id":"p9","description":"Platform team"}`, repos: `{"value":[]}`, kept: "not created by forgelab"},
	} {
		project := "platform"
		if strings.Contains(tc.repos, "pushed") {
			project = "pushed"
		}
		deleted := false
		c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
			switch {
			case r.Method == http.MethodDelete && r.URL.Path == "/acme/_apis/projects/p9":
				deleted = true
				fmt.Fprint(w, `{"id":"op1"}`)
			case r.URL.Path == "/acme/_apis/projects/"+project:
				fmt.Fprint(w, tc.project)
			case r.URL.Path == "/acme/"+project+"/_apis/git/repositories":
				fmt.Fprint(w, tc.repos)
			case strings.HasSuffix(r.URL.Path, "/pushed/refs"):
				fmt.Fprint(w, `{"value":[{"name":"refs/heads/main","objectId":"abc"}]}`)
			case strings.HasSuffix(r.URL.Path, "/refs"):
				fmt.Fprint(w, `{"value":[]}`)
			case r.URL.Path == "/acme/_apis/operations/op1":
				fmt.Fprint(w, `{"id":"op1","status":"succeeded"}`)
			default:
				t.Errorf("%s: unexpected %s %s", name, r.Method, r.URL.Path)
			}
		})
		removed, kept, err := c.DeleteNamespace(ctx, project)
		if err != nil || removed != tc.removed || kept != tc.kept || deleted != tc.removed {
			t.Errorf("%s: removed=%t kept=%q deleted=%t err=%v", name, removed, kept, deleted, err)
		}
	}

	// Deeper than a project there is nothing to remove, and the sandbox's own project stays.
	c, seen := serve(t, func(w http.ResponseWriter, r *http.Request) { http.NotFound(w, r) })
	for _, ns := range []string{"platform/core", "sandbox", "gone"} {
		if removed, kept, err := c.DeleteNamespace(ctx, ns); removed || kept != "" || err != nil {
			t.Errorf("%s: removed=%t kept=%q err=%v", ns, removed, kept, err)
		}
	}
	if len(*seen) != 1 {
		t.Errorf("only the missing project is looked up: %v", *seen)
	}
}

// Only a project made a moment ago is known to hold nothing but its born-with repository.
func TestCreateConflictInExistingProject(t *testing.T) {
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			http.Error(w, "exists", http.StatusConflict)
			return
		}
		fmt.Fprint(w, `{"id":"p9"}`)
	})
	if err := c.Create(ctx, "platform/platform", "", "", nil); err == nil {
		t.Error("a conflict in a project that was already there must surface")
	}
}
