package sandbox

import (
	"os"
	"path/filepath"
	"testing"
)

const testConfig = `version: 1
org_allowlist: '^this-is-ignored-now$'
sandboxes:
  gh:      {forge: github,      org: acme-sandbox, token_env: T}
  gl:      {forge: gitlab,      org: acme-sandbox/services, token_env: T}
  ado:     {forge: azuredevops, org: acme, project: sandbox, token_env: T}
  local:   {forge: forgejo,     base_url: "http://127.0.0.1:3000", org: anything, token_env: T}
  nourl:   {forge: forgejo,     org: acme-sandbox, token_env: T}
  badurl:  {forge: forgejo,     base_url: "localhost:3000", org: acme-sandbox, token_env: T}
  partial: {forge: github,      org: acme-sandbox}
  noproj:  {forge: azuredevops, org: acme, token_env: T}
`

func TestSandboxResolution(t *testing.T) {
	path := filepath.Join(t.TempDir(), ConfigFile)
	if err := os.WriteFile(path, []byte(testConfig), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := LoadConfig(path)
	if err != nil {
		t.Fatal(err)
	}

	// Hosted forges know where they live; the marker topic has a default.
	for name, want := range map[string]struct{ baseURL, scope string }{
		"gh":    {"https://github.com", "acme-sandbox"},
		"gl":    {"https://gitlab.com", "acme-sandbox/services"},
		"ado":   {"https://dev.azure.com", "acme/sandbox"},
		"local": {"http://127.0.0.1:3000", "anything"},
	} {
		sb, err := cfg.Sandbox(name)
		if err != nil || sb.BaseURL != want.baseURL || sb.Scope() != want.scope || sb.MarkerTopic != DefaultMarkerTopic {
			t.Errorf("%s: %+v err=%v", name, sb, err)
		}
	}
	// A left-over org_allowlist no longer refuses anything.
	if cfg.OrgAllowlist == "" {
		t.Error("org_allowlist should still be parsed, so that Open can say it is unused")
	}

	for _, name := range []string{"nourl", "badurl", "partial", "noproj", "nope"} {
		if _, err := cfg.Sandbox(name); err == nil {
			t.Errorf("%s: want an error", name)
		}
	}
}
