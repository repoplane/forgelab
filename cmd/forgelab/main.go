// Command forgelab puts a declared set of repositories into a sandbox organisation on a
// forge, and puts them back.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"

	"github.com/repoplane/forgelab/internal/sandbox"
)

// version is stamped by the release build (-ldflags "-X main.version=v1.2.3").
var version = "dev"

const usage = `forgelab — deterministic repository fleets on a sandbox org

Usage:
  forgelab <command> --sandbox <name> [flags]

Commands:
  plan      show what apply would create (+) or update (~); no writes
  apply     create, seed and configure the declared repositories; write fleet.lock.json
  verify    assert the sandbox matches the lock    exit 0 ok · 1 drift · 2 guard failure
  reset     put drifted repositories back to baseline; never creates or deletes one
  destroy   delete the declared repositories, and nothing else
  version   print the version

Flags:
`

func main() { os.Exit(run(os.Args[1:])) }

func run(args []string) int {
	var o sandbox.Options
	fs := flag.NewFlagSet("forgelab", flag.ContinueOnError)
	fs.StringVar(&o.Sandbox, "sandbox", "", "sandbox name from the config (required)")
	fs.StringVar(&o.FleetDir, "fleet", ".", "fleet directory: fleet.yaml, repos/, fleet.lock.json")
	fs.StringVar(&o.ConfigPath, "config", "", "sandbox config (default <fleet>/sandboxes.yaml)")
	fs.BoolVar(&o.Yes, "yes", false, "skip the destroy confirmation")
	fs.BoolVar(&o.Verbose, "v", false, "verbose output")
	fs.Usage = func() {
		fmt.Fprint(os.Stderr, usage)
		fs.PrintDefaults()
	}

	if len(args) == 0 || args[0] == "-h" || args[0] == "--help" || args[0] == "help" {
		fs.Usage()
		return 2
	}
	command := args[0]
	if command == "version" || command == "--version" {
		fmt.Println("forgelab", version)
		return 0
	}
	if err := fs.Parse(args[1:]); err != nil {
		return 2
	}
	if o.Sandbox == "" {
		fmt.Fprintln(os.Stderr, "forgelab: --sandbox is required")
		return 2
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()

	env, err := sandbox.Open(o)
	if err == nil {
		switch command {
		case "plan":
			err = env.Plan(ctx)
		case "apply":
			err = env.Apply(ctx)
		case "verify":
			err = env.Verify(ctx)
		case "reset":
			err = env.Reset(ctx)
		case "destroy":
			err = env.Destroy(ctx)
		default:
			fmt.Fprintf(os.Stderr, "forgelab: unknown command %q\n\n", command)
			fs.Usage()
			return 2
		}
	}
	return exitCode(err)
}

// exitCode keeps drift and guard failures apart because they want opposite responses: a
// caller resets and retries on 1, and must stop on 2. Collapsing them into "non-zero"
// would turn a misconfigured sandbox into an automatic reset loop against it.
func exitCode(err error) int {
	if err == nil {
		return 0
	}
	fmt.Fprintln(os.Stderr, "forgelab:", err)
	var drift *sandbox.DriftError
	if errors.As(err, &drift) {
		return 1
	}
	return 2
}
