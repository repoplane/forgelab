//! A fleet declaration and the lock that records what it resolves to. Knows nothing about
//! forges.

pub mod digest;
pub mod gojson;
pub mod lock;
pub mod spec;

pub use digest::{digest, is_os_junk};
pub use lock::{LOCK_FILE, Lock, LockRepo, parse_lock, read_lock};
pub use spec::{
    Author, GitIdentity, REPOS_DIR, Repo, SPEC_FILE, SPEC_VERSION, Spec, VISIBILITY_PRIVATE,
    VISIBILITY_PUBLIC, load_spec,
};

/// What can go wrong reading a fleet: an invalid declaration, or a file that cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl FleetError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        FleetError::Invalid(msg.into())
    }

    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        FleetError::Io { context: context.into(), source }
    }

    /// True when the underlying file does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, FleetError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}
