package sandbox

import (
	"fmt"
	"net/url"
	"os"

	"gopkg.in/yaml.v3"

	"github.com/repoplane/forgelab/internal/forge/azuredevops"
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
	Version   int                `yaml:"version"`
	Sandboxes map[string]Sandbox `yaml:"sandboxes"`

	// OrgAllowlist is no longer used; it is still parsed so that Open can say so. It was a
	// pattern the org had to match, kept in the same file as the org it was checking -- a
	// speed bump, not a guard. What scopes a sandbox is what its token can reach, and what
	// protects a wrong target is checked on the forge: the marker, and declared-repos-only.
	OrgAllowlist string `yaml:"org_allowlist"`
}

// Sandbox is one forge organisation. Credentials are descriptors, never values: the file
// names an environment variable, so nothing secret is committed or passed as an argument.
type Sandbox struct {
	Name    string `yaml:"-"`
	Forge   string `yaml:"forge"`
	BaseURL string `yaml:"base_url"`
	Org     string `yaml:"org"` // on GitLab: the group's full path, e.g. acme-sandbox/services
	// Project is required on Azure DevOps, where repositories live in a project inside the
	// organisation, and unused elsewhere.
	Project     string `yaml:"project"`
	TokenEnv    string `yaml:"token_env"`
	MarkerTopic string `yaml:"marker_topic"`
}

// Scope is where the sandbox writes: the org, or org/project on a forge that has projects.
func (s Sandbox) Scope() string {
	if s.Project != "" {
		return s.Org + "/" + s.Project
	}
	return s.Org
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

// Sandbox resolves a sandbox by name. The org is only reachable through here: there is no
// flag that takes one.
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
		case "azuredevops":
			sb.BaseURL = azuredevops.DefaultBaseURL
		}
	}
	if sb.Forge == "azuredevops" && sb.Project == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: azuredevops needs a project", name)
	}
	if sb.Forge == "" || sb.BaseURL == "" || sb.Org == "" || sb.TokenEnv == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: forge, base_url, org and token_env are required", name)
	}
	if u, err := url.Parse(sb.BaseURL); err != nil || u.Scheme == "" || u.Hostname() == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: base_url %q is not a URL", name, sb.BaseURL)
	}
	return sb, nil
}
