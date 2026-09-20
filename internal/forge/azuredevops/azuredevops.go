// Package azuredevops implements forge.Forge against Azure DevOps Services and Server.
//
// It is the odd one out. Repositories live in a project inside the organisation; they have
// no topics and no visibility of their own; and the nearest thing to "archived" is
// "disabled", which makes a repository unreadable rather than read-only. See Caps.
package azuredevops

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
const DefaultBaseURL = "https://dev.azure.com"

const (
	apiVersion       = "api-version=7.1"
	maxRateLimitWait = 90 * time.Second
	zeroSHA          = "0000000000000000000000000000000000000000"
)

// Client talks to one organisation. Repositories without a namespace live in its default
// project; a namespace's first segment names another project, and the rest is joined into the
// name. Every project is made by Create when first needed, the default one included.
type Client struct {
	baseURL string
	org     string
	project string
	token   string
	http    *http.Client

	// projects remembers project ids by name. The lock is held across a lookup, so that two
	// creates in a new project make it once.
	projects struct {
		sync.Mutex
		ids  map[string]string
		born map[string]bool // made by this client, so still holding only what they came with
	}

	sleep func(context.Context, time.Duration) error // swapped out in tests
}

var _ forge.Forge = (*Client)(nil)

// New returns a client. baseURL is https://dev.azure.com, or a Server's collection root.
func New(baseURL, org, project, token string, hc *http.Client) (*Client, error) {
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
		org:     org,
		project: project,
		token:   token,
		http:    hc,
		sleep: func(ctx context.Context, d time.Duration) error {
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(d):
				return nil
			}
		},
	}, nil
}

// Caps: no topics (so no marker), visibility belongs to the project, and a disabled
// repository answers 404 to everything but the project's listing.
func (c *Client) Caps() forge.Caps { return forge.Caps{ArchivedUnreadable: true, NamespaceDepth: 1} }

type statusError struct {
	code int
	msg  string
}

func (e *statusError) Error() string { return e.msg }

func hasStatus(err error, codes ...int) bool {
	var se *statusError
	return errors.As(err, &se) && slices.Contains(codes, se.code)
}

