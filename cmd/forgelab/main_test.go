package main

import (
	"os/exec"
	"strings"
	"testing"
)

// A CLI that drags in a container runtime is a CLI nobody installs. testcontainers is for
// the end-to-end tests only.
func TestCLIDoesNotLinkTestcontainers(t *testing.T) {
	out, err := exec.Command("go", "list", "-deps", ".").CombinedOutput()
	if err != nil {
		t.Fatalf("go list: %v: %s", err, out)
	}
	for _, dep := range strings.Fields(string(out)) {
		if strings.Contains(dep, "testcontainers") || strings.Contains(dep, "docker") {
			t.Errorf("the CLI links %s", dep)
		}
	}
}

func TestUsageErrorsExitTwo(t *testing.T) {
	for _, args := range [][]string{{}, {"verify"}, {"verify", "--nope"}} {
		if code := run(args); code != 2 {
			t.Errorf("run(%v) = %d, want 2", args, code)
		}
	}
}
