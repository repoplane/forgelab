// Package sandbox implements the commands: plan, apply, verify, reset, destroy.
//
// Everything here loops over the declared fleet and looks repositories up by name. Nothing
// enumerates the organisation, so a repository the fleet does not declare is never
// compared, reported or touched.
package sandbox

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/repoplane/forgelab/internal/fleet"
	"github.com/repoplane/forgelab/internal/forge"
	"github.com/repoplane/forgelab/internal/forge/azuredevops"
	"github.com/repoplane/forgelab/internal/forge/forgejo"
	"github.com/repoplane/forgelab/internal/forge/github"
	"github.com/repoplane/forgelab/internal/forge/gitlab"
)

// GuardError means forgelab refused to act: wrong place, missing repository, stale lock.
// Retrying will not help and a reset must not be attempted. Exit code 2.
type GuardError struct{ Msg string }

func (e *GuardError) Error() string { return e.Msg }

// DriftError means the sandbox differs from the lock in ways reset can repair. Exit code 1.
type DriftError struct{ Msg string }

func (e *DriftError) Error() string { return e.Msg }

// Options configures Open.
type Options struct {
	FleetDir   string // directory holding fleet.yaml, repos/ and fleet.lock.json
	ConfigPath string // default: <FleetDir>/sandboxes.yaml
	Sandbox    string
	Yes        bool // skip the destroy confirmation
	Verbose    bool

	In  io.Reader // default os.Stdin
	Out io.Writer // default os.Stdout

	// HTTPClient, if set, is used for every forge request.
	HTTPClient *http.Client
}

// Env is a resolved sandbox plus the fleet directory it is driven from.
type Env struct {
	Sandbox Sandbox
	Forge   forge.Forge

	fleetDir string
	fsys     fs.FS
	yes      bool
	verbose  bool
	in       io.Reader
	out      io.Writer
}

// workers bounds how many repositories are worked on at once.
const workers = 8

// Open resolves the sandbox and builds the forge client.
func Open(o Options) (*Env, error) {
	o.FleetDir, o.ConfigPath = resolvePaths(o.FleetDir, o.ConfigPath)
	if o.In == nil {
		o.In = os.Stdin
	}
	if o.Out == nil {
		o.Out = os.Stdout
	}
	cfg, err := LoadConfig(o.ConfigPath)
	if err != nil {
		return nil, err
	}
	if cfg.OrgAllowlist != "" {
		fmt.Fprintf(os.Stderr, "forgelab: note: org_allowlist is no longer used; remove it from %s\n", o.ConfigPath)
	}
	sb, err := cfg.Sandbox(o.Sandbox)
	if err != nil {
		return nil, err
	}
	token := os.Getenv(sb.TokenEnv)
	if token == "" {
		return nil, fmt.Errorf("sandbox %q: environment variable %s is empty", sb.Name, sb.TokenEnv)
	}

	var f forge.Forge
	switch sb.Forge {
	case "forgejo":
		f = forgejo.New(sb.BaseURL, sb.Org, token, o.HTTPClient)
	case "github":
		if f, err = github.New(sb.BaseURL, sb.Org, token, o.HTTPClient); err != nil {
			return nil, fmt.Errorf("sandbox %q: %w", sb.Name, err)
		}
	case "gitlab":
		if f, err = gitlab.New(sb.BaseURL, sb.Org, token, o.HTTPClient); err != nil {
			return nil, fmt.Errorf("sandbox %q: %w", sb.Name, err)
		}
	case "azuredevops":
		if f, err = azuredevops.New(sb.BaseURL, sb.Org, sb.DefaultProject, token, o.HTTPClient); err != nil {
			return nil, fmt.Errorf("sandbox %q: %w", sb.Name, err)
		}
	default:
		return nil, fmt.Errorf("sandbox %q: forge %q is not supported (forgejo, github, gitlab and azuredevops are)", sb.Name, sb.Forge)
	}

	return &Env{
		Sandbox:  sb,
		Forge:    f,
		fleetDir: o.FleetDir,
		fsys:     os.DirFS(o.FleetDir),
		yes:      o.Yes,
		verbose:  o.Verbose,
		in:       o.In,
		out:      o.Out,
	}, nil
}

func (e *Env) lockPath() string { return filepath.Join(e.fleetDir, fleet.LockFile) }

func (e *Env) printf(format string, args ...any) { fmt.Fprintf(e.out, format, args...) }

func (e *Env) debugf(format string, args ...any) {
	if e.verbose {
		fmt.Fprintf(e.out, format, args...)
	}
}

// loadLock reads the lock and checks it still describes the fleet on disk. Resetting
// against a stale baseline is not something to retry, so both failures are guard errors.
func (e *Env) loadLock() (*fleet.Lock, error) {
	lock, err := fleet.ReadLock(e.lockPath())
	if err != nil {
		if os.IsNotExist(err) {
			return nil, &GuardError{Msg: fmt.Sprintf("no %s: run `forgelab apply --sandbox %s`",
				fleet.LockFile, e.Sandbox.Name)}
		}
		return nil, &GuardError{Msg: err.Error()}
	}
	digest, err := fleet.Digest(e.fsys)
	if err != nil {
		return nil, err
	}
	if digest != lock.FleetDigest {
		return nil, &GuardError{Msg: fmt.Sprintf(
			"the fleet changed since %s was written: run `forgelab apply --sandbox %s` and commit the lock",
			fleet.LockFile, e.Sandbox.Name)}
	}
	return lock, nil
}

// confirm asks before destroy, the one command that deletes. --yes skips it.
//
// Deliberately a plain y/N: with one sandbox there is nothing to mistake it for. Typing the
// sandbox name earns its keep once two cloud sandboxes exist on the same forge -- and then
// the prompt must not print the expected answer.
func (e *Env) confirm() error {
	if e.yes {
		return nil
	}
	e.printf("\n  Delete them? [y/N] ")
	line, _ := bufio.NewReader(e.in).ReadString('\n')
	if got := strings.ToLower(strings.TrimSpace(line)); got != "y" && got != "yes" {
		return fmt.Errorf("not confirmed; nothing was changed")
	}
	return nil
}

func (e *Env) header(verb string, n int) {
	e.printf("\n  %s   sandbox %s · %s · %s/%s · %d repositories\n\n",
		verb, e.Sandbox.Name, e.Sandbox.Forge, strings.TrimRight(e.Sandbox.BaseURL, "/"), e.Sandbox.Org, n)
}

// forEach runs fn over items with bounded concurrency and returns the first error by index.
func forEach[T any](ctx context.Context, items []T, fn func(context.Context, T) error) error {
	errs := make([]error, len(items))
	jobs := make(chan int)
	var wg sync.WaitGroup
	for w := 0; w < workers && w < len(items); w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range jobs {
				errs[i] = fn(ctx, items[i])
			}
		}()
	}
	for i := range items {
		jobs <- i
	}
	close(jobs)
	wg.Wait()
	for _, err := range errs {
		if err != nil {
			return err
		}
	}
	return nil
}

func ptr[T any](v T) *T { return &v }
