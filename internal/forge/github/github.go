// Package github implements forge.Forge against the GitHub REST API (github.com and
// GitHub Enterprise Server).
package github

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/repoplane/forgelab/internal/forge"
)

// DefaultBaseURL is used when a sandbox names no base_url.
const DefaultBaseURL = "https://github.com"

// maxRateLimitWait bounds how long a request sleeps on a rate limit before giving up. A
// secondary limit asks for seconds; an exhausted primary limit can ask for most of an hour,
// which is an error to report, not a pause to sit through.
const maxRateLimitWait = 90 * time.Second

// Client talks to one organisation.
type Client struct {
	apiURL string // https://api.github.com, or <base>/api/v3 on Enterprise Server
	gitURL string // https://github.com
	org    string
	token  string
	http   *http.Client

	// GitHub asks that mutating requests be made serially, not concurrently; bursts of
	// them are what trips the secondary rate limit. Reads stay concurrent.
	writes sync.Mutex

	sleep func(context.Context, time.Duration) error // swapped out in tests
}

var _ forge.Forge = (*Client)(nil)

// New returns a client. baseURL is the web address, e.g. https://github.com; hc may be nil.
func New(baseURL, org, token string, hc *http.Client) (*Client, error) {
	if baseURL == "" {
		baseURL = DefaultBaseURL
	}
	u, err := url.Parse(baseURL)
	if err != nil || u.Scheme == "" || u.Host == "" {
		return nil, fmt.Errorf("base URL %q has no scheme or host", baseURL)
	}
	base := strings.TrimRight(baseURL, "/")
	api := base + "/api/v3"
	if u.Host == "github.com" {
		api = "https://api.github.com"
	}
	return newClient(api, base, org, token, hc), nil
}

func newClient(apiURL, gitURL, org, token string, hc *http.Client) *Client {
	if hc == nil {
		hc = &http.Client{Timeout: 30 * time.Second}
	}
	return &Client{apiURL: apiURL, gitURL: gitURL, org: org, token: token, http: hc, sleep: sleepCtx}
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(d):
		return nil
	}
}

// statusError keeps the HTTP status so callers can tell "not there" from "broken".
type statusError struct {
	code int
	msg  string
}

func (e *statusError) Error() string { return e.msg }

func hasStatus(err error, codes ...int) bool {
	var se *statusError
	return errors.As(err, &se) && slices.Contains(codes, se.code)
}

// do sends one request. target is a path under the API root, or an absolute URL taken from
// a Link header.
func (c *Client) do(ctx context.Context, method, target string, body, out any) (http.Header, error) {
	var raw []byte
	if body != nil {
		var err error
		if raw, err = json.Marshal(body); err != nil {
			return nil, err
		}
	}
	if method != http.MethodGet {
		c.writes.Lock()
		defer c.writes.Unlock()
	}
	if !strings.HasPrefix(target, "http") {
		target = c.apiURL + target
	}

	for attempt := 1; ; attempt++ {
		req, err := http.NewRequestWithContext(ctx, method, target, bytes.NewReader(raw))
		if err != nil {
			return nil, err
		}
		req.Header.Set("Accept", "application/vnd.github+json")
		req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
		req.Header.Set("Authorization", "Bearer "+c.token)
		if body != nil {
			req.Header.Set("Content-Type", "application/json")
		}
		resp, err := c.http.Do(req)
		if err != nil {
			return nil, err
		}
		payload, _ := io.ReadAll(resp.Body)
		resp.Body.Close()

		if wait, limited := rateLimited(resp); limited {
			if attempt >= 4 || wait > maxRateLimitWait {
				return nil, fmt.Errorf("%s %s: rate limited by GitHub; retry in %s", method, pathOf(target), wait.Round(time.Second))
			}
			if err := c.sleep(ctx, wait); err != nil {
				return nil, err
			}
			continue
		}
		if resp.StatusCode < 200 || resp.StatusCode > 299 {
			return nil, &statusError{code: resp.StatusCode, msg: fmt.Sprintf("%s %s: %s: %s",
				method, pathOf(target), resp.Status, strings.TrimSpace(string(payload)))}
		}
		if out != nil && len(payload) > 0 {
			return resp.Header, json.Unmarshal(payload, out)
		}
		return resp.Header, nil
	}
}

