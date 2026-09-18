package seed

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"testing/fstest"
	"time"

	"github.com/repoplane/forgelab/internal/fleet"
)

var (
	testID = fleet.GitIdentity{
		Author:    fleet.Author{Name: "Forgelab Fixture", Email: "fixture@forgelab.test"},
		Timestamp: time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC),
	}
	testRepo = fleet.Repo{Name: "svc", Dir: "repos/svc", DefaultBranch: "main", Tags: []string{"v1"}}
	testFS   = fstest.MapFS{
		"repos/svc/README.md":      {Data: []byte("# svc\n"), Mode: 0o644},
		"repos/svc/app.properties": {Data: []byte("a=b\n"), Mode: 0o644},
		"repos/svc/run.sh":         {Data: []byte("#!/bin/sh\n"), Mode: 0o755},
		"repos/svc/.DS_Store":      {Data: []byte("junk"), Mode: 0o644},
	}
)

func buildSHA(t *testing.T) string {
	t.Helper()
	b, err := Build(context.Background(), testFS, testRepo, testID)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	return b.SHA
}

func TestBuildIsDeterministic(t *testing.T) {
	first, second := buildSHA(t), buildSHA(t)
	if first != second {
		t.Fatalf("same fixture, different SHAs: %s vs %s", first, second)
	}
	if len(first) != 40 {
		t.Fatalf("not a SHA: %q", first)
	}
}

// A global ignore file is read from its default XDG path even when GIT_CONFIG_GLOBAL is
// /dev/null. Without core.excludesFile=/dev/null this machine would silently commit a
// different tree.
func TestBuildIgnoresGlobalIgnoreFile(t *testing.T) {
	want := buildSHA(t)

	home := t.TempDir()
	ignore := filepath.Join(home, ".config", "git", "ignore")
	if err := os.MkdirAll(filepath.Dir(ignore), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(ignore, []byte("*.properties\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("HOME", home)

	if got := buildSHA(t); got != want {
		t.Fatalf("global ignore file changed the SHA: %s vs %s", got, want)
	}
}

// Inside a hook, `git rebase --exec` or `git bisect run`, GIT_DIR is exported and would
// retarget the seeder at the caller's own repository.
func TestBuildIgnoresInheritedGitDir(t *testing.T) {
	want := buildSHA(t)
	t.Setenv("GIT_DIR", filepath.Join(t.TempDir(), "elsewhere.git"))
	t.Setenv("GIT_WORK_TREE", t.TempDir())
	if got := buildSHA(t); got != want {
		t.Fatalf("inherited GIT_DIR changed the SHA: %s vs %s", got, want)
	}
}

func TestRedact(t *testing.T) {
	url := "http://user:s3cret@localhost:3000/org/repo.git"
	if got := redact("push "+url+" failed", url); got != "push <url redacted> failed" {
		t.Fatalf("got %q", got)
	}
}
