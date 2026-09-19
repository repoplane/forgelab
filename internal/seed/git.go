// Package seed turns fixture directories into deterministic git commits and moves refs on
// a forge. git runs as a subprocess, never through a library, because the determinism
// guarantees are about the exact environment git runs in.
package seed

import (
	"context"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/repoplane/forgelab/internal/fleet"
)

// BaselineTag marks the seed commit on the forge. Reset points the default branch back at
// it, and because the tag is already on the server that push transfers no objects.
const BaselineTag = "forgelab-baseline"

// Built is a fixture materialised as a local git repository, ready to push.
type Built struct {
	// SHA is the seed commit.
	SHA string

	dir    string
	branch string
	env    []string
}

// Close removes the scratch repository.
func (b *Built) Close() {
	if b != nil && b.dir != "" {
		os.RemoveAll(b.dir)
	}
}

// Build materialises one fixture directory as a git repository with a pinned identity and
// clock. It needs git and nothing else: no forge, no network.
//
// Content is pushed with git rather than written through a forge's contents API on
// purpose: the API stamps the current time into every commit, which would give a
// different SHA on every run and leave nothing stable to assert against.
//
// Every repository is committed at the same pinned timestamp rather than at an
// increasing one, so a repository's SHA is a function of its own content alone. Adding a
// new fixture therefore cannot change any existing repository's SHA.
func Build(ctx context.Context, fsys fs.FS, r fleet.Repo, id fleet.GitIdentity) (*Built, error) {
	work, err := os.MkdirTemp("", "forgelab-"+r.Name+"-")
	if err != nil {
		return nil, err
	}
	b := &Built{dir: work, branch: r.DefaultBranch, env: gitEnv(id)}

	if err := copyTree(fsys, r.Dir, work); err != nil {
		b.Close()
		return nil, fmt.Errorf("copy %s: %w", r.Dir, err)
	}

	steps := [][]string{
		{"-c", "init.defaultBranch=" + r.DefaultBranch, "init", "-q"},
		{"add", "-A"},
		{"commit", "-q", "--allow-empty", "-m", "seed: " + r.Name},
		{"tag", BaselineTag},
	}
	for _, tag := range r.Tags {
		steps = append(steps, []string{"tag", tag})
	}
	for _, args := range steps {
		if _, err := git(ctx, work, b.env, "", args...); err != nil {
			b.Close()
			return nil, err
		}
	}

	sha, err := git(ctx, work, b.env, "", "rev-parse", "HEAD")
	if err != nil {
		b.Close()
		return nil, err
	}
	b.SHA = sha
	return b, nil
}

// Push force-pushes the seed commit, the declared tags and the baseline tag.
//
// Forced because the fixture directory is authoritative: re-seeding a changed fixture
// replaces the single seed commit, which is never a fast-forward.
func (b *Built) Push(ctx context.Context, pushURL string) error {
	_, err := git(ctx, b.dir, b.env, pushURL,
		"push", "-q", "--force", pushURL, "refs/heads/"+b.branch, "--tags")
	return err
}