// rateLimited recognises both limits. The secondary one answers 403 or 429 with
// Retry-After; an exhausted primary one answers 403 with X-RateLimit-Remaining: 0 and the
// reset time. A plain 403 (no permission) is neither, and is not retried.
func rateLimited(resp *http.Response) (time.Duration, bool) {
	if resp.StatusCode != http.StatusForbidden && resp.StatusCode != http.StatusTooManyRequests {
		return 0, false
	}
	if s, err := strconv.Atoi(resp.Header.Get("Retry-After")); err == nil {
		return time.Duration(s+1) * time.Second, true
	}
	if resp.Header.Get("X-RateLimit-Remaining") == "0" {
		if reset, err := strconv.ParseInt(resp.Header.Get("X-RateLimit-Reset"), 10, 64); err == nil {
			return max(time.Until(time.Unix(reset, 0)), time.Second), true
		}
	}
	if resp.StatusCode == http.StatusTooManyRequests {
		return time.Minute, true
	}
	return 0, false
}

func pathOf(target string) string {
	if u, err := url.Parse(target); err == nil {
		return u.Path
	}
	return target
}

var nextLink = regexp.MustCompile(`<([^>]+)>;\s*rel="next"`)

// list follows Link: rel="next" until there is none. Only the Link header is
// authoritative: a short page is not the last page, and a full page is not a promise of
// another.
func list[T any](ctx context.Context, c *Client, path string) ([]T, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	var all []T
	for target := path + sep + "per_page=100"; target != ""; {
		var items []T
		hdr, err := c.do(ctx, http.MethodGet, target, nil, &items)
		if err != nil {
			return nil, err
		}
		all = append(all, items...)
		target = ""
		if m := nextLink.FindStringSubmatch(hdr.Get("Link")); m != nil {
			target = m[1]
		}
	}
	return all, nil
}

func (c *Client) repoPath(name string) string {
	return "/repos/" + url.PathEscape(c.org) + "/" + url.PathEscape(name)
}

// EnsureOrg only checks: a GitHub organisation cannot be created through the API.
func (c *Client) EnsureOrg(ctx context.Context) error {
	_, err := c.do(ctx, http.MethodGet, "/orgs/"+url.PathEscape(c.org), nil, nil)
	if hasStatus(err, http.StatusNotFound) {
		return fmt.Errorf("organisation %q does not exist, or the token cannot see it", c.org)
	}
	return err
}

func (c *Client) Get(ctx context.Context, name string) (forge.Repo, bool, error) {
	var raw struct {
		Name          string   `json:"name"`
		DefaultBranch string   `json:"default_branch"`
		Private       bool     `json:"private"`
		Archived      bool     `json:"archived"`
		Topics        []string `json:"topics"`
	}
	if _, err := c.do(ctx, http.MethodGet, c.repoPath(name), nil, &raw); err != nil {
		if hasStatus(err, http.StatusNotFound) {
			return forge.Repo{}, false, nil
		}
		return forge.Repo{}, false, err
	}
	// GitHub redirects a renamed repository's old name to it. That is not the repository
	// that was asked for.
	if !strings.EqualFold(raw.Name, name) {
		return forge.Repo{}, false, nil
	}

	// The repository object does not say whether it has commits (`size` lags), so ask.
	branches, err := c.Branches(ctx, name)
	if err != nil {
		return forge.Repo{}, false, err
	}

	r := forge.Repo{
		Name:          raw.Name,
		DefaultBranch: raw.DefaultBranch,
		Visibility:    "public",
		Archived:      raw.Archived,
		Empty:         len(branches) == 0,
		Topics:        raw.Topics,
	}
	if raw.Private {
		r.Visibility = "private"
	}
	return r, true, nil
}

// Create ignores defaultBranch: GitHub takes none at creation. The first branch pushed
// becomes the default, and UpdateSettings pins it afterwards.
func (c *Client) Create(ctx context.Context, name, visibility, _ string, topics []string) error {
	_, err := c.do(ctx, http.MethodPost, "/orgs/"+url.PathEscape(c.org)+"/repos", map[string]any{
		"name":      name,
		"private":   visibility == "private",
		"auto_init": false,
	}, nil)
	if err != nil {
		return err
	}
	if err := c.SetTopics(ctx, name, topics); err != nil {
		// Created a moment ago by this very call, so removing it loses nothing -- and leaving
		// it would strand an unmarked repository that apply refuses to adopt.
		if derr := c.Delete(ctx, name); derr != nil {
			return fmt.Errorf("set topics: %w (and the new repository could not be removed: %v)", err, derr)
		}
		return fmt.Errorf("set topics: %w", err)
	}
	return nil
}