// do sends one request. path is relative to the organisation; query carries no api-version.
func (c *Client) do(ctx context.Context, method, path, query string, body, out any) (http.Header, error) {
	var raw []byte
	if body != nil {
		var err error
		if raw, err = json.Marshal(body); err != nil {
			return nil, err
		}
	}
	target := c.baseURL + "/" + url.PathEscape(c.org) + path + "?" + apiVersion
	if query != "" {
		target += "&" + query
	}
	for attempt := 1; ; attempt++ {
		req, err := http.NewRequestWithContext(ctx, method, target, bytes.NewReader(raw))
		if err != nil {
			return nil, err
		}
		req.SetBasicAuth("", c.token) // a PAT is the password of an empty username
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
				return nil, fmt.Errorf("%s %s: rate limited by Azure DevOps; retry in %s", method, path, wait.Round(time.Second))
			}
			if err := c.sleep(ctx, wait); err != nil {
				return nil, err
			}
			continue
		}
		// An expired or wrong PAT is answered with a 203 and a sign-in page, not a 401.
		if resp.StatusCode == http.StatusNonAuthoritativeInfo {
			return nil, &statusError{code: http.StatusUnauthorized, msg: fmt.Sprintf(
				"%s %s: not authenticated: the token is wrong, expired, or not valid for organisation %q", method, path, c.org)}
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

// split lands a fleet name: "dotfiles" is that repository in the sandbox's project, and
// "platform/core/api" is core-api in the project platform.
func (c *Client) split(name string) (project, repo string, err error) {
	project, rest, nested := strings.Cut(name, "/")
	if !nested {
		return c.project, name, nil
	}
	// Otherwise platform/x and x would be one repository, which the fleet cannot see coming.
	if strings.EqualFold(project, c.project) {
		return "", "", fmt.Errorf("namespace %q is the sandbox's own project, where repositories without a namespace already go: rename one of the two", project)
	}
	return project, forge.FlatName(rest), nil
}

func git(project, suffix string) string {
	return "/" + url.PathEscape(project) + "/_apis/git" + suffix
}

// EnsureOrg checks the organisation answers to this token: an organisation cannot be created
// through the API. Projects are made by Create.
func (c *Client) EnsureOrg(ctx context.Context) error {
	_, err := c.do(ctx, http.MethodGet, "/_apis/projects", "$top=1", nil, nil)
	if hasStatus(err, http.StatusNotFound) {
		return fmt.Errorf("organisation %q does not exist, or the token cannot see it", c.org)
	}
	return err
}

// projectIdent resolves a project to its id; the caller holds c.projects. One that is missing
// is reported as such, or with create is made.
func (c *Client) projectIdent(ctx context.Context, project string, create bool) (string, bool, error) {
	if id, ok := c.projects.ids[project]; ok {
		return id, true, nil
	}
	var p struct {
		ID string `json:"id"`
	}
	get := func() error {
		_, err := c.do(ctx, http.MethodGet, "/_apis/projects/"+url.PathEscape(project), "", nil, &p)
		return err
	}
	err := get()
	switch {
	case err == nil:
	case !hasStatus(err, http.StatusNotFound):
		return "", false, err
	case !create:
		return "", false, nil
	default:
		if err := c.createProject(ctx, project); err != nil {
			return "", false, fmt.Errorf("create project %s: %w", project, err)
		}
		if err := get(); err != nil {
			return "", false, err
		}
		if c.projects.born == nil {
			c.projects.born = map[string]bool{}
		}
		c.projects.born[project] = true
	}
	if c.projects.ids == nil {
		c.projects.ids = map[string]string{}
	}
	c.projects.ids[project] = p.ID
	return p.ID, true, nil
}

// createProject makes a private Git project on the organisation's default process, and waits
// for it: Azure DevOps builds a project in the background.
func (c *Client) createProject(ctx context.Context, project string) error {
	var procs struct {
		Value []struct {
			ID        string `json:"id"`
			IsDefault bool   `json:"isDefault"`
		} `json:"value"`
	}
	if _, err := c.do(ctx, http.MethodGet, "/_apis/process/processes", "", nil, &procs); err != nil {
		return err
	}
	if len(procs.Value) == 0 {
		return errors.New("the organisation lists no process to create it on")
	}
	process := procs.Value[0].ID
	for _, p := range procs.Value {
		if p.IsDefault {
			process = p.ID
		}
	}
	var op operation
	_, err := c.do(ctx, http.MethodPost, "/_apis/projects", "", map[string]any{
		"name":        project,
		"description": forge.NamespaceMarker,
		"visibility":  "private",
		"capabilities": map[string]any{
			"versioncontrol":  map[string]any{"sourceControlType": "Git"},
			"processTemplate": map[string]any{"templateTypeId": process},
		},
	}, &op)
	if err != nil {
		return err
	}
	return c.wait(ctx, op)
}

type operation struct {
	ID            string `json:"id"`
	Status        string `json:"status"`
	ResultMessage string `json:"resultMessage"`
}

// wait polls a background operation, for about three minutes.
func (c *Client) wait(ctx context.Context, op operation) error {
	for attempt := 1; attempt <= 90; attempt++ {
		if err := c.sleep(ctx, 2*time.Second); err != nil {
			return err
		}
		if _, err := c.do(ctx, http.MethodGet, "/_apis/operations/"+url.PathEscape(op.ID), "", nil, &op); err != nil {
			return err
		}
		switch op.Status {
		case "succeeded":
			return nil
		case "failed", "cancelled":
			return fmt.Errorf("operation %s: %s", op.Status, op.ResultMessage)
		}
	}
	return fmt.Errorf("operation %s is still %s", op.ID, op.Status)
}

type repository struct {
	ID            string `json:"id"`
	Name          string `json:"name"`
	DefaultBranch string `json:"defaultBranch"`
	IsDisabled    bool   `json:"isDisabled"`

	project string // where lookup found it
}

// lookup finds a repository by name. Azure DevOps caches both ways of asking for about a
// second, in opposite directions, so each is used for what it gets right:
//
//   - a direct GET sees a new repository at once, but answers 404 for a disabled one and
//     keeps answering 200 for one that was just deleted (Get catches that: its refs are gone);
//   - the project's listing is the only place a disabled repository shows up, but it lags on
//     repositories just created and on a flag just changed.
//
// So: GET first, and on a 404 the listing decides between "disabled" and "missing". If the
// listing still calls it enabled, one of the two caches is stale; look again shortly.
func (c *Client) lookup(ctx context.Context, name string) (repository, bool, error) {
	project, name, err := c.split(name)
	if err != nil {
		return repository{}, false, err
	}
	r := repository{project: project}
	_, err = c.do(ctx, http.MethodGet, git(project, "/repositories/"+url.PathEscape(name)), "", nil, &r)
	if err == nil {
		return r, strings.EqualFold(r.Name, name), nil
	}
	if !hasStatus(err, http.StatusNotFound) {
		return repository{}, false, err
	}
	for attempt := 1; ; attempt++ {
		var all struct {
			Value []repository `json:"value"`
		}
		if _, err := c.do(ctx, http.MethodGet, git(project, "/repositories"), "", nil, &all); err != nil {
			if hasStatus(err, http.StatusNotFound) {
				return repository{}, false, nil // no such project yet: Create makes it
			}
			return repository{}, false, err
		}
		i := slices.IndexFunc(all.Value, func(r repository) bool { return strings.EqualFold(r.Name, name) })
		switch {
		case i < 0:
			return repository{}, false, nil
		case all.Value[i].IsDisabled:
			all.Value[i].project = project
			return all.Value[i], true, nil
		case attempt == 4:
			return repository{}, false, nil // listed as enabled, yet unreadable: gone
		}
		if err := c.sleep(ctx, time.Second); err != nil {
			return repository{}, false, err
		}
	}
}

func (c *Client) Get(ctx context.Context, name string) (forge.Repo, bool, error) {
	r, found, err := c.lookup(ctx, name)
	if err != nil || !found {
		return forge.Repo{}, false, err
	}
	out := forge.Repo{
		Name:          r.Name,
		DefaultBranch: strings.TrimPrefix(r.DefaultBranch, "refs/heads/"),
		Visibility:    "private", // the project's; not comparable per repository, see Caps
		Archived:      r.IsDisabled,
		Topics:        []string{},
	}
	if r.IsDisabled {
		return out, true, nil // unreadable: nothing more can be learned
	}
	// `size` stays 0 well after a push, so emptiness is read from the refs.
	branches, err := c.Branches(ctx, name)
	if hasStatus(err, http.StatusNotFound) {
		return forge.Repo{}, false, nil // the GET answered from cache for a repository just deleted
	}
	if err != nil {
		return forge.Repo{}, false, err
	}
	out.Empty = len(branches) == 0
	return out, true, nil
}

// Create ignores visibility, defaultBranch and topics: none of them exists per repository at
// creation. The first branch pushed becomes the default.
func (c *Client) Create(ctx context.Context, name, _, _ string, _ []string) error {
	project, name, err := c.split(name)
	if err != nil {
		return err
	}
	c.projects.Lock()
	pid, _, err := c.projectIdent(ctx, project, true)
	born := c.projects.born[project]
	c.projects.Unlock()
	if err != nil {
		return err
	}
	_, err = c.do(ctx, http.MethodPost, git(project, "/repositories"), "", map[string]any{
		"name":    name,
		"project": map[string]any{"id": pid},
	}, nil)
	// A project made a moment ago came with an empty repository of its own name, and a
	// fixture by that name takes it over. In any other project a conflict is a conflict.
	if hasStatus(err, http.StatusConflict) && born && strings.EqualFold(name, project) {
		return nil
	}
	return err
}

// Delete removes the repository for good. A DELETE alone only moves it to the project's
// recycle bin; the name is free again at once, but the repository lingers, so it is purged.
func (c *Client) Delete(ctx context.Context, name string) error {
	r, found, err := c.lookup(ctx, name)
	if err != nil || !found {
		return err
	}
	// A disabled repository answers 404 to everything, its own deletion included.
	if r.IsDisabled {
		if _, err := c.do(ctx, http.MethodPatch, git(r.project, "/repositories/"+r.ID), "", map[string]any{"isDisabled": false}, nil); err != nil {
			return fmt.Errorf("enable before delete: %w", err)
		}
	}
	if _, err := c.do(ctx, http.MethodDelete, git(r.project, "/repositories/"+r.ID), "", nil, nil); err != nil {
		return err
	}
	_, _ = c.do(ctx, http.MethodDelete, git(r.project, "/recycleBin/repositories/"+r.ID), "", nil, nil)
	return nil
}

// DeleteNamespace removes a project forgelab made, with all it holds. Only the first segment
// of a namespace is a project (see Caps), and "" is the default one.
func (c *Client) DeleteNamespace(ctx context.Context, ns string) (bool, string, error) {
	if strings.Contains(ns, "/") {
		return false, "", nil
	}
	if ns == "" {
		ns = c.project
	}
	c.projects.Lock()
	defer c.projects.Unlock()
	var p struct {
		ID          string `json:"id"`
		Description string `json:"description"`
	}
	if _, err := c.do(ctx, http.MethodGet, "/_apis/projects/"+url.PathEscape(ns), "", nil, &p); err != nil {
		if hasStatus(err, http.StatusNotFound) {
			return false, "", nil
		}
		return false, "", err
	}
	if !strings.HasPrefix(p.Description, forge.NamespaceMarker) {
		return false, "not created by forgelab", nil
	}
	delete(c.projects.ids, ns)
	delete(c.projects.born, ns)

	var op operation
	if _, err := c.do(ctx, http.MethodDelete, "/_apis/projects/"+p.ID, "", nil, &op); err != nil {
		return false, "", err
	}
	if err := c.wait(ctx, op); err != nil {
		return false, "", err
	}
	return true, "", nil
}

func (c *Client) UpdateSettings(ctx context.Context, name string, s forge.Settings) error {
	r, found, err := c.lookup(ctx, name)
	if err != nil {
		return err
	}
	if !found {
		return fmt.Errorf("repository %q not found", name)
	}
	patch := func(fields map[string]any) error {
		_, err := c.do(ctx, http.MethodPatch, git(r.project, "/repositories/"+r.ID), "", fields, nil)
		return err
	}
	// A disabled repository rejects everything else, so enable first and disable last.
	if s.Archived != nil && !*s.Archived && r.IsDisabled {
		if err := patch(map[string]any{"isDisabled": false}); err != nil {
			return err
		}
	}
	if s.DefaultBranch != nil && "refs/heads/"+*s.DefaultBranch != r.DefaultBranch {
		if err := patch(map[string]any{"defaultBranch": "refs/heads/" + *s.DefaultBranch}); err != nil {
			return err
		}
	}
	if s.Archived != nil && *s.Archived && !r.IsDisabled {
		return patch(map[string]any{"isDisabled": true})
	}
	return nil
}

// SetTopics has nowhere to put them.
func (c *Client) SetTopics(context.Context, string, []string) error { return nil }

// AllowForcePush has nothing to lift: force-push is a permission here, not a branch setting,
// and whoever may delete repositories has it.
func (c *Client) AllowForcePush(context.Context, string, string) error { return nil }

type ref struct {
	Name     string `json:"name"`
	ObjectID string `json:"objectId"`
}

// refs lists refs under a prefix such as "heads/", following the continuation token.
func (c *Client) refs(ctx context.Context, name, filter string) ([]ref, error) {
	project, name, err := c.split(name)
	if err != nil {
		return nil, err
	}
	return c.refsIn(ctx, project, name, filter)
}

func (c *Client) refsIn(ctx context.Context, project, name, filter string) ([]ref, error) {
	var all []ref
	for token := ""; ; {
		q := "filter=" + url.QueryEscape(filter)
		if token != "" {
			q += "&continuationToken=" + url.QueryEscape(token)
		}
		var page struct {
			Value []ref `json:"value"`
		}
		hdr, err := c.do(ctx, http.MethodGet, git(project, "/repositories/"+url.PathEscape(name)+"/refs"), q, nil, &page)
		if err != nil {
			return nil, err
		}
		all = append(all, page.Value...)
		if token = hdr.Get("x-ms-continuationtoken"); token == "" {
			return all, nil
		}
	}
}

func (c *Client) Branches(ctx context.Context, name string) ([]forge.Ref, error) {
	items, err := c.refs(ctx, name, "heads/")
	if err != nil {
		return nil, err
	}
	var out []forge.Ref
	for _, r := range items {
		out = append(out, forge.Ref{Name: strings.TrimPrefix(r.Name, "refs/heads/"), SHA: r.ObjectID})
	}
	return out, nil
}

// deleteRef updates the ref to the zero id, which needs the id it currently points at.
func (c *Client) deleteRef(ctx context.Context, name, full string) error {
	project, name, err := c.split(name)
	if err != nil {
		return err
	}
	items, err := c.refsIn(ctx, project, name, strings.TrimPrefix(full, "refs/"))
	if err != nil {
		return err
	}
	for _, r := range items {
		if r.Name != full { // the filter is a prefix match
			continue
		}
		var res struct {
			Value []struct {
				Success      bool   `json:"success"`
				UpdateStatus string `json:"updateStatus"`
			} `json:"value"`
		}
		body := []map[string]string{{"name": full, "oldObjectId": r.ObjectID, "newObjectId": zeroSHA}}
		if _, err := c.do(ctx, http.MethodPost, git(project, "/repositories/"+url.PathEscape(name)+"/refs"), "", body, &res); err != nil {
			return err
		}
		if len(res.Value) == 0 || !res.Value[0].Success {
			status := "no result"
			if len(res.Value) > 0 {
				status = res.Value[0].UpdateStatus
			}
			return fmt.Errorf("delete %s: %s", full, status)
		}
		return nil
	}
	return nil // already gone
}

func (c *Client) DeleteBranch(ctx context.Context, name, branch string) error {
	return c.deleteRef(ctx, name, "refs/heads/"+branch)
}

func (c *Client) DeleteTag(ctx context.Context, name, tag string) error {
	return c.deleteRef(ctx, name, "refs/tags/"+tag)
}

func (c *Client) OpenRequests(ctx context.Context, name string) ([]forge.Request, error) {
	project, name, err := c.split(name)
	if err != nil {
		return nil, err
	}
	const pageSize = 100
	var out []forge.Request
	for skip := 0; ; skip += pageSize {
		var page struct {
			Value []struct {
				ID    int    `json:"pullRequestId"`
				Title string `json:"title"`
			} `json:"value"`
		}
		q := fmt.Sprintf("searchCriteria.status=active&$top=%d&$skip=%d", pageSize, skip)
		if _, err := c.do(ctx, http.MethodGet, git(project, "/repositories/"+url.PathEscape(name)+"/pullrequests"), q, nil, &page); err != nil {
			return nil, err
		}
		for _, p := range page.Value {
			out = append(out, forge.Request{Number: p.ID, Title: p.Title})
		}
		if len(page.Value) < pageSize {
			return out, nil
		}
	}
}

// CloseRequest abandons. The id is unique across the project, not per repository, and is
// never reused.
func (c *Client) CloseRequest(ctx context.Context, name string, number int) error {
	project, name, err := c.split(name)
	if err != nil {
		return err
	}
	_, err = c.do(ctx, http.MethodPatch,
		git(project, "/repositories/"+url.PathEscape(name)+"/pullrequests/"+strconv.Itoa(number)), "",
		map[string]any{"status": "abandoned"}, nil)
	return err
}

// GitURL embeds the token in the remote so that no credential helper is involved. Any
// username works with a PAT as the password.
func (c *Client) GitURL(name string) (string, error) {
	project, name, err := c.split(name)
	if err != nil {
		return "", err
	}
	u, err := url.Parse(c.baseURL)
	if err != nil {
		return "", err
	}
	u.User = url.UserPassword("forgelab", c.token)
	u = u.JoinPath(c.org, project, "_git", name)
	return u.String(), nil
}
