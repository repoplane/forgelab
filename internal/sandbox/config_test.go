package sandbox

import (
	"errors"
	"os"
	"path/filepath"
	"testing"
)

const testConfig = `version: 1
org_allowlist: '^acme-sandbox'
sandboxes:
  ok:       {forge: forgejo, base_url: "https://forge.example.test", org: acme-sandbox-2, token_env: T}
  prod:     {forge: forgejo, base_url: "https://forge.example.test", org: acme,           token_env: T}
  loopback: {forge: forgejo, base_url: "http://127.0.0.1:3000",      org: anything,       token_env: T}
  partial:  {forge: forgejo, base_url: "https://forge.example.test", org: acme-sandbox}
`

func TestAllowlist(t *testing.T) {
	path := filepath.Join(t.TempDir(), ConfigFile)
	if err := os.WriteFile(path, []byte(testConfig), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := LoadConfig(path)
	if err != nil {
		t.Fatal(err)
	}

	if sb, err := cfg.Sandbox("ok"); err != nil || sb.MarkerTopic != DefaultMarkerTopic {
		t.Errorf("ok: sandbox=%+v err=%v", sb, err)
	}
	// There is no production organisation on a laptop.
	if _, err := cfg.Sandbox("loopback"); err != nil {
		t.Errorf("loopback: %v", err)
	}
	// The adjacent org -- the one typed every day -- is the likeliest wrong target.
	var guard *GuardError
	if _, err := cfg.Sandbox("prod"); !errors.As(err, &guard) {
		t.Errorf("prod: want a guard failure, got %v", err)
	}
	if _, err := cfg.Sandbox("partial"); err == nil {
		t.Error("partial: want an error for the missing token_env")
	}
	if _, err := cfg.Sandbox("nope"); err == nil {
		t.Error("nope: want an error for an unknown sandbox")
	}

	// Off loopback, no allowlist at all is a refusal rather than a free pass.
	cfg.OrgAllowlist = ""
	if _, err := cfg.Sandbox("ok"); !errors.As(err, &guard) {
		t.Errorf("no allowlist: want a guard failure, got %v", err)
	}
}
