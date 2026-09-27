//! Command forgelab puts a declared set of repositories into a sandbox organisation on a
//! forge, and puts them back.

use std::io::Write as _;

use forgelab::sandbox::{self, CommandError, Env, Options};

use forgelab::completion;

const USAGE: &str = "forgelab — deterministic repository fleets on a sandbox org

Usage:
  forgelab <command> --sandbox <name> [flags]

Commands:
  plan      show what apply would create (+) or update (~); no writes
  apply     create, seed and configure the declared repositories; write fleet.lock.json
  verify    assert the sandbox matches the lock    exit 0 ok · 1 drift · 2 guard failure
  reset     put drifted repositories back to baseline; never creates or deletes one
  destroy   delete the declared repositories, and nothing else
  version   print the version
  completion bash|zsh   print a shell completion script:  eval \"$(forgelab completion zsh)\"

Flags:
  -concurrency int
    \trepositories worked on at once (default: the forge's own)
  -config string
    \tsandbox config (default <fleet>/sandboxes.yaml)
  -fleet string
    \tfleet directory: fleet.yaml, repos/, fleet.lock.json (default \".\")
  -sandbox string
    \tsandbox name from the config (required)
  -v\tverbose output
  -yes
    \tskip the destroy confirmation
";

const COMMANDS: &[&str] = &["plan", "apply", "verify", "reset", "destroy"];

fn usage() {
    let _ = std::io::stderr().write_all(USAGE.as_bytes());
}

/// The flags, parsed the way Go's `flag` package did: `-name value`, `--name value`,
/// `-name=value`, booleans with or without `=true`; parsing stops at the first argument that
/// is not a flag.
#[derive(Default, Debug)]
struct Flags {
    sandbox: String,
    fleet: String,
    config: String,
    yes: bool,
    verbose: bool,
    concurrency: Option<usize>,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags {
        fleet: ".".into(),
        ..Flags::default()
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break; // Go stops at the first non-flag; the rest is positional and ignored
        }
        let body = arg.trim_start_matches('-');
        if body.is_empty() {
            return Err(format!("bad flag syntax: {arg}"));
        }
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (body, None),
        };
        let value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("flag needs an argument: -{name}"))
        };
        match name {
            "sandbox" => f.sandbox = value(&mut i)?,
            "fleet" => f.fleet = value(&mut i)?,
            "config" => f.config = value(&mut i)?,
            "concurrency" => {
                let v = value(&mut i)?;
                f.concurrency =
                    Some(v.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
                        format!(
                            "invalid value {v:?} for flag -concurrency: want a positive integer"
                        )
                    })?);
            }
            "yes" | "v" => {
                let on = match &inline {
                    None => true,
                    Some(v) => v
                        .parse::<bool>()
                        .map_err(|_| format!("invalid boolean value {v:?} for -{name}"))?,
                };
                if name == "yes" {
                    f.yes = on
                } else {
                    f.verbose = on
                }
            }
            "h" | "help" => return Err("help".into()),
            other => return Err(format!("flag provided but not defined: -{other}")),
        }
        i += 1;
    }
    Ok(f)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        return 2;
    };
    match command {
        "-h" | "--help" | "help" => {
            usage();
            return 2;
        }
        "version" | "--version" => {
            println!("forgelab {}", forgelab::VERSION);
            return 0;
        }
        "completion" => {
            return completion::completion(args.get(1).map(String::as_str).unwrap_or(""));
        }
        _ => {}
    }
    let flags = match parse_flags(&args[1..]) {
        Ok(f) => f,
        Err(msg) => {
            if msg != "help" {
                eprintln!("{msg}");
            }
            usage();
            return 2;
        }
    };
    // Hidden: the sandbox names in the config, one per line, for shell completion. It needs no
    // token and says nothing on error -- a completion that prints errors is worse than none.
    if command == "__sandboxes" {
        for name in sandbox::names(&flags.fleet, &flags.config) {
            println!("{name}");
        }
        return 0;
    }
    // The command is checked before anything is opened, so that a typo is reported as a typo
    // rather than as whatever opening the sandbox happened to trip on first.
    if !COMMANDS.contains(&command) {
        eprintln!("forgelab: unknown command {command:?}\n");
        usage();
        return 2;
    }
    if flags.sandbox.is_empty() {
        eprintln!("forgelab: --sandbox is required");
        return 2;
    }
    if flags.verbose {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "forgelab=debug".parse().unwrap()),
            )
            .with_writer(std::io::stderr)
            .with_target(false)
            .without_time()
            .try_init();
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("forgelab: {e}");
            return 2;
        }
    };
    runtime.block_on(async move {
        let cancel = tokio_util::sync::CancellationToken::new();
        spawn_signal_handler(cancel.clone());
        let opened = Env::open(Options {
            fleet_dir: flags.fleet,
            config_path: flags.config,
            sandbox: flags.sandbox,
            yes: flags.yes,
            verbose: flags.verbose,
            concurrency: flags.concurrency,
            cancel: Some(cancel.clone()),
            ..Options::default()
        });
        let result = match opened {
            Err(e) => Err(e),
            Ok(env) => {
                if let Err(e) = forgelab::seed::check_git().await {
                    Err(CommandError::Other(e.to_string()))
                } else {
                    match command {
                        "plan" => env.plan().await,
                        "apply" => env.apply().await,
                        "verify" => env.verify().await,
                        "reset" => env.reset().await,
                        "destroy" => env.destroy().await,
                        _ => unreachable!(),
                    }
                }
            }
        };
        exit_code(result)
    })
}

