// Package gitlab implements forge.Forge against the GitLab REST API (gitlab.com and
// self-managed). The "org" is a group, addressed by its full path, which may be nested.
package gitlab

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/repoplane/forgelab/internal/forge"
)

// DefaultBaseURL is used when a sandbox names no base_url.
const DefaultBaseURL = "https://gitlab.com"

// maxRateLimitWait bounds how long a request sleeps on a 429 before giving up.
const maxRateLimitWait = 90 * time.Second

// Client talks to one group, and to the subgroups the fleet's namespaces map to.
type Client struct {
	baseURL string
	group   string
	token   string
	http    *http.Client

	// groups remembers the group and its subgroups by namespace; "" is the group itself. The
	// lock is held across a lookup, so that two creates in a new subgroup make it once.
	groups struct {
		sync.Mutex
		byNS map[string]groupInfo
	}

	sleep func(context.Context, time.Duration) error // swapped out in tests
}

var _ forge.Forge = (*Client)(nil)

// New returns a client. baseURL is the web address, e.g. https://gitlab.com; hc may be nil.
func New(baseURL, group, token string, hc *http.Client) (*Client, error) {
	if baseURL == "" {
		baseURL = DefaultBaseURL
	}
	u, err := url.Parse(baseURL)
	if err != nil || u.Scheme == "" || u.Host == "" {
		return nil, fmt.Errorf("base URL %q has no scheme or host", baseURL)
	}
	if hc == nil {
		hc = &http.Client{Timeout: 30 * time.Second}
	}
	return &Client{
		baseURL: strings.TrimRight(baseURL, "/"),
		group:   strings.Trim(group, "/"),
		token:   token,
		http:    hc,
		sleep:   sleepCtx,
	}, nil
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

// do sends one request to a path under /api/v4. Paths carry %2F-encoded project paths, so
// they are passed to the URL parser whole rather than assembled from decoded parts.
func (c *Client) do(ctx context.Context, method, path string, body, out any) (http.Header, error) {
	var raw []byte
	if body != nil {
		var err error
		if raw, err = json.Marshal(body); err != nil {
			return nil, err
		}
	}
	for attempt := 1; ; attempt++ {
		req, err := http.NewRequestWithContext(ctx, method, c.baseURL+"/api/v4"+path, bytes.NewReader(raw))
		if err != nil {
			return nil, err
		}
		req.Header.Set("PRIVATE-TOKEN", c.token)
		if body != nil {
			req.Header.Set("Content-Type", "application/json")
		}
		resp, err := c.http.Do(req)
		if err != nil {
			return nil, err
		}
		payload, _ := io.ReadAll(resp.Body)
		resp.Body.Close()

		if resp.StatusCode == http.StatusTooManyRequests {
			wait := time.Minute
			if s, err := strconv.Atoi(resp.Header.Get("Retry-After")); err == nil {
				wait = time.Duration(s+1) * time.Second
			}
			if attempt >= 4 || wait > maxRateLimitWait {
				return nil, fmt.Errorf("%s %s: rate limited by GitLab; retry in %s", method, path, wait.Round(time.Second))
			}
			if err := c.sleep(ctx, wait); err != nil {
				return nil, err
			}
			continue
		}
		if resp.StatusCode < 200 || resp.StatusCode > 299 {
			return nil, &statusError{code: resp.StatusCode, msg: fmt.Sprintf("%s %s: %s: %s",
				method, path, resp.Status, strings.TrimSpace(string(payload)))}
		}
		if out != nil && len(payload) > 0 {
			return resp.Header, json.Unmarshal(payload, out)
		}
		return resp.Header, nil
	}
}

// list follows X-Next-Page until it is empty, which is the only authoritative signal: the
// total-count headers are omitted on large collections.
func list[T any](ctx context.Context, c *Client, path string) ([]T, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	var all []T
	for page := "1"; page != ""; {
		var items []T
		hdr, err := c.do(ctx, http.MethodGet, path+sep+"per_page=100&page="+page, nil, &items)
		if err != nil {
			return nil, err
		}
		all = append(all, items...)
		page = hdr.Get("X-Next-Page")
	}
	return all, nil
}

// project is the API path of one project, addressed by its URL-encoded full path.
func (c *Client) project(name string) string {
	return "/projects/" + url.PathEscape(c.group+"/"+name)
}

// Caps: everything forgelab declares has a home here.
func (c *Client) Caps() forge.Caps {
	return forge.Caps{Topics: true, Visibility: true, Namespaces: true}
}

// EnsureOrg only checks: on gitlab.com a top-level group cannot be created through the API.
func (c *Client) EnsureOrg(ctx context.Context) error {
	c.groups.Lock()
	defer c.groups.Unlock()
	_, _, err := c.groupOf(ctx, "", false)
	return err
}

type groupInfo struct {
	ID         int    `json:"id"`
	Visibility string `json:"visibility"`
}

// groupOf resolves a namespace to its group; the caller holds c.groups. The sandbox's own
// group must exist. A missing subgroup is reported as such, or with create is made, parents
// first, as visible as its parent: a subgroup cannot be more, and a public fixture needs
// every group above it to be.
func (c *Client) groupOf(ctx context.Context, ns string, create bool) (groupInfo, bool, error) {
	if g, ok := c.groups.byNS[ns]; ok {
		return g, true, nil
	}
	full := c.group
	if ns != "" {
		full += "/" + ns
	}
	var g groupInfo
	_, err := c.do(ctx, http.MethodGet, "/groups/"+url.PathEscape(full)+"?with_projects=false", nil, &g)
	switch {
	case err == nil:
	case !hasStatus(err, http.StatusNotFound):
		return groupInfo{}, false, err
	case ns == "":
		return groupInfo{}, false, fmt.Errorf("group %q does not exist, or the token cannot see it", c.group)
	case !create:
		return groupInfo{}, false, nil
	default:
		parentNS, leaf := "", ns
		if i := strings.LastIndex(ns, "/"); i >= 0 {
			parentNS, leaf = ns[:i], ns[i+1:]
		}
		parent, _, err := c.groupOf(ctx, parentNS, true)
		if err != nil {
			return groupInfo{}, false, err
		}
		_, err = c.do(ctx, http.MethodPost, "/groups", map[string]any{
			"name": leaf, "path": leaf, "parent_id": parent.ID, "visibility": parent.Visibility,
		}, &g)
		if err != nil {
			return groupInfo{}, false, fmt.Errorf("create subgroup %s: %w", full, err)
		}
	}
	if c.groups.byNS == nil {
		c.groups.byNS = map[string]groupInfo{}
	}
	c.groups.byNS[ns] = g
	return g, true, nil
}

func (c *Client) Get(ctx context.Context, name string) (forge.Repo, bool, error) {
	var raw struct {
		FullPath      string   `json:"path_with_namespace"`
		DefaultBranch string   `json:"default_branch"`
		Visibility    string   `json:"visibility"`
		Archived      bool     `json:"archived"`
		EmptyRepo     bool     `json:"empty_repo"`
		Topics        []string `json:"topics"`
		Deleting      *string  `json:"marked_for_deletion_on"`
	}
	if _, err := c.do(ctx, http.MethodGet, c.project(name), nil, &raw); err != nil {
		if hasStatus(err, http.StatusNotFound) {
			return forge.Repo{}, false, nil
		}
		return forge.Repo{}, false, err
	}
	// A project answering under another path was renamed; one scheduled for deletion is
	// already gone as far as the fleet is concerned.
	if !strings.EqualFold(raw.FullPath, c.group+"/"+name) || raw.Deleting != nil {
		return forge.Repo{}, false, nil
	}

	r := forge.Repo{
		Name:          name,
		DefaultBranch: raw.DefaultBranch,
		Visibility:    raw.Visibility,
		Archived:      raw.Archived,
		Empty:         raw.EmptyRepo,
		Topics:        raw.Topics,
	}
	// empty_repo is updated after a push, not by it. When it claims "empty", ask the
	// repository itself, so that a project seeded a moment ago is not reported as unseeded.
	if r.Empty {
		branches, err := c.Branches(ctx, name)
		if err != nil {
			return forge.Repo{}, false, err
		}
		r.Empty = len(branches) == 0
	}
	return r, true, nil
}

// Create sets the topics in the same call, so a project never exists unmarked. It ignores
// defaultBranch: the first branch pushed becomes the default, and UpdateSettings pins it.
func (c *Client) Create(ctx context.Context, name, visibility, _ string, topics []string) error {
	ns, leaf := "", name
	if i := strings.LastIndex(name, "/"); i >= 0 {
		ns, leaf = name[:i], name[i+1:]
	}
	c.groups.Lock()
	g, _, err := c.groupOf(ctx, ns, true)
	c.groups.Unlock()
	if err != nil {
		return err
	}
	_, err = c.do(ctx, http.MethodPost, "/projects", map[string]any{
		"name":                   leaf,
		"path":                   leaf,
		"namespace_id":           g.ID,
		"visibility":             visibility,
		"topics":                 topics,
		"initialize_with_readme": false,
	}, nil)
	return err
}

// update edits the project. GitLab answers a burst of updates -- a fleet being applied,
// eight projects at a time -- with a bare 422 "Project could not be updated!" that succeeds
// when simply tried again, so it is, a few times.
func (c *Client) update(ctx context.Context, name string, fields map[string]any) error {
	var err error
	for attempt := 1; attempt <= 4; attempt++ {
		if _, err = c.do(ctx, http.MethodPut, c.project(name), fields, nil); !hasStatus(err, http.StatusUnprocessableEntity) {
			return err
		}
		if serr := c.sleep(ctx, time.Duration(attempt)*time.Second); serr != nil {
			return serr
		}
	}
	return err
}

// Delete removes the project for good. On gitlab.com a first DELETE only schedules it: the
// project is renamed, which frees its path at once, and lingers for days. A second DELETE
// naming that new path removes it now, so that a destroy does not leave a dozen ghosts
// behind each time. The second step is best effort -- the path is already free without it.
func (c *Client) Delete(ctx context.Context, name string) error {
	var before struct {
		ID int `json:"id"`
	}
	if _, err := c.do(ctx, http.MethodGet, c.project(name), nil, &before); err != nil {
		return err
	}
	byID := "/projects/" + strconv.Itoa(before.ID)
	if _, err := c.do(ctx, http.MethodDelete, byID, nil, nil); err != nil {
		return err
	}

	var after struct {
		FullPath string  `json:"path_with_namespace"`
		Deleting *string `json:"marked_for_deletion_on"`
	}
	if _, err := c.do(ctx, http.MethodGet, byID, nil, &after); err != nil || after.Deleting == nil {
		return nil // already gone: deletion was immediate
	}
	_, _ = c.do(ctx, http.MethodDelete,
		byID+"?permanently_remove=true&full_path="+url.QueryEscape(after.FullPath), nil, nil)
	return nil
}

// DeleteNamespace removes an empty subgroup for good, the way Delete does a project: a first
// DELETE may only schedule it, a second naming its path removes it now. Both what it holds
// and its own removal are read a few times over, since GitLab deletes in the background: a
// project deleted a moment ago is still listed, and a subgroup still there would keep its
// parent.
func (c *Client) DeleteNamespace(ctx context.Context, ns string) (bool, error) {
	c.groups.Lock()
	defer c.groups.Unlock()
	g, found, err := c.groupOf(ctx, ns, false)
	if err != nil || !found {
		return false, err
	}
	delete(c.groups.byNS, ns)
	byID := "/groups/" + strconv.Itoa(g.ID)

	for attempt := 1; ; attempt++ {
		var projects, subgroups []struct {
			ID int `json:"id"`
		}
		if _, err := c.do(ctx, http.MethodGet, byID+"/projects?include_subgroups=true&per_page=1", nil, &projects); err != nil {
			return false, err
		}
		if _, err := c.do(ctx, http.MethodGet, byID+"/subgroups?per_page=1", nil, &subgroups); err != nil {
			return false, err
		}
		if len(projects)+len(subgroups) == 0 {
			break
		}
		if attempt == 5 {
			return true, nil
		}
		if err := c.sleep(ctx, 2*time.Second); err != nil {
			return false, err
		}
	}

	if _, err := c.do(ctx, http.MethodDelete, byID, nil, nil); err != nil {
		return false, err
	}
	purged := false
	for attempt := 1; attempt <= 30; attempt++ {
		var after struct {
			FullPath string  `json:"full_path"`
			Deleting *string `json:"marked_for_deletion_on"`
		}
		if _, err := c.do(ctx, http.MethodGet, byID+"?with_projects=false", nil, &after); err != nil {
			if hasStatus(err, http.StatusNotFound) {
				return false, nil
			}
			return false, err
		}
		if after.Deleting != nil && !purged {
			purged = true
			_, _ = c.do(ctx, http.MethodDelete,
				byID+"?permanently_remove=true&full_path="+url.QueryEscape(after.FullPath), nil, nil)
		}
		if err := c.sleep(ctx, time.Second); err != nil {
			return false, err
		}
	}
	return false, nil // still going; its path is what a re-create waits on, not forgelab
}

func (c *Client) UpdateSettings(ctx context.Context, name string, s forge.Settings) error {
	// Archiving has endpoints of its own, and an archived project rejects everything else,
	// so unarchive comes first and archive last.
	if s.Archived != nil && !*s.Archived {
		if _, err := c.do(ctx, http.MethodPost, c.project(name)+"/unarchive", nil, nil); err != nil {
			return err
		}
	}

	fields := map[string]any{}
	if s.DefaultBranch != nil {
		fields["default_branch"] = *s.DefaultBranch
	}
	if s.Visibility != nil {
		fields["visibility"] = *s.Visibility
	}
	if len(fields) > 0 {
		if err := c.update(ctx, name, fields); err != nil {
			return err
		}
	}
	if s.DefaultBranch != nil {
		if err := c.AllowForcePush(ctx, name, *s.DefaultBranch); err != nil {
			return err
		}
	}

	if s.Archived != nil && *s.Archived {
		if _, err := c.do(ctx, http.MethodPost, c.project(name)+"/archive", nil, nil); err != nil {
			return err
		}
	}
	return nil
}

// AllowForcePush removes the branch's protection rule. GitLab protects a default branch the
// moment it is first pushed, and protected means "no force-push": without this, reset fails
// with "You are not allowed to force push code to a protected branch".
//
// A 404 means there is no rule, which is the goal. It is also what GitLab answers if its own
// rule has not been created yet, which is why callers ask again right before each force-push
// instead of relying on the call made at apply time.
func (c *Client) AllowForcePush(ctx context.Context, name, branch string) error {
	_, err := c.do(ctx, http.MethodDelete, c.project(name)+"/protected_branches/"+url.PathEscape(branch), nil, nil)
	if hasStatus(err, http.StatusNotFound) {
		return nil
	}
	return err
}

func (c *Client) SetTopics(ctx context.Context, name string, topics []string) error {
	if topics == nil {
		topics = []string{}
	}
	return c.update(ctx, name, map[string]any{"topics": topics})
}

func (c *Client) refs(ctx context.Context, name, kind string) ([]forge.Ref, error) {
	type ref struct {
		Name   string `json:"name"`
		Commit struct {
			ID string `json:"id"`
		} `json:"commit"`
	}
	items, err := list[ref](ctx, c, c.project(name)+"/repository/"+kind)
	if err != nil {
		// A project with no commits may have no repository to list.
		if hasStatus(err, http.StatusNotFound) {
			return nil, nil
		}
		return nil, err
	}
	var out []forge.Ref
	for _, r := range items {
		out = append(out, forge.Ref{Name: r.Name, SHA: r.Commit.ID})
	}
	return out, nil
}

func (c *Client) Branches(ctx context.Context, name string) ([]forge.Ref, error) {
	return c.refs(ctx, name, "branches")
}

func (c *Client) DeleteBranch(ctx context.Context, name, branch string) error {
	_, err := c.do(ctx, http.MethodDelete, c.project(name)+"/repository/branches/"+url.PathEscape(branch), nil, nil)
	return err
}

func (c *Client) DeleteTag(ctx context.Context, name, tag string) error {
	_, err := c.do(ctx, http.MethodDelete, c.project(name)+"/repository/tags/"+url.PathEscape(tag), nil, nil)
	return err
}

func (c *Client) OpenRequests(ctx context.Context, name string) ([]forge.Request, error) {
	type mr struct {
		IID   int    `json:"iid"`
		Title string `json:"title"`
	}
	items, err := list[mr](ctx, c, c.project(name)+"/merge_requests?state=opened")
	if err != nil {
		return nil, err
	}
	var out []forge.Request
	for _, m := range items {
		out = append(out, forge.Request{Number: m.IID, Title: m.Title})
	}
	return out, nil
}

// CloseRequest closes rather than deletes: closing is enough for verify, needs a lower
// role, and behaves like the other forges. The IID is never reused either way.
func (c *Client) CloseRequest(ctx context.Context, name string, number int) error {
	_, err := c.do(ctx, http.MethodPut, c.project(name)+"/merge_requests/"+strconv.Itoa(number),
		map[string]any{"state_event": "close"}, nil)
	return err
}

// GitURL embeds the token in the remote so that no credential helper is involved. GitLab
// accepts a personal access token as the password; oauth2 is the conventional username.
func (c *Client) GitURL(name string) (string, error) {
	u, err := url.Parse(c.baseURL)
	if err != nil {
		return "", err
	}
	u.User = url.UserPassword("oauth2", c.token)
	u.Path = strings.TrimRight(u.Path, "/") + "/" + c.group + "/" + name + ".git"
	return u.String(), nil
}
