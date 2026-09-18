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