/// Ctrl-C and SIGTERM cancel the run: every wait and every git subprocess stops, scratch
/// directories are removed, and the command reports "interrupted".
fn spawn_signal_handler(cancel: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => return,
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}

/// Keeps drift and guard failures apart because they want opposite responses: a caller
/// resets and retries on 1, and must stop on 2. Collapsing them into "non-zero" would turn a
/// misconfigured sandbox into an automatic reset loop against it.
fn exit_code(result: Result<(), CommandError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("forgelab: {e}");
            e.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_parse_like_go() {
        let f = parse_flags(&args(&[
            "-sandbox",
            "gh",
            "--fleet=shapes",
            "--yes",
            "-v",
            "-concurrency",
            "3",
            "extra",
        ]))
        .unwrap();
        assert_eq!(
            (
                f.sandbox.as_str(),
                f.fleet.as_str(),
                f.yes,
                f.verbose,
                f.concurrency
            ),
            ("gh", "shapes", true, true, Some(3))
        );
        assert!(
            parse_flags(&args(&["--nope"]))
                .unwrap_err()
                .contains("not defined")
        );
        assert!(
            parse_flags(&args(&["--sandbox"]))
                .unwrap_err()
                .contains("needs an argument")
        );
        assert!(parse_flags(&args(&["--concurrency", "0"])).is_err());
        assert_eq!(parse_flags(&args(&["-h"])).unwrap_err(), "help");
    }

    #[test]
    fn usage_errors_exit_two() {
        for a in [
            vec![],
            vec!["verify"],
            vec!["verify", "--nope"],
            vec!["nope", "--sandbox", "x"],
            vec!["verify", "-h"],
        ] {
            assert_eq!(run(&args(&a)), 2, "{a:?}");
        }
    }

    #[test]
    fn version_needs_no_sandbox() {
        for a in ["version", "--version"] {
            assert_eq!(run(&args(&[a])), 0);
        }
    }

    #[test]
    fn completion_command() {
        assert_eq!(run(&args(&["completion", "bash"])), 0);
        assert_eq!(run(&args(&["completion", "fish"])), 2);
        assert_eq!(
            run(&args(&["__sandboxes", "--fleet", "/nonexistent"])),
            0,
            "a missing config must stay quiet and succeed"
        );
    }
}
