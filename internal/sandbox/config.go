package sandbox

import (
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"

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
	// DefaultProject is required on Azure DevOps, where every repository lives in a project,
	// and unused elsewhere. It holds the repositories without a namespace; a namespace names
	// a project of its own. forgelab makes and removes it like any other.
	DefaultProject string `yaml:"default_project"`
	// OldProject is what default_project was called; still parsed so that Sandbox can say so.
	OldProject  string `yaml:"project"`
	TokenEnv    string `yaml:"token_env"`
	MarkerTopic string `yaml:"marker_topic"`
}

// sandboxName is what a sandbox may be called. The names are typed on a command line and
// offered by shell completion, so they stay free of anything a shell would interpret.
var sandboxName = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]*$`)

// resolvePaths applies the defaults: the fleet is the current directory, and the config
// sits in it.
func resolvePaths(fleetDir, configPath string) (string, string) {
	if fleetDir == "" {
		fleetDir = "."
	}
	if configPath == "" {
		configPath = filepath.Join(fleetDir, ConfigFile)
	}
	return fleetDir, configPath
}

// Names lists the sandboxes a config declares, sorted; nil if the config cannot be read. It
// exists for shell completion, which wants names and no errors -- and whose input is a file
// that may have come with somebody else's repository, so a name a shell could interpret is
// never returned.
func Names(fleetDir, configPath string) []string {
	_, configPath = resolvePaths(fleetDir, configPath)
	cfg, err := LoadConfig(configPath)
	if err != nil {
		return nil
	}
	names := make([]string, 0, len(cfg.Sandboxes))
	for name := range cfg.Sandboxes {
		if sandboxName.MatchString(name) {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	return names
}

// Scope is where the sandbox writes: the org, or org/project on a forge that has projects.
func (s Sandbox) Scope() string {
	if s.DefaultProject != "" {
		return s.Org + "/" + s.DefaultProject
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
	for name := range c.Sandboxes {
		if !sandboxName.MatchString(name) {
			return nil, fmt.Errorf("%s: sandbox name %q: use letters, digits, '.', '_' and '-'", path, name)
		}
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
	if sb.OldProject != "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: project is now called default_project", name)
	}
	if sb.Forge == "azuredevops" && sb.DefaultProject == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: azuredevops needs a default_project", name)
	}
	if sb.Forge == "" || sb.BaseURL == "" || sb.Org == "" || sb.TokenEnv == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: forge, base_url, org and token_env are required", name)
	}
	if u, err := url.Parse(sb.BaseURL); err != nil || u.Scheme == "" || u.Hostname() == "" {
		return Sandbox{}, fmt.Errorf("sandbox %q: base_url %q is not a URL", name, sb.BaseURL)
	}
	return sb, nil
}
