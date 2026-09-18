package sandbox

import (
	"io"
	"strings"
	"testing"
)

func TestConfirm(t *testing.T) {
	cases := map[string]bool{"y\n": true, "YES\n": true, "n\n": false, "\n": false, "": false, "local\n": false}
	for input, want := range cases {
		e := &Env{in: strings.NewReader(input), out: io.Discard}
		if got := e.confirm() == nil; got != want {
			t.Errorf("confirm(%q) = %t, want %t", input, got, want)
		}
	}
	// --yes never reads stdin.
	if err := (&Env{yes: true, out: io.Discard}).confirm(); err != nil {
		t.Errorf("confirm with --yes: %v", err)
	}
}
