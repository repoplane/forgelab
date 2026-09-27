//! A fault-injecting HTTP layer for testing forgelab against a local forge.
//!
//! A local Forgejo answers promptly and honestly. The cloud forges do not: they rate-limit,
//! answer 5xx under load, drop connections, and serve reads from caches that lag a write by
//! seconds. This crate puts those behaviours back, by rule and by seed, in two places:
//!
//! - [`FaultTransport`] wraps any [`forgelab::forge::Transport`] in-process, so the e2e tests
//!   see every API request forgelab makes and can fault it;
//! - [`Proxy`] is a reverse proxy in front of the forge, so git smart-HTTP traffic is faulted
//!   too, exactly as a load balancer between forgelab and a real forge would.
//!
//! Every decision is a pure function of the seed and the request's position in its own
//! path's history, so a failing run is reproduced by its seed alone.

pub mod engine;
pub mod proxy;
pub mod rules;
pub mod transport;

pub use engine::{Action, Decision, FaultEngine, Fired};
pub use proxy::Proxy;
pub use rules::{
    Inject, Match, MissingAs, Rule, Rules, RulesBuilder, Scope, Stale, StaleTrigger, When,
};
pub use transport::{FaultTransport, Shared};
