package github

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

// serve starts a fake API and a client pointed at it, with sleeping stubbed out.
func serve(t *testing.T, h http.HandlerFunc) (*Client, *[]time.Duration) {
	t.Helper()
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	c := newClient(srv.URL, "https://github.com", "acme-sandbox", "s3cret", nil)
	var slept []time.Duration
	c.sleep = func(_ context.Context, d time.Duration) error { slept = append(slept, d); return nil }
	return c, &slept
}

func TestNewPicksTheAPIRoot(t *testing.T) {
	for base, want := range map[string]string{
		"":                         "https://api.github.com",
		"https://github.com/":      "https://api.github.com",
		"https://ghe.example.test": "https://ghe.example.test/api/v3",
	} {
		c, err := New(base, "o", "t", nil)
		if err != nil || c.apiURL != want {
			t.Errorf("New(%q): api=%q err=%v, want %q", base, c.apiURL, err, want)
		}
	}
	if _, err := New("github.com", "o", "t", nil); err == nil {
		t.Error("want an error for a base URL with no scheme")
	}
}

func TestGet(t *testing.T) {
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/repos/acme-sandbox/svc":
			fmt.Fprint(w, `{"name":"svc","default_branch":"main","private":true,"archived":true,"topics":["a","forgelab-managed"]}`)
		case "/repos/acme-sandbox/svc/git/matching-refs/heads/":
			fmt.Fprint(w, `[{"ref":"refs/heads/main","object":{"sha":"abc"}},{"ref":"refs/heads/feature/x","object":{"sha":"def"}}]`)
		// the old name of a renamed repository answers with the repository it became
		case "/repos/acme-sandbox/old-name":
			fmt.Fprint(w, `{"name":"new-name","default_branch":"main"}`)
		case "/repos/acme-sandbox/bare":
			fmt.Fprint(w, `{"name":"bare","default_branch":"main","private":false}`)
		case "/repos/acme-sandbox/bare/git/matching-refs/heads/":
			http.Error(w, `{"message":"Git Repository is empty."}`, http.StatusConflict)
		default:
			http.NotFound(w, r)
		}
	})

	r, found, err := c.Get(ctx, "svc")
	if err != nil || !found || r.Visibility != "private" || !r.Archived || r.Empty || len(r.Topics) != 2 {
		t.Errorf("svc: %+v found=%t err=%v", r, found, err)
	}
	if _, found, err := c.Get(ctx, "missing"); found || err != nil {
		t.Errorf("missing: found=%t err=%v", found, err)
	}
	if _, found, err := c.Get(ctx, "old-name"); found || err != nil {
		t.Errorf("a renamed repository must read as missing: found=%t err=%v", found, err)
	}
	r, found, err = c.Get(ctx, "bare")
	if err != nil || !found || !r.Empty || r.Visibility != "public" {
		t.Errorf("bare: %+v found=%t err=%v", r, found, err)
	}

	branches, _ := c.Branches(ctx, "svc")
	if len(branches) != 2 || branches[1].Name != "feature/x" || branches[1].SHA != "def" {
		t.Errorf("branches: %+v", branches)
	}
}

// Only the Link header says whether there is another page.
func TestListFollowsLinkHeader(t *testing.T) {
	var srvURL string
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("page") == "2" {
			fmt.Fprint(w, `[{"number":3,"title":"c"}]`)
			return
		}
		w.Header().Set("Link", `<`+srvURL+`/repos/acme-sandbox/svc/pulls?state=open&per_page=100&page=2>; rel="next", <x>; rel="last"`)
		fmt.Fprint(w, `[{"number":1,"title":"a"},{"number":2,"title":"b"}]`)
	})
	srvURL = c.apiURL
	reqs, err := c.OpenRequests(ctx, "svc")
	if err != nil || len(reqs) != 3 || reqs[2].Number != 3 {
		t.Errorf("reqs=%+v err=%v", reqs, err)
	}
}

