package itest

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/sandbox"
	"github.com/repoplane/forgelab/internal/seed"
)

var ctx = context.Background()

// exitKind is the distinction the CLI's exit code carries: reset-and-retry on drift, stop
// on a guard failure.
func exitKind(err error) string {
	var g *sandbox.GuardError
	var d *sandbox.DriftError
	switch {
	case err == nil:
		return "ok"
	case errors.As(err, &g):
		return "guard"
	case errors.As(err, &d):
		return "drift"
	}
	return "error"
}

func wantGuard(t *testing.T, err error, contains string) {
	t.Helper()
	if exitKind(err) != "guard" || !strings.Contains(err.Error(), contains) {
		t.Fatalf("want a guard failure containing %q, got: %v", contains, err)
	}
}

// drift makes the kinds of mess a test run leaves behind, across three repositories.
func (l *lab) drift() {
	l.t.Helper()
	// a feature branch with a commit, and an open pull request from it
	l.mustAPI("POST", l.repo("billing-api")+"/contents/NEW.md",
		`{"content":"aGVsbG8K","message":"change","new_branch":"feature/run-42"}`)
	l.mustAPI("POST", l.repo("billing-api")+"/pulls",
		`{"head":"feature/run-42","base":"main","title":"run 42"}`)
	// a direct commit to the default branch, and a stray tag
	l.mustAPI("POST", l.repo("compliant")+"/contents/HACK.md", `{"content":"aGFjawo=","message":"direct"}`)
	l.mustAPI("POST", l.repo("compliant")+"/tags", `{"tag_name":"stray","target":"main"}`)
	// settings
	l.mustAPI("PUT", l.repo("parser-svc")+"/topics", `{"topics":["`+sandbox.DefaultMarkerTopic+`","changed"]}`)
}

