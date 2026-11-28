//! What a command may touch, and which backend can actually enforce it.
//!
//! Four **orthogonal** dimensions rather than one ladder: files, network,
//! environment, resources. A single ordered enum ("read only" → "workspace
//! write" → "full access") reads well in a table and then cannot express
//! "writes under `/srv`, no network, and only these four environment variables" —
//! which is the shape every real deployment asks for.
//!
//! The second half of this module is [`Enforcement`]: what a backend can
//! **actually enforce**, declared rather than discovered, because a policy a
//! backend cannot keep must be an error where an operator sees it and never a
//! silent downgrade.

mod files;
pub use files::Files;
mod net;
pub use net::{Mitm, NetAction, NetMode, NetPolicy, NetRule};
mod env;
pub use env::EnvPolicy;
mod exec;
pub use exec::{ExecOptions, ExecPolicy, Resources, SessionSpec};
mod confine;
pub use confine::Confine;
mod enforce;
pub use enforce::{BackendCapabilities, Enforcement, SessionFeatures};
mod exit;
pub use exit::{ExitReason, SessionExit};
#[cfg(test)]
mod tests;