func (c *Client) Delete(ctx context.Context, name string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name), nil, nil)
	if hasStatus(err, http.StatusForbidden) {
		return fmt.Errorf("%w (deleting needs the delete_repo scope, or Administration: write on a fine-grained token)", err)
	}
	return err
}

func (c *Client) UpdateSettings(ctx context.Context, name string, s forge.Settings) error {
	fields := map[string]any{}
	if s.DefaultBranch != nil {
		fields["default_branch"] = *s.DefaultBranch
	}
	if s.Visibility != nil {
		fields["private"] = *s.Visibility == "private"
	}
	if s.Archived != nil {
		fields["archived"] = *s.Archived
	}
	if len(fields) == 0 {
		return nil
	}
	_, err := c.do(ctx, http.MethodPatch, c.repoPath(name), fields, nil)
	return err
}

func (c *Client) SetTopics(ctx context.Context, name string, topics []string) error {
	if topics == nil {
		topics = []string{}
	}
	_, err := c.do(ctx, http.MethodPut, c.repoPath(name)+"/topics", map[string]any{"names": topics}, nil)
	return err
}

// refs reads the git database directly rather than the branch and tag listings: it is the
// source those are derived from, and it answers the same way for both kinds of ref.
func (c *Client) refs(ctx context.Context, name, kind string) ([]forge.Ref, error) {
	type ref struct {
		Ref    string `json:"ref"`
		Object struct {
			SHA string `json:"sha"`
		} `json:"object"`
	}
	prefix := "refs/" + kind + "/"
	items, err := list[ref](ctx, c, c.repoPath(name)+"/git/matching-refs/"+kind+"/")
	if err != nil {
		// A repository with no commits has no git database to read.
		if hasStatus(err, http.StatusConflict) {
			return nil, nil
		}
		return nil, err
	}
	var out []forge.Ref
	for _, r := range items {
		out = append(out, forge.Ref{Name: strings.TrimPrefix(r.Ref, prefix), SHA: r.Object.SHA})
	}
	return out, nil
}

func (c *Client) Branches(ctx context.Context, name string) ([]forge.Ref, error) {
	return c.refs(ctx, name, "heads")
}

func (c *Client) DeleteBranch(ctx context.Context, name, branch string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name)+"/git/refs/heads/"+escapeRef(branch), nil, nil)
	return err
}

func (c *Client) DeleteTag(ctx context.Context, name, tag string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name)+"/git/refs/tags/"+escapeRef(tag), nil, nil)
	return err
}

// AllowForcePush has nothing to lift: forgelab does not protect branches on this forge yet.
func (c *Client) AllowForcePush(context.Context, string, string) error { return nil }

func (c *Client) OpenRequests(ctx context.Context, name string) ([]forge.Request, error) {
	type pull struct {
		Number int    `json:"number"`
		Title  string `json:"title"`
	}
	items, err := list[pull](ctx, c, c.repoPath(name)+"/pulls?state=open")
	if err != nil {
		return nil, err
	}
	var out []forge.Request
	for _, p := range items {
		out = append(out, forge.Request{Number: p.Number, Title: p.Title})
	}
	return out, nil
}

// CloseRequest closes; GitHub cannot delete a pull request, and its number is never reused.
func (c *Client) CloseRequest(ctx context.Context, name string, number int) error {
	_, err := c.do(ctx, http.MethodPatch, c.repoPath(name)+"/pulls/"+strconv.Itoa(number),
		map[string]any{"state": "closed"}, nil)
	return err
}

// GitURL embeds the token in the remote so that no credential helper is involved. GitHub
// accepts a token as the password for any username; x-access-token is the conventional one.
func (c *Client) GitURL(name string) (string, error) {
	u, err := url.Parse(c.gitURL)
	if err != nil {
		return "", err
	}
	u.User = url.UserPassword("x-access-token", c.token)
	u.Path = strings.TrimRight(u.Path, "/") + "/" + c.org + "/" + name + ".git"
	return u.String(), nil
}

// escapeRef escapes each path segment but keeps the slashes a ref name may contain.
func escapeRef(ref string) string {
	parts := strings.Split(ref, "/")
	for i, p := range parts {
		parts[i] = url.PathEscape(p)
	}
	return strings.Join(parts, "/")
}
