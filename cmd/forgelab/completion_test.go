package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// complete runs the real script in a real bash: it sets the words the shell would, calls the
// function, and returns what it offered.
func complete(t *testing.T, bin string, words ...string) string {
	t.Helper()
	// The binary is reached by path, as `./bin/forgelab <TAB>` would: nothing called
	// "forgelab" is on PATH, so the script must ask the binary being completed.
	words = append([]string{bin}, words[1:]...)
	script := `eval "$('` + bin + `' completion bash)"
COMP_WORDS=(` + quoteAll(words) + `)
COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
_forgelab
printf '%s\n' "${COMPREPLY[@]}"`
	cmd := exec.Command("bash", "-c", script)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("bash: %v: %s", err, out)
	}
	return strings.Join(strings.Fields(string(out)), " ")
}

func quoteAll(words []string) string {
	quoted := make([]string, len(words))
	for i, w := range words {
		quoted[i] = "'" + w + "'"
	}
	return strings.Join(quoted, " ")
}

func TestCompletion(t *testing.T) {
	if _, err := exec.LookPath("bash"); err != nil {
		t.Skip("no bash")
	}
	bin := filepath.Join(t.TempDir(), "forgelab")
	if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v: %s", err, out)
	}
	fleet, _ := filepath.Abs(filepath.Join("..", "..", "examples", "fleet"))

	for _, c := range []struct {
		words []string
		want  string
	}{
		{[]string{"forgelab", ""}, "plan apply verify reset destroy version completion"},
		{[]string{"forgelab", "re"}, "reset"},
		{[]string{"forgelab", "verify", "--s"}, "--sandbox"},
		{[]string{"forgelab", "completion", ""}, "bash zsh"},
		// sandbox names come from the config, found through a --fleet typed earlier on the line
		{[]string{"forgelab", "verify", "--fleet", fleet, "--sandbox", ""}, "local"},
		{[]string{"forgelab", "verify", "--fleet", "/nonexistent", "--sandbox", ""}, ""},
		// --flag=value, which bash splits around the "="
		{[]string{"forgelab", "verify", "--fleet", "=", fleet, "--sandbox", "=", "lo"}, "local"},
		{[]string{"forgelab", "verify", "--config", filepath.Join(fleet, "sandboxes.yaml"), "--sandbox", ""}, "local"},
		// position, not the previous word, decides: a value that happens to be "completion"
		{[]string{"forgelab", "verify", "--fleet", "completion", ""}, "--sandbox --fleet --config --yes -v"},
		{[]string{"forgelab", "completion", "bash", ""}, ""},
		{[]string{"forgelab", "version", ""}, ""},
	} {
		if got := complete(t, bin, c.words...); got != c.want {
			t.Errorf("%v: offered %q, want %q", c.words, got, c.want)
		}
	}
}

func TestCompletionCommand(t *testing.T) {
	if code := run([]string{"completion", "bash"}); code != 0 {
		t.Errorf("completion bash = %d", code)
	}
	if code := run([]string{"completion", "fish"}); code != 2 {
		t.Errorf("completion fish = %d, want 2", code)
	}
	if code := run([]string{"__sandboxes", "--fleet", "/nonexistent"}); code != 0 {
		t.Errorf("__sandboxes on a missing config must stay quiet and succeed, got %d", code)
	}
}

// A sandboxes.yaml can arrive with somebody else's repository. Pressing TAB must never run
// what it contains: `compgen -W` expands its word list, command substitutions included.
func TestCompletionDoesNotExecuteConfig(t *testing.T) {
	if _, err := exec.LookPath("bash"); err != nil {
		t.Skip("no bash")
	}
	bin := filepath.Join(t.TempDir(), "forgelab")
	if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v: %s", err, out)
	}
	dir := t.TempDir()
	canary := filepath.Join(dir, "PWNED")
	config := "version: 1\nsandboxes:\n  '$(touch " + canary + ")':\n    {forge: github, org: x, token_env: T}\n  '`touch " + canary + "`':\n    {forge: github, org: x, token_env: T}\n"
	if err := os.WriteFile(filepath.Join(dir, "sandboxes.yaml"), []byte(config), 0o644); err != nil {
		t.Fatal(err)
	}

	if got := complete(t, bin, "forgelab", "verify", "--fleet", dir, "--sandbox", ""); got != "" {
		t.Errorf("offered %q for a config with hostile names, want nothing", got)
	}
	if _, err := os.Stat(canary); err == nil {
		t.Fatal("completion executed a command taken from sandboxes.yaml")
	}
}
