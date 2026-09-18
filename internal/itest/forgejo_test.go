// Package itest runs forgelab end to end against a throwaway Forgejo container. It is the
// only place testcontainers is imported, and only from _test files, so the CLI never
// links a container runtime.
package itest

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/testcontainers/testcontainers-go"
	tclog "github.com/testcontainers/testcontainers-go/log"
	"github.com/testcontainers/testcontainers-go/modules/forgejo"

	"github.com/repoplane/forgelab/internal/sandbox"
)

// image is Forgejo 15.0.8 (LTS), pinned by digest rather than by tag so that an upstream
// retag cannot silently change what the tests run against. The digest is a multi-arch
// index. Keep in step with compose.yaml.
const image = "codeberg.org/forgejo/forgejo@sha256:0a2e377fd3c5af3451bfa1f44e6f198f322b6d5e03f04a028b8e672f1ccddc9f"

const (
	adminUser     = "labadmin"
	adminPassword = "labadmin-not-a-secret"
	adminEmail    = "admin@forgelab.test"
	tokenEnv      = "FORGELAB_TEST_TOKEN"
)

// config keeps start-up cheap: the repository indexer in particular would otherwise index
// every seeded repository.
var config = map[string]string{
	"repository.DEFAULT_BRANCH":    "main",
	"indexer.REPO_INDEXER_ENABLED": "false",
	"actions.ENABLED":              "false",
	"cron.ENABLED":                 "false",
	"mailer.ENABLED":               "false",
	"log.LEVEL":                    "warn",
	"server.OFFLINE_MODE":          "true",
	"service.DISABLE_REGISTRATION": "true",
	"security.SECRET_KEY":          "forgelab-test-not-a-secret",
}

var (
	baseURL string
	token   string
)

type quietLogger struct{}

func (quietLogger) Printf(string, ...any) {}

// One container for the whole package; every test works in an organisation of its own.
func TestMain(m *testing.M) {
	flag.Parse()
	if testing.Short() {
		fmt.Println("skipping end-to-end tests in -short mode: they need Docker")
		return
	}
	// testcontainers narrates every container it touches. FORGELAB_DEBUG=1 brings it back.
	if os.Getenv("FORGELAB_DEBUG") == "" {
		tclog.SetDefault(quietLogger{})
	}

	ctx := context.Background()
	opts := []testcontainers.ContainerCustomizer{
		forgejo.WithAdminCredentials(adminUser, adminPassword, adminEmail),
	}
	for key, value := range config {
		section, name, _ := strings.Cut(key, ".")
		opts = append(opts, forgejo.WithConfig(section, name, value))
	}
	ctr, err := forgejo.Run(ctx, image, opts...)
	if err != nil {
		panic("start forgejo: " + err.Error())
	}
	code := 1
	defer func() {
		// Zero stop timeout: the instance is disposable and nothing needs flushing.
		_ = testcontainers.TerminateContainer(ctr, testcontainers.StopTimeout(0))
		os.Exit(code)
	}()

	if baseURL, err = ctr.ConnectionString(ctx); err != nil {
		panic(err)
	}
	if token, err = mintToken(ctx); err != nil {
		panic("mint token: " + err.Error())
	}
	os.Setenv(tokenEnv, token)
	code = m.Run()
}

// mintToken creates an API token using basic auth, which is how the first token is
// obtained when none exists yet.
func mintToken(ctx context.Context) (string, error) {
	body := `{"name":"forgelab","scopes":["write:organization","write:repository","write:user"]}`
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		baseURL+"/api/v1/users/"+adminUser+"/tokens", strings.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	req.SetBasicAuth(adminUser, adminPassword)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var out struct {
		SHA1 string `json:"sha1"`
	}
	if err := json.Unmarshal(raw, &out); err != nil || out.SHA1 == "" {
		return "", fmt.Errorf("%s: %s", resp.Status, raw)
	}
	return out.SHA1, nil
}

var labSeq atomic.Int32

// lab is one test's sandbox: a private copy of the example fleet and an org of its own.
type lab struct {
	t   *testing.T
	org string
	dir string
	out *bytes.Buffer
	rec *recorder
}

func newLab(t *testing.T) *lab {
	t.Helper()
	l := &lab{
		t: t,
		// Numbered, not named after the test: Forgejo caps an org name at 40 characters.
		org: fmt.Sprintf("forgelab-sandbox-%d", labSeq.Add(1)),
		dir: t.TempDir(),
		out: &bytes.Buffer{},
		rec: &recorder{},
	}
	if err := os.CopyFS(l.dir, os.DirFS(filepath.Join("..", "..", "examples", "fleet"))); err != nil {
		t.Fatal(err)
	}
	cfg := fmt.Sprintf("version: 1\nsandboxes:\n  test:\n    forge: forgejo\n    base_url: %s\n    org: %s\n    token_env: %s\n",
		baseURL, l.org, tokenEnv)
	if err := os.WriteFile(filepath.Join(l.dir, sandbox.ConfigFile), []byte(cfg), 0o644); err != nil {
		t.Fatal(err)
	}
	return l
}

// env opens the sandbox afresh, as each CLI invocation would.
func (l *lab) env() *sandbox.Env {
	l.t.Helper()
	l.out.Reset()
	e, err := sandbox.Open(sandbox.Options{
		FleetDir:   l.dir,
		Sandbox:    "test",
		Yes:        true,
		Out:        l.out,
		HTTPClient: &http.Client{Transport: l.rec},
	})
	if err != nil {
		l.t.Fatal(err)
	}
	return e
}

func (l *lab) mustApply() {
	l.t.Helper()
	if err := l.env().Apply(context.Background()); err != nil {
		l.t.Fatalf("apply: %v", err)
	}
}

// api calls the forge directly, as a test under way (or a person) would, and returns the
// status code.
func (l *lab) api(method, path, body string) int {
	l.t.Helper()
	req, err := http.NewRequest(method, baseURL+"/api/v1"+path, strings.NewReader(body))
	if err != nil {
		l.t.Fatal(err)
	}
	req.Header.Set("Authorization", "token "+token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		l.t.Fatal(err)
	}
	defer resp.Body.Close()
	io.Copy(io.Discard, resp.Body)
	return resp.StatusCode
}

func (l *lab) mustAPI(method, path, body string) {
	l.t.Helper()
	if code := l.api(method, path, body); code < 200 || code > 299 {
		l.t.Fatalf("%s %s: HTTP %d", method, path, code)
	}
}

func (l *lab) repo(name string) string { return "/repos/" + l.org + "/" + name }

// recorder notes every API request forgelab makes. git traffic is a subprocess and is not
// seen here.
type recorder struct {
	mu   sync.Mutex
	reqs []string // "METHOD /path"
}

func (r *recorder) RoundTrip(req *http.Request) (*http.Response, error) {
	r.mu.Lock()
	r.reqs = append(r.reqs, req.Method+" "+req.URL.Path)
	r.mu.Unlock()
	return http.DefaultTransport.RoundTrip(req)
}

func (r *recorder) reset() {
	r.mu.Lock()
	r.reqs = nil
	r.mu.Unlock()
}

func (r *recorder) all() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]string(nil), r.reqs...)
}
