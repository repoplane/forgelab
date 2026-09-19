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
	script := `eval "$(forgelab completion bash)"
COMP_WORDS=(` + quoteAll(words) + `)
COMP_CWORD=$((${#COMP_WORDS[@]} - 1))
_forgelab
printf '%s\n' "${COMPREPLY[@]}"`
	cmd := exec.Command("bash", "-c", script)
	cmd.Env = append(os.Environ(), "PATH="+filepath.Dir(bin)+string(os.PathListSeparator)+os.Getenv("PATH"))
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