func TestRateLimits(t *testing.T) {
	t.Run("a secondary limit is waited out", func(t *testing.T) {
		calls := 0
		c, slept := serve(t, func(w http.ResponseWriter, r *http.Request) {
			if calls++; calls == 1 {
				w.Header().Set("Retry-After", "2")
				http.Error(w, `{"message":"You have exceeded a secondary rate limit"}`, http.StatusForbidden)
			}
		})
		if err := c.SetTopics(ctx, "svc", nil); err != nil {
			t.Fatal(err)
		}
		if calls != 2 || len(*slept) != 1 || (*slept)[0] != 3*time.Second {
			t.Errorf("calls=%d slept=%v", calls, *slept)
		}
	})

	t.Run("an exhausted hourly limit is an error, not an hour's sleep", func(t *testing.T) {
		c, slept := serve(t, func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("X-RateLimit-Remaining", "0")
			w.Header().Set("X-RateLimit-Reset", fmt.Sprint(time.Now().Add(40*time.Minute).Unix()))
			http.Error(w, `{"message":"API rate limit exceeded"}`, http.StatusForbidden)
		})
		err := c.SetTopics(ctx, "svc", nil)
		if err == nil || !strings.Contains(err.Error(), "rate limited") || len(*slept) != 0 {
			t.Errorf("err=%v slept=%v", err, *slept)
		}
	})

	// The regression, and the shape the headers cannot describe. A secondary limit often
	// arrives with no Retry-After, and X-RateLimit-Remaining reports the *primary* budget,
	// which it leaves untouched -- so by the headers alone this is indistinguishable from
	// having no permission. Only the body says what it is. Reading it as a refusal is what
	// stopped an apply 27 repositories into a fleet of 108.
	t.Run("a secondary limit with nothing but a body is still waited out", func(t *testing.T) {
		calls := 0
		c, slept := serve(t, func(w http.ResponseWriter, r *http.Request) {
			if calls++; calls == 1 {
				w.Header().Set("X-RateLimit-Remaining", "4264") // not zero: the hourly budget is fine
				http.Error(w, `{"message":"You have exceeded a secondary rate limit and have been `+
					`temporarily blocked from content creation."}`, http.StatusForbidden)
			}
		})
		if err := c.SetTopics(ctx, "svc", nil); err != nil {
			t.Fatalf("a secondary limit is a pause, not a refusal: %v", err)
		}
		if calls != 2 || len(*slept) != 1 || (*slept)[0] != time.Minute {
			t.Errorf("calls=%d slept=%v, want one minute-long sleep and a retry", calls, *slept)
		}
	})

	t.Run("a plain 403 is not retried", func(t *testing.T) {
		calls := 0
		c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
			calls++
			http.Error(w, `{"message":"Must have admin rights to Repository."}`, http.StatusForbidden)
		})
		err := c.Delete(ctx, "svc")
		if err == nil || calls != 1 || !strings.Contains(err.Error(), "delete_repo") {
			t.Errorf("calls=%d err=%v", calls, err)
		}
	})
}

// Create has to leave the repository marked. GitHub takes no topics at creation, so they are
// set next -- and if that fails, what was just created is removed rather than stranded.
func TestCreateSetsTopicsOrRollsBack(t *testing.T) {
	failTopics := false
	var seen []string
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		seen = append(seen, r.Method+" "+r.URL.Path)
		if failTopics && strings.HasSuffix(r.URL.Path, "/topics") {
			http.Error(w, `{"message":"nope"}`, http.StatusUnprocessableEntity)
		}
	})

	if err := c.Create(ctx, "svc", "private", "main", []string{"forgelab-managed"}); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(seen, ", "); got != "POST /orgs/acme-sandbox/repos, PUT /repos/acme-sandbox/svc/topics" {
		t.Errorf("create: %s", got)
	}

	failTopics, seen = true, nil
	if err := c.Create(ctx, "svc", "private", "main", []string{"forgelab-managed"}); err == nil {
		t.Fatal("want an error when the topics cannot be set")
	}
	if last := seen[len(seen)-1]; last != "DELETE /repos/acme-sandbox/svc" {
		t.Errorf("the unmarked repository must be removed, last request was %s", last)
	}
}

func TestRequestsAndGitURL(t *testing.T) {
	private := "private"
	var got struct {
		method, path, auth string
		body               map[string]any
	}
	c, _ := serve(t, func(w http.ResponseWriter, r *http.Request) {
		got.method, got.path, got.auth = r.Method, r.URL.Path, r.Header.Get("Authorization")
		got.body = nil
		json.NewDecoder(r.Body).Decode(&got.body)
	})

	c.UpdateSettings(ctx, "svc", forge.Settings{Visibility: &private})
	if got.method != "PATCH" || got.path != "/repos/acme-sandbox/svc" || got.body["private"] != true ||
		got.auth != "Bearer s3cret" {
		t.Errorf("settings: %+v", got)
	}
	c.SetTopics(ctx, "svc", []string{"a"})
	if got.method != "PUT" || got.path != "/repos/acme-sandbox/svc/topics" || got.body["names"] == nil {
		t.Errorf("topics: %+v", got)
	}
	c.DeleteBranch(ctx, "svc", "feature/run 42")
	if got.method != "DELETE" || got.path != "/repos/acme-sandbox/svc/git/refs/heads/feature/run 42" {
		t.Errorf("delete branch: %+v", got)
	}
	c.CloseRequest(ctx, "svc", 7)
	if got.method != "PATCH" || got.path != "/repos/acme-sandbox/svc/pulls/7" || got.body["state"] != "closed" {
		t.Errorf("close: %+v", got)
	}

	u, err := c.GitURL("svc")
	if err != nil || u != "https://x-access-token:s3cret@github.com/acme-sandbox/svc.git" {
		t.Errorf("git url: %q err=%v", u, err)
	}

	// There is nowhere to put a namespace, so it is joined into the name.
	c.SetTopics(ctx, "platform/core/api", nil)
	u, _ = c.GitURL("platform/core/api")
	if got.path != "/repos/acme-sandbox/platform-core-api/topics" || !strings.HasSuffix(u, "/acme-sandbox/platform-core-api.git") {
		t.Errorf("namespaced: %s %q", got.path, u)
	}
}