// ResetToBaseline points branch, and every declared tag, back at the baseline tag that is
// already on the forge. It fetches one commit and pushes refs only -- no fixture tree, no
// clone cache, and no knowledge of what the content is.
func ResetToBaseline(ctx context.Context, pushURL, branch string, tags []string) error {
	work, err := os.MkdirTemp("", "forgelab-reset-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(work)
	env := gitEnv(fleet.GitIdentity{})

	base := "refs/tags/" + BaselineTag
	push := []string{"push", "-q", "--force", pushURL, base + ":refs/heads/" + branch}
	for _, tag := range tags {
		push = append(push, base+":refs/tags/"+tag)
	}
	steps := [][]string{
		{"init", "-q", "--bare"},
		{"fetch", "-q", "--depth=1", pushURL, base + ":" + base},
		push,
	}
	for _, args := range steps {
		if _, err := git(ctx, work, env, pushURL, args...); err != nil {
			return err
		}
	}
	return nil
}

// LsRemote reads a repository's branches and tags straight from git, as name -> commit SHA.
//
// This, not a forge's branch and tag listings, is what the sandbox is compared against:
// those listings are caches, and at least one forge serves them stale for seconds after a
// write -- long enough for a verify run right after a test to miss a pushed commit. git has
// no such window, answers the same way on every forge, and returns both kinds of ref in one
// round trip. An annotated tag is reported at the commit it points to.
func LsRemote(ctx context.Context, remoteURL string) (branches, tags map[string]string, err error) {
	work, err := os.MkdirTemp("", "forgelab-ls-")
	if err != nil {
		return nil, nil, err
	}
	defer os.RemoveAll(work)

	out, err := git(ctx, work, gitEnv(fleet.GitIdentity{}), remoteURL, "ls-remote", "--heads", "--tags", remoteURL)
	if err != nil {
		return nil, nil, err
	}
	branches, tags = map[string]string{}, map[string]string{}
	for _, line := range strings.Split(out, "\n") {
		sha, ref, ok := strings.Cut(line, "\t")
		if !ok {
			continue
		}
		switch {
		case strings.HasPrefix(ref, "refs/heads/"):
			branches[strings.TrimPrefix(ref, "refs/heads/")] = sha
		case strings.HasSuffix(ref, "^{}"): // the peeled line follows the tag's own and wins
			tags[strings.TrimSuffix(strings.TrimPrefix(ref, "refs/tags/"), "^{}")] = sha
		case strings.HasPrefix(ref, "refs/tags/"):
			if _, peeled := tags[strings.TrimPrefix(ref, "refs/tags/")]; !peeled {
				tags[strings.TrimPrefix(ref, "refs/tags/")] = sha
			}
		}
	}
	return branches, tags, nil
}

// gitEnv builds the environment from scratch rather than inheriting it. Any GIT_DIR,
// GIT_WORK_TREE, GIT_INDEX_FILE or GIT_OBJECT_DIRECTORY in the caller's environment would
// redirect these commands at the caller's own repository -- which happens for real inside
// a git hook, `git rebase --exec` or `git bisect run`.
func gitEnv(id fleet.GitIdentity) []string {
	stamp := id.Timestamp.UTC().Format(time.RFC3339)
	return []string{
		"PATH=" + os.Getenv("PATH"),
		"HOME=" + os.Getenv("HOME"),
		"TMPDIR=" + os.Getenv("TMPDIR"),
		"GIT_AUTHOR_NAME=" + id.Author.Name,
		"GIT_AUTHOR_EMAIL=" + id.Author.Email,
		"GIT_AUTHOR_DATE=" + stamp,
		"GIT_COMMITTER_NAME=" + id.Author.Name,
		"GIT_COMMITTER_EMAIL=" + id.Author.Email,
		"GIT_COMMITTER_DATE=" + stamp,
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_CONFIG_SYSTEM=/dev/null",
		"GIT_TERMINAL_PROMPT=0",
	}
}

// git runs one command in dir and returns its trimmed stdout. secret, if set, is a
// credentialed URL that must not reach a log or a CI transcript.
func git(ctx context.Context, dir string, env []string, secret string, args ...string) (string, error) {
	full := append([]string{
		"-c", "commit.gpgsign=false",
		"-c", "tag.gpgsign=false",
		"-c", "gc.auto=0",
		"-c", "core.autocrlf=false",
		// Not covered by GIT_CONFIG_GLOBAL=/dev/null: git looks for the global ignore
		// and attributes files at their default XDG paths regardless of which config
		// files it reads. A machine with "*.properties" in ~/.config/git/ignore would
		// otherwise drop that file from the commit and produce a different SHA,
		// silently, on that machine only.
		"-c", "core.excludesFile=/dev/null",
		"-c", "core.attributesFile=/dev/null",
	}, args...)
	cmd := exec.CommandContext(ctx, "git", full...)
	cmd.Dir = dir
	cmd.Env = env
	var stdout, stderr strings.Builder
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("git %s: %w: %s",
			redact(strings.Join(args, " "), secret),
			err, redact(strings.TrimSpace(stderr.String()), secret))
	}
	return strings.TrimSpace(stdout.String()), nil
}

// redact removes a credentialed URL from text that may reach a log or a CI transcript.
func redact(text, secret string) string {
	if secret == "" {
		return text
	}
	return strings.ReplaceAll(text, secret, "<url redacted>")
}

// copyTree materialises a fixture directory into an empty working directory. File modes
// are normalised to 0644/0755 so that a checkout's umask cannot leak into the commit.
func copyTree(fsys fs.FS, src, dst string) error {
	return fs.WalkDir(fsys, src, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		if rel == "." {
			return nil
		}
		target := filepath.Join(dst, rel)
		if d.IsDir() {
			return os.MkdirAll(target, 0o755)
		}
		if fleet.IsOSJunk(d.Name()) {
			return nil
		}
		if !d.Type().IsRegular() {
			return fmt.Errorf("%s: only regular files and directories may be seeded", path)
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		mode := os.FileMode(0o644)
		if info.Mode()&0o111 != 0 {
			mode = 0o755
		}
		if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
			return err
		}
		in, err := fsys.Open(path)
		if err != nil {
			return err
		}
		defer in.Close()
		out, err := os.OpenFile(target, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, mode)
		if err != nil {
			return err
		}
		if _, err := io.Copy(out, in); err != nil {
			out.Close()
			return err
		}
		if err := out.Close(); err != nil {
			return err
		}
		// OpenFile's mode is masked by the process umask, so set it explicitly: the
		// executable bit is part of the git tree object and therefore of the commit SHA.
		return os.Chmod(target, mode)
	})
}
