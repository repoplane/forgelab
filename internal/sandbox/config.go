package sandbox

import (
	"fmt"
	"net"
	"net/url"
	"os"
	"regexp"

	"gopkg.in/yaml.v3"

	"github.com/repoplane/forgelab/internal/forge/github"
	"github.com/repoplane/forgelab/internal/forge/gitlab"
)

// ConfigFile is the default name of the sandbox configuration inside a fleet directory.
const ConfigFile = "sandboxes.yaml"

// DefaultMarkerTopic is set on every repository forgelab creates. It is how forgelab tells
// its own repositories from a same-named stranger, and how a consumer can filter an
// organisation listing down to the fleet.
const DefaultMarkerTopic = "forgelab-managed"

const configVersion = 1

// Config is sandboxes.yaml: where a fleet may be applied.
type Config struct {
	Version int `yaml:"version"`
	// OrgAllowlist is a regular expression every non-loopback org must match. Choose it so
	// that it cannot match the organisation holding your real code.
	OrgAllowlist string             `yaml:"org_allowlist"`
	Sandboxes    map[string]Sandbox `yaml:"sandboxes"`
}

// Sandbox is one forge organisation. Credentials are descriptors, never values: the file
// names an environment variable, so nothing secret is committed or passed as an argument.
type Sandbox struct {
	Name        string `yaml:"-"`
	Forge       string `yaml:"forge"`
	BaseURL     string `yaml:"base_url"`
	Org         string `yaml:"org"` // on GitLab: the group's full path, e.g. acme-sandbox/services
	TokenEnv    string `yaml:"token_env"`
	MarkerTopic string `yaml:"marker_topic"`
}

// LoadConfig reads sandboxes.yaml.
func LoadConfig(path string) (*Config, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var c Config
	if err := yaml.Unmarshal(raw, &c); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	if c.Version != configVersion {
		return nil, fmt.Errorf("%s: version is %d, want %d", path, c.Version, configVersion)
	}
	return &c, nil
}

// Sandbox resolves a sandbox by name and checks it against the allowlist. The org is only
// reachable through here: there is no flag that takes one.
func (c *Config) Sandbox(name string) (Sandbox, error) {
	sb, ok := c.Sandboxes[name]
	if !ok {
		return Sandbox{}, fmt.Errorf("no sandbox %q in the config", name)
	}
	sb.Name = name
	if sb.MarkerTopic == "" {
		sb.MarkerTopic = DefaultMarkerTopic
	}
	// Only a self-hosted GitHub or GitLab needs to say where it lives.
	if sb.BaseURL == "" {
		switch sb.Forge {
		case "github":
			sb.BaseURL = github.DefaultBaseURL
		case "gitlab":
			sb.BaseURL = gitlab.DefaultBaseURL
		}
	}
	if sb.Forge == "" || sb.BaseURL == "" || sb.Org == "" || sb.TokenEnv == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: forge, base_url, org and token_env are required", name)
	}
	if err := c.checkAllowlist(sb); err != nil {
		return Sandbox{}, err
	}
	return sb, nil
}

// checkAllowlist is the one guard that is a claim in a file rather than a fact on the
// forge. A loopback forge is exempt: there is no production organisation on a laptop, and
// requiring the pattern there would only teach people to loosen it.
func (c *Config) checkAllowlist(sb Sandbox) error {
	u, err := url.Parse(sb.BaseURL)
	if err != nil || u.Hostname() == "" {
		return fmt.Errorf("sandbox %q: base_url %q is not a URL", sb.Name, sb.BaseURL)
	}
	if isLoopback(u.Hostname()) {
		return nil
	}
	if c.OrgAllowlist == "" {
		return &GuardError{Msg: fmt.Sprintf(
			"sandbox %q is not on loopback and the config has no org_allowlist", sb.Name)}
	}
	re, err := regexp.Compile(c.OrgAllowlist)
	if err != nil {
		return fmt.Errorf("org_allowlist: %w", err)
	}
	if !re.MatchString(sb.Org) {
		return &GuardError{Msg: fmt.Sprintf(
			"sandbox %q: org %q does not match org_allowlist %q", sb.Name, sb.Org, c.OrgAllowlist)}
	}
	return nil
}

func isLoopback(host string) bool {
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}
