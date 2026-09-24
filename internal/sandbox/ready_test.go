package sandbox

import (
	"context"
	"errors"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
)

var bg = context.Background()

// branches answers with the baseline only from the nth call onwards, which is the shape of a
// forge that has taken the push but not yet reflected it.
type branches struct {
	ready int32 // calls before the baseline appears
	calls atomic.Int32
	err   error
}

func (b *branches) Branches(context.Context, string) ([]forge.Ref, error) {
	if b.err != nil {
		return nil, b.err
	}
	if b.calls.Add(1) <= b.ready {
		return nil, nil // 200 with nothing in it: not an error, just not ready
	}
	return []forge.Ref{{Name: "main", SHA: "abc123"}}, nil
}

var repo = fleet.LockRepo{Name: "svc", DefaultBranch: "main", Baseline: "abc123"}

func far() time.Time { return time.Now().Add(time.Hour) }

func TestWaitOneReturnsOnceReflected(t *testing.T) {
	if err := waitOne(bg, &branches{ready: 3}, repo, time.Minute, far()); err != nil {
		t.Fatalf("a repository that becomes ready must not error: %v", err)
	}
}

func TestWaitOneNamesTheRepositoryItWaitedFor(t *testing.T) {
	err := waitOne(bg, &branches{ready: 1 << 30}, repo, 40*time.Millisecond, far())
	if err == nil || !strings.Contains(err.Error(), "svc never reported main at abc123") {
		t.Fatalf("got %v", err)
	}
}

// An error from the forge is reported as itself rather than as "never reported": the two send
// a reader to different places.
func TestWaitOneSurfacesTheForgeError(t *testing.T) {
	err := waitOne(bg, &branches{err: errors.New("boom")}, repo, 40*time.Millisecond, far())
	if err == nil || !strings.Contains(err.Error(), "svc not ready: boom") {
		t.Fatalf("got %v", err)
	}
}

// The fleet's ceiling and a repository's own clock are different failures. Blaming the
// repository when the fleet ran out of time is what sent a CI failure looking at a healthy
// repository, so the message has to say which bound was hit.
func TestWaitOneDistinguishesTheFleetCeiling(t *testing.T) {
	err := waitOne(bg, &branches{ready: 1 << 30}, repo, time.Hour, time.Now().Add(-time.Second))
	if err == nil || !strings.Contains(err.Error(), "gave up on the fleet") {
		t.Fatalf("got %v", err)
	}
	if strings.Contains(err.Error(), "never reported") {
		t.Error("the fleet running out of time must not read as the repository being slow")
	}
}

// The property that makes the original bug unreachable. The budget used to be one clock
// shared by the whole fleet, so a slow repository spent the time and a later, healthy one was
// blamed for it. waitOne takes a duration rather than a deadline, so there is no clock to
// share: every call starts its own, and this asserts a call after an exhausted one is
// unaffected. It does not reproduce the old failure, which lived in waitReady's single
// deadline and cannot be expressed against this signature at all.
func TestWaitOneBudgetIsNotSharedBetweenRepositories(t *testing.T) {
	overall, perRepo := far(), 100*time.Millisecond

	slow := &branches{ready: 1 << 30}
	if err := waitOne(bg, slow, repo, perRepo, overall); err == nil {
		t.Fatal("the slow repository should have run out of its own time")
	}
	// Under a shared budget the time is gone by now and this call fails without waiting.
	start := time.Now()
	if err := waitOne(bg, &branches{ready: 2}, repo, perRepo, overall); err != nil {
		t.Fatalf("a healthy repository must not inherit an exhausted budget: %v", err)
	}
	if time.Since(start) > perRepo {
		t.Error("the healthy repository waited longer than its own budget")
	}
}

func TestWaitOneStopsOnCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(bg)
	cancel()
	if err := waitOne(ctx, &branches{ready: 1 << 30}, repo, time.Hour, far()); !errors.Is(err, context.Canceled) {
		t.Fatalf("got %v", err)
	}
}