// The whole loop: plan, apply, verify, drift, verify, reset, verify, destroy, apply -- and
// the lock comes out byte-identical, to itself and to the committed example.
func TestWalk(t *testing.T) {
	l := newLab(t)
	lockPath := filepath.Join(l.dir, fleet.LockFile)
	os.Remove(lockPath) // the example ships one; start without it

	if err := l.env().Plan(ctx); err != nil {
		t.Fatalf("plan: %v", err)
	}
	if got := strings.Count(l.out.String(), "  + "); got != 12 {
		t.Fatalf("plan: want 12 creates, got %d:\n%s", got, l.out)
	}
	if _, err := os.Stat(lockPath); err == nil {
		t.Error("plan wrote the lock; it must not write anything")
	}
	wantGuard(t, l.env().Verify(ctx), "no fleet.lock.json")

	l.mustApply()
	first, err := os.ReadFile(filepath.Join(l.dir, fleet.LockFile))
	if err != nil {
		t.Fatal(err)
	}
	if err := l.env().Verify(ctx); err != nil {
		t.Fatalf("verify after apply: %v", err)
	}
	if err := l.env().Plan(ctx); err != nil || !strings.Contains(l.out.String(), "nothing to do") {
		t.Fatalf("plan after apply: err=%v\n%s", err, l.out)
	}

	l.drift()
	err = l.env().Verify(ctx)
	if exitKind(err) != "drift" {
		t.Fatalf("verify after drift: want drift, got %v", err)
	}
	for _, want := range []string{
		"billing-api: extra branch feature/run-42; 1 open request(s)",
		"compliant: extra tag stray; main is at",
		"parser-svc: topics are [changed], want [service]",
	} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("verify did not name the drift %q:\n%v", want, err)
		}
	}

	if err := l.env().Reset(ctx); err != nil {
		t.Fatalf("reset: %v", err)
	}
	if !strings.Contains(l.out.String(), "3 of 12 repositories reset") {
		t.Errorf("reset output:\n%s", l.out)
	}
	if err := l.env().Verify(ctx); err != nil {
		t.Fatalf("verify after reset: %v", err)
	}

	if err := l.env().Destroy(ctx); err != nil {
		t.Fatalf("destroy: %v", err)
	}
	l.mustApply()
	second, _ := os.ReadFile(filepath.Join(l.dir, fleet.LockFile))
	if string(first) != string(second) {
		t.Error("the lock changed across destroy + apply: seeding is not deterministic")
	}
	golden := filepath.Join("..", "..", "examples", "fleet", fleet.LockFile)
	if os.Getenv("FORGELAB_UPDATE_LOCK") != "" {
		if err := os.WriteFile(golden, second, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	committed, err := os.ReadFile(golden)
	if err != nil {
		t.Fatalf("examples/fleet has no committed lock: %v", err)
	}
	if string(committed) != string(second) {
		t.Error("examples/fleet/fleet.lock.json is stale: run `make lock` and commit it")
	}
}

// The path a pull-request-driven consumer actually takes: open a pull request, merge it, then
// roll it back with a second merged pull request. Nobody force-pushed or touched main
// directly, yet main is two merges ahead -- and reset has to rewind it.
func TestMergedPullRequestsAreRewound(t *testing.T) {
	l := newLab(t)
	l.mustApply()
	repo := l.repo("billing-api")

	// merge retries: a forge computes mergeability after the pull request is opened, and
	// answers "try again later" until it has.
	merge := func(number int) {
		t.Helper()
		var code int
		for range 40 {
			if code = l.api("POST", fmt.Sprintf("%s/pulls/%d/merge", repo, number), `{"Do":"merge"}`); code == 200 {
				return
			}
			time.Sleep(250 * time.Millisecond)
		}
		t.Fatalf("merge #%d: HTTP %d", number, code)
	}
	open := func(branch, title string) int {
		t.Helper()
		var pr struct {
			Number int `json:"number"`
		}
		body := fmt.Sprintf(`{"head":%q,"base":"main","title":%q}`, branch, title)
		if code := l.json("POST", repo+"/pulls", body, &pr); code != 201 {
			t.Fatalf("open %s: HTTP %d", branch, code)
		}
		return pr.Number
	}

	// the change
	l.mustAPI("POST", repo+"/contents/POLICY.md",
		`{"content":"cG9saWN5Cg==","message":"add policy","new_branch":"campaign/run-42"}`)
	change := open("campaign/run-42", "campaign: add policy")
	merge(change)

	// the rollback: a second pull request undoing the first
	var file struct {
		SHA string `json:"sha"`
	}
	l.json("GET", repo+"/contents/POLICY.md", "", &file)
	l.mustAPI("DELETE", repo+"/contents/POLICY.md",
		fmt.Sprintf(`{"sha":%q,"message":"revert: add policy","new_branch":"campaign/run-42-rollback"}`, file.SHA))
	rollback := open("campaign/run-42-rollback", "revert: add policy")
	merge(rollback)

	err := l.env().Verify(ctx)
	if exitKind(err) != "drift" || !strings.Contains(err.Error(), "billing-api: ") ||
		!strings.Contains(err.Error(), "main is at") {
		t.Fatalf("verify after two merges: want drift on main, got %v", err)
	}
	if strings.Contains(err.Error(), "open request") {
		t.Errorf("merged pull requests are not open ones: %v", err)
	}

	if err := l.env().Reset(ctx); err != nil {
		t.Fatalf("reset: %v", err)
	}
	if err := l.env().Verify(ctx); err != nil {
		t.Fatalf("verify after reset: %v", err)
	}

	// What reset cannot do, pinned so that nobody is surprised: the pull requests are still
	// there, still merged, and the next one will be #3. A consumer must tell its runs apart
	// by something it controls -- a branch prefix, a label -- never by number or by count.
	for _, number := range []int{change, rollback} {
		var pr struct {
			Merged bool `json:"merged"`
		}
		if code := l.json("GET", fmt.Sprintf("%s/pulls/%d", repo, number), "", &pr); code != 200 || !pr.Merged {
			t.Errorf("pull request #%d after reset: HTTP %d merged=%t, want it to persist as merged", number, code, pr.Merged)
		}
	}
}

// A repository the fleet does not declare is never compared, never requested, and
// survives destroy.
func TestUndeclaredReposAreInvisible(t *testing.T) {
	l := newLab(t)
	l.mustApply()
	l.mustAPI("POST", "/orgs/"+l.org+"/repos", `{"name":"my-scratch","auto_init":true}`)

	l.rec.reset()
	if err := l.env().Verify(ctx); err != nil {
		t.Fatalf("verify with an undeclared repository present: %v", err)
	}
	l.drift()
	if err := l.env().Reset(ctx); err != nil {
		t.Fatalf("reset: %v", err)
	}
	if err := l.env().Destroy(ctx); err != nil {
		t.Fatalf("destroy: %v", err)
	}
	for _, req := range l.rec.all() {
		if strings.Contains(req, "my-scratch") || strings.HasSuffix(req, "/orgs/"+l.org+"/repos") {
			t.Errorf("forgelab looked at what it did not declare: %s", req)
		}
	}
	if code := l.api("GET", l.repo("my-scratch"), ""); code != 200 {
		t.Errorf("the undeclared repository did not survive destroy: HTTP %d", code)
	}
	if code := l.api("GET", l.repo("compliant"), ""); code != 404 {
		t.Errorf("destroy left a declared repository behind: HTTP %d", code)
	}
}

// Reset reads the whole fleet but writes only to what drifted.
func TestResetWritesOnlyToTouched(t *testing.T) {
	l := newLab(t)
	l.mustApply()
	l.mustAPI("POST", l.repo("billing-api")+"/contents/NEW.md",
		`{"content":"aGVsbG8K","message":"change","new_branch":"feature/x"}`)

	l.rec.reset()
	if err := l.env().Reset(ctx); err != nil {
		t.Fatalf("reset: %v", err)
	}
	writes := 0
	for _, req := range l.rec.all() {
		if strings.HasPrefix(req, "GET ") {
			continue
		}
		writes++
		if !strings.Contains(req, "/billing-api") {
			t.Errorf("reset wrote to a repository that had not drifted: %s", req)
		}
	}
	if writes == 0 {
		t.Error("reset made no writes at all")
	}
}

// Reset cannot create a repository: a missing one is a guard failure, not something to
// helpfully put back.
func TestResetCannotCreate(t *testing.T) {
	l := newLab(t)
	l.mustApply()
	l.mustAPI("DELETE", l.repo("compliant"), "")
	l.drift2("billing-api")

	err := l.env().Reset(ctx)
	wantGuard(t, err, "compliant: missing")
	if code := l.api("GET", l.repo("compliant"), ""); code != 404 {
		t.Errorf("reset recreated the repository: HTTP %d", code)
	}
	// and it refused as a whole: the drifted repository was not touched either
	if err := l.env().Verify(ctx); exitKind(err) != "guard" {
		t.Errorf("want verify to still report the guard failure, got %v", err)
	}
}

func (l *lab) drift2(name string) {
	l.t.Helper()
	l.mustAPI("POST", l.repo(name)+"/contents/X.md", `{"content":"eAo=","message":"x"}`)
}

func TestGuards(t *testing.T) {
	t.Run("apply never adopts a same-named repository", func(t *testing.T) {
		l := newLab(t)
		l.org = "forgelab-sandbox-adopt"
		l.rewriteOrg()
		l.mustAPI("POST", "/orgs", `{"username":"`+l.org+`"}`)
		l.mustAPI("POST", "/orgs/"+l.org+"/repos", `{"name":"compliant","auto_init":true}`)

		err := l.env().Apply(ctx)
		wantGuard(t, err, "compliant already exists without")
		if code := l.api("GET", l.repo("billing-api"), ""); code != 404 {
			t.Errorf("apply created repositories despite refusing: HTTP %d", code)
		}
		// destroy leaves it alone too
		if err := l.env().Destroy(ctx); err != nil {
			t.Fatal(err)
		}
		if code := l.api("GET", l.repo("compliant"), ""); code != 200 {
			t.Errorf("destroy deleted a repository that is not forgelab's: HTTP %d", code)
		}
	})

	l := newLab(t)
	l.mustApply()

	t.Run("a moved baseline tag", func(t *testing.T) {
		l.drift2("scaffold")
		l.mustAPI("DELETE", l.repo("scaffold")+"/tags/"+seed.BaselineTag, "")
		l.mustAPI("POST", l.repo("scaffold")+"/tags", `{"tag_name":"`+seed.BaselineTag+`","target":"main"}`)
		wantGuard(t, l.env().Verify(ctx), "scaffold: tag forgelab-baseline is missing or is not")
		wantGuard(t, l.env().Reset(ctx), "nothing was reset")
		l.mustApply() // apply is the way out
	})

	t.Run("a renamed repository is missing, not followed", func(t *testing.T) {
		l.mustAPI("PATCH", l.repo("public"), `{"name":"public-renamed"}`)
		wantGuard(t, l.env().Verify(ctx), "public: missing")
		l.mustAPI("PATCH", l.repo("public-renamed"), `{"name":"public"}`)
	})

	t.Run("an empty repository that was pushed to", func(t *testing.T) {
		l.mustAPI("POST", l.repo("no-commits")+"/contents/X.md", `{"content":"eAo=","message":"x"}`)
		wantGuard(t, l.env().Verify(ctx), "no-commits: is no longer empty")
		l.mustAPI("DELETE", l.repo("no-commits"), "")
		l.mustApply()
	})

	t.Run("a fixture edited without re-applying", func(t *testing.T) {
		readme := filepath.Join(l.dir, "repos", "scaffold", "README.md")
		if err := os.WriteFile(readme, []byte("# edited\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		wantGuard(t, l.env().Verify(ctx), "the fleet changed since")
		wantGuard(t, l.env().Reset(ctx), "the fleet changed since")

		if err := l.env().Plan(ctx); err != nil || !strings.Contains(l.out.String(), "~ scaffold") ||
			!strings.Contains(l.out.String(), "content: re-seed") {
			t.Fatalf("plan: err=%v\n%s", err, l.out)
		}
		l.mustApply()
		if err := l.env().Verify(ctx); err != nil {
			t.Fatalf("verify after re-apply: %v", err)
		}
	})

	t.Run("archived repositories can be re-seeded and reset", func(t *testing.T) {
		l.mustAPI("PATCH", l.repo("archived"), `{"archived":false}`)
		l.drift2("archived")
		l.mustAPI("PATCH", l.repo("archived"), `{"archived":true}`)
		if err := l.env().Reset(ctx); err != nil {
			t.Fatalf("reset: %v", err)
		}
		if err := l.env().Verify(ctx); err != nil {
			t.Fatalf("verify: %v", err)
		}
	})
}

// rewriteOrg points the lab's config at l.org after it was changed.
func (l *lab) rewriteOrg() {
	l.t.Helper()
	path := filepath.Join(l.dir, "sandboxes.yaml")
	raw, err := os.ReadFile(path)
	if err != nil {
		l.t.Fatal(err)
	}
	var out []string
	for _, line := range strings.Split(string(raw), "\n") {
		if strings.HasPrefix(strings.TrimSpace(line), "org:") {
			line = "    org: " + l.org
		}
		out = append(out, line)
	}
	if err := os.WriteFile(path, []byte(strings.Join(out, "\n")), 0o644); err != nil {
		l.t.Fatal(err)
	}
}
