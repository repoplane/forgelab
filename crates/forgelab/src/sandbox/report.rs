//! What a command reports, and the exit code it turns into.

use std::fmt;

use crate::fleet::FleetError;
use crate::forge::ForgeError;
use crate::seed::SeedError;

/// What a command returns when it does not succeed.
#[derive(Debug, Clone)]
pub enum CommandError {
    /// forgelab refused to act: wrong place, missing repository, stale lock. Retrying will not
    /// help and a reset must not be attempted. Exit code 2.
    Guard(String),
    /// The sandbox differs from the lock in ways reset can repair. Exit code 1.
    Drift(String),
    /// Several repositories failed; every one is named. Exit code 2.
    Failed(FailureReport),
    /// Anything else: configuration, a forge answering badly, git. Exit code 2.
    Other(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::Guard(m) | CommandError::Drift(m) | CommandError::Other(m) => {
                f.write_str(m)
            }
            CommandError::Failed(r) => r.fmt(f),
        }
    }
}

impl std::error::Error for CommandError {}

/// One repository that could not be worked on.
#[derive(Debug, Clone)]
pub struct RepoFailure {
    pub name: String,
    pub message: String,
}

/// Every repository that failed, out of how many were worked on. One failure reads exactly as
/// it used to; several are listed, so that a run over a hundred repositories says which ones
/// and why rather than the first it happened to notice.
#[derive(Debug, Clone)]
pub struct FailureReport {
    pub total: usize,
    pub failures: Vec<RepoFailure>,
}

impl fmt::Display for FailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.len() == 1 {
            let x = &self.failures[0];
            return write!(f, "{}: {}", x.name, x.message);
        }
        write!(
            f,
            "{} of {} repositories failed:",
            self.failures.len(),
            self.total
        )?;
        for x in &self.failures {
            write!(f, "\n  {}: {}", x.name, x.message)?;
        }
        Ok(())
    }
}

impl CommandError {
    /// Keeps drift and guard failures apart because they want opposite responses: a caller
    /// resets and retries on 1, and must stop on 2. Collapsing them into "non-zero" would turn
    /// a misconfigured sandbox into an automatic reset loop against it.
    pub fn exit_code(&self) -> i32 {
        match self {
            CommandError::Drift(_) => 1,
            _ => 2,
        }
    }
}

impl From<FleetError> for CommandError {
    fn from(e: FleetError) -> Self {
        CommandError::Other(e.to_string())
    }
}

impl From<ForgeError> for CommandError {
    fn from(e: ForgeError) -> Self {
        match e {
            ForgeError::Cancelled => CommandError::Other("interrupted".into()),
            other => CommandError::Other(other.to_string()),
        }
    }
}

impl From<SeedError> for CommandError {
    fn from(e: SeedError) -> Self {
        CommandError::Other(e.to_string())
    }
}

/// Folds per-repository outcomes into one result. Guard failures win: nothing is retried
/// past one. Otherwise every failure is reported, not the first.
pub fn aggregate<T>(
    results: Vec<(T, Result<(), CommandError>)>,
    name: impl Fn(&T) -> &str,
) -> Result<Vec<T>, CommandError> {
    let total = results.len();
    let mut guards: Vec<String> = Vec::new();
    let mut failures: Vec<RepoFailure> = Vec::new();
    let mut items = Vec::with_capacity(total);
    for (item, r) in results {
        match r {
            Ok(()) => {}
            Err(CommandError::Guard(m)) => guards.push(format!("{}: {m}", name(&item))),
            Err(e) => failures.push(RepoFailure {
                name: name(&item).to_string(),
                message: e.to_string(),
            }),
        }
        items.push(item);
    }
    if !guards.is_empty() {
        return Err(if guards.len() == 1 {
            CommandError::Guard(guards.remove(0))
        } else {
            CommandError::Guard(format!("guard failure:\n  {}", guards.join("\n  ")))
        });
    }
    if !failures.is_empty() {
        failures.sort_by(|a, b| a.name.cmp(&b.name));
        return Err(CommandError::Failed(FailureReport { total, failures }));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_failure_reads_as_before_and_many_are_listed() {
        let one = aggregate(
            vec![
                ("a", Ok(())),
                ("b", Err(CommandError::Other("create: boom".into()))),
            ],
            |n| n,
        )
        .unwrap_err();
        assert_eq!(one.to_string(), "b: create: boom");
        assert_eq!(one.exit_code(), 2);

        let many = aggregate(
            vec![
                ("c", Err(CommandError::Other("x".into()))),
                ("a", Ok(())),
                ("b", Err(CommandError::Other("y".into()))),
            ],
            |n| n,
        )
        .unwrap_err();
        assert_eq!(
            many.to_string(),
            "2 of 3 repositories failed:\n  b: y\n  c: x"
        );

        let guard = aggregate(
            vec![
                ("a", Err(CommandError::Other("x".into()))),
                ("b", Err(CommandError::Guard("not ours".into()))),
            ],
            |n| n,
        )
        .unwrap_err();
        assert!(
            matches!(guard, CommandError::Guard(ref m) if m == "b: not ours"),
            "{guard}"
        );

        assert_eq!(CommandError::Drift("d".into()).exit_code(), 1);
    }
}
