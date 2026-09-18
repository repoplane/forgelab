// Package forgejo implements forge.Forge against the Forgejo (and Gitea) API.
package forgejo

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/repoplane/forgelab/internal/forge"
)

// Client talks to one organisation on one Forgejo instance.
type Client struct {
	baseURL string
	org     string
	token   string
	http    *http.Client
}

var _ forge.Forge = (*Client)(nil)

// New returns a client. hc may be nil.
func New(baseURL, org, token string, hc *http.Client) *Client {
	if hc == nil {
		hc = &http.Client{Timeout: 30 * time.Second}
	}
	return &Client{baseURL: strings.TrimRight(baseURL, "/"), org: org, token: token, http: hc}
}

// statusError keeps the HTTP status so callers can tell "not there" from "broken".
type statusError struct {
	code int
	msg  string
}

func (e *statusError) Error() string { return e.msg }

func hasStatus(err error, codes ...int) bool {
	var se *statusError
	if !errors.As(err, &se) {
		return false
	}
	for _, c := range codes {
		if se.code == c {
			return true
		}
	}
	return false
}

func (c *Client) do(ctx context.Context, method, path string, body, out any) (http.Header, error) {
	var rdr io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		rdr = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+"/api/v1"+path, rdr)
	if err != nil {
		return nil, err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	req.Header.Set("Authorization", "token "+c.token)
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return nil, &statusError{code: resp.StatusCode, msg: fmt.Sprintf("%s %s: %s: %s",
			method, path, resp.Status, strings.TrimSpace(string(raw)))}
	}
	if out != nil && len(raw) > 0 {
		return resp.Header, json.Unmarshal(raw, out)
	}
	return resp.Header, nil
}

// list pages through a collection. It stops on X-Total-Count or an empty page, never on a
// short page: the server clamps `limit` to its own maximum, so a page shorter than the one
// asked for is not evidence of being the last.
func list[T any](ctx context.Context, c *Client, path string) ([]T, error) {
	sep := "?"
	if strings.Contains(path, "?") {
		sep = "&"
	}
	var all []T
	for page := 1; ; page++ {
		var items []T
		hdr, err := c.do(ctx, http.MethodGet, fmt.Sprintf("%s%spage=%d&limit=50", path, sep, page), nil, &items)
		if err != nil {
			return nil, err
		}
		all = append(all, items...)
		if len(items) == 0 {
			return all, nil
		}
		if total, err := strconv.Atoi(hdr.Get("X-Total-Count")); err == nil && len(all) >= total {
			return all, nil
		}
	}
}

func (c *Client) repoPath(name string) string {
	return "/repos/" + url.PathEscape(c.org) + "/" + url.PathEscape(name)
}

func (c *Client) EnsureOrg(ctx context.Context) error {
	_, err := c.do(ctx, http.MethodGet, "/orgs/"+url.PathEscape(c.org), nil, nil)
	if !hasStatus(err, http.StatusNotFound) {
		return err
	}
	_, err = c.do(ctx, http.MethodPost, "/orgs", map[string]any{
		"username":   c.org,
		"visibility": "public",
	}, nil)
	return err
}

func (c *Client) Get(ctx context.Context, name string) (forge.Repo, bool, error) {
	var raw struct {
		Name          string `json:"name"`
		DefaultBranch string `json:"default_branch"`
		Private       bool   `json:"private"`
		Archived      bool   `json:"archived"`
		Empty         bool   `json:"empty"`
	}
	if _, err := c.do(ctx, http.MethodGet, c.repoPath(name), nil, &raw); err != nil {
		if hasStatus(err, http.StatusNotFound) {
			return forge.Repo{}, false, nil
		}
		return forge.Repo{}, false, err
	}
	// A renamed repository's old name redirects to it. That is not the repository asked for.
	if !strings.EqualFold(raw.Name, name) {
		return forge.Repo{}, false, nil
	}

	var topics struct {
		Topics []string `json:"topics"`
	}
	if _, err := c.do(ctx, http.MethodGet, c.repoPath(name)+"/topics", nil, &topics); err != nil {
		return forge.Repo{}, false, err
	}

	r := forge.Repo{
		Name:          raw.Name,
		DefaultBranch: raw.DefaultBranch,
		Visibility:    "public",
		Archived:      raw.Archived,
		Empty:         raw.Empty,
		Topics:        topics.Topics,
	}
	if raw.Private {
		r.Visibility = "private"
	}
	return r, true, nil
}

func (c *Client) Create(ctx context.Context, name, visibility, defaultBranch string) error {
	_, err := c.do(ctx, http.MethodPost, "/orgs/"+url.PathEscape(c.org)+"/repos", map[string]any{
		"name":           name,
		"auto_init":      false,
		"default_branch": defaultBranch,
		"private":        visibility == "private",
	}, nil)
	return err
}

func (c *Client) Delete(ctx context.Context, name string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name), nil, nil)
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
	_, err := c.do(ctx, http.MethodPut, c.repoPath(name)+"/topics", map[string]any{"topics": topics}, nil)
	return err
}

func (c *Client) Branches(ctx context.Context, name string) ([]forge.Ref, error) {
	type branch struct {
		Name   string `json:"name"`
		Commit struct {
			ID string `json:"id"`
		} `json:"commit"`
	}
	items, err := list[branch](ctx, c, c.repoPath(name)+"/branches")
	if err != nil {
		return nil, err
	}
	var out []forge.Ref
	for _, b := range items {
		out = append(out, forge.Ref{Name: b.Name, SHA: b.Commit.ID})
	}
	return out, nil
}

func (c *Client) Tags(ctx context.Context, name string) ([]forge.Ref, error) {
	type tag struct {
		Name   string `json:"name"`
		Commit struct {
			SHA string `json:"sha"`
		} `json:"commit"`
	}
	items, err := list[tag](ctx, c, c.repoPath(name)+"/tags")
	if err != nil {
		return nil, err
	}
	var out []forge.Ref
	for _, t := range items {
		out = append(out, forge.Ref{Name: t.Name, SHA: t.Commit.SHA})
	}
	return out, nil
}

func (c *Client) DeleteBranch(ctx context.Context, name, branch string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name)+"/branches/"+escapeRef(branch), nil, nil)
	return err
}

func (c *Client) DeleteTag(ctx context.Context, name, tag string) error {
	_, err := c.do(ctx, http.MethodDelete, c.repoPath(name)+"/tags/"+escapeRef(tag), nil, nil)
	return err
}

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

func (c *Client) CloseRequest(ctx context.Context, name string, number int) error {
	_, err := c.do(ctx, http.MethodPatch, c.repoPath(name)+"/pulls/"+strconv.Itoa(number),
		map[string]any{"state": "closed"}, nil)
	return err
}

// GitURL embeds the token in the remote so that no credential helper is involved.
// url.UserPassword percent-encodes the userinfo, which matters because a token is opaque
// and may contain characters that are significant in a URL. Forgejo identifies the user
// from the token, so the username is a placeholder.
func (c *Client) GitURL(name string) (string, error) {
	u, err := url.Parse(c.baseURL)
	if err != nil {
		return "", fmt.Errorf("parse base URL %q: %w", c.baseURL, err)
	}
	if u.Scheme == "" || u.Host == "" {
		return "", fmt.Errorf("base URL %q has no scheme or host", c.baseURL)
	}
	u.User = url.UserPassword("forgelab", c.token)
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
