package fleet

import (
	"reflect"
	"strings"
	"testing"
	"testing/fstest"
)

const specYAML = `version: 1
git:
  author: {name: "Forgelab Fixture", email: "fixture@forgelab.test"}
  timestamp: "2026-01-01T00:00:00Z"
repos:
  legacy: {default_branch: master, topics: [Batch, legacy, batch]}
  bare: {empty: true}
`

func testFS(spec string) fstest.MapFS {
	return fstest.MapFS{
		"fleet.yaml":             {Data: []byte(spec)},
		"repos/legacy/README.md": {Data: []byte("# legacy\n")},
		"repos/plain/README.md":  {Data: []byte("# plain\n")},
		"repos/bare/.gitkeep":    {Data: nil},
	}
}

func TestLoadSpecAppliesDefaultsAndOverrides(t *testing.T) {
	spec, err := LoadSpec(testFS(specYAML))
	if err != nil {
		t.Fatal(err)
	}
	if len(spec.Repos) != 3 {
		t.Fatalf("want 3 repos, got %d", len(spec.Repos))
	}
	bare, legacy, plain := spec.Repos[0], spec.Repos[1], spec.Repos[2]
	if !bare.Empty {
		t.Error("bare: want empty")
	}
	if legacy.DefaultBranch != "master" || !reflect.DeepEqual(legacy.Topics, []string{"batch", "legacy"}) {
		t.Errorf("legacy: got %+v", legacy)
	}
	if plain.DefaultBranch != "main" || plain.Visibility != VisibilityPrivate || plain.Topics == nil {
		t.Errorf("plain: got %+v", plain)
	}
}

func TestLoadSpecRejects(t *testing.T) {
	cases := map[string]string{
		"no such directory": strings.Replace(specYAML, "legacy:", "legcy:", 1),
		"version is 2":      strings.Replace(specYAML, "version: 1", "version: 2", 1),
		"visibility":        specYAML + "  plain: {visibility: internal}\n",
		"no commit to tag":  strings.Replace(specYAML, "{empty: true}", "{empty: true, tags: [v1]}", 1),
	}
	for want, yaml := range cases {
		if _, err := LoadSpec(testFS(yaml)); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("want error containing %q, got %v", want, err)
		}
	}
}

func TestParseLockRejectsEmpty(t *testing.T) {
	for _, raw := range []string{`{"version":1,"repos":[]}`, `{"version":1}`, `{"version":1,"repos":[`} {
		if _, err := ParseLock([]byte(raw)); err == nil {
			t.Errorf("ParseLock(%s): want error", raw)
		}
	}
}

func TestDigestTracksContent(t *testing.T) {
	fsys := testFS(specYAML)
	before, err := Digest(fsys)
	if err != nil {
		t.Fatal(err)
	}
	fsys["repos/plain/.DS_Store"] = &fstest.MapFile{Data: []byte("junk")}
	if same, _ := Digest(fsys); same != before {
		t.Error("OS junk changed the digest")
	}
	fsys["repos/plain/README.md"] = &fstest.MapFile{Data: []byte("# edited\n")}
	if after, _ := Digest(fsys); after == before {
		t.Error("an edited fixture did not change the digest")
	}
}

// The tree alone says which directory is which: one that holds a file is a repository, one
// that holds only directories is a namespace.
func TestLoadSpecWalksNamespaces(t *testing.T) {
	fsys := testFS(specYAML + "  platform/core/api: {topics: [service]}\n")
	fsys["repos/platform/core/api/README.md"] = &fstest.MapFile{Data: []byte("# api\n")}
	fsys["repos/platform/core/api/src/main.go"] = &fstest.MapFile{Data: []byte("package main\n")}
	fsys["repos/platform/tooling/run.sh"] = &fstest.MapFile{Data: []byte("#!/bin/sh\n")}
	fsys["repos/payments/api/.gitkeep"] = &fstest.MapFile{}
	fsys["repos/payments/.DS_Store"] = &fstest.MapFile{Data: []byte("junk")}

	spec, err := LoadSpec(fsys)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, r := range spec.Repos {
		names = append(names, r.Name)
		if r.Name == "platform/core/api" && (r.Dir != "repos/platform/core/api" || len(r.Topics) != 1) {
			t.Errorf("platform/core/api: %+v", r)
		}
	}
	want := []string{"bare", "legacy", "payments/api", "plain", "platform/core/api", "platform/tooling"}
	if !reflect.DeepEqual(names, want) {
		t.Errorf("repos: got %v, want %v", names, want)
	}
	if got, want := spec.Namespaces(), []string{"payments", "platform", "platform/core"}; !reflect.DeepEqual(got, want) {
		t.Errorf("namespaces, outermost first: got %v, want %v", got, want)
	}

	// A file makes it a repository, so the override below it no longer names one.
	fsys["repos/platform/core/NOTES.md"] = &fstest.MapFile{Data: []byte("stray\n")}
	if _, err := LoadSpec(fsys); err == nil || !strings.Contains(err.Error(), "no such directory") {
		t.Errorf("want the stray file caught through the override, got %v", err)
	}
}

func TestLoadSpecRejectsTrees(t *testing.T) {
	for want, extra := range map[string][]string{
		"no repository is under it": {"repos/hollow/.hidden/x"},
		`both "a-b-c"`:              {"repos/a-b/c/README.md", "repos/a/b-c/README.md"},
		`both "x-y-z"`:              {"repos/X-y/z/README.md", "repos/x/y-z/README.md"},
		"not a valid":               {"repos/team one/api/README.md"},
	} {
		fsys := testFS(specYAML)
		for _, p := range extra {
			fsys[p] = &fstest.MapFile{Data: []byte("x\n")}
		}
		if _, err := LoadSpec(fsys); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("want error containing %q, got %v", want, err)
		}
	}
}
