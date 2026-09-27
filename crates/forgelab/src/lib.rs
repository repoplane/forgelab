//! forgelab puts a declared set of repositories into a sandbox organisation on a forge, and
//! puts them back.
//!
//! The crate is organised the way the Go original was: `fleet` reads the declaration and the
//! lock, `seed` drives git, `forge` is the small surface forgelab needs from a forge with one
//! client per forge, and `sandbox` implements the commands over them.

pub mod fleet;
pub mod forge;
pub mod sandbox;
pub mod seed;
pub mod util;

/// The version stamped by the release build (`FORGELAB_VERSION=v1.2.3 make build`), else the
/// crate version.
pub const VERSION: &str = match option_env!("FORGELAB_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
