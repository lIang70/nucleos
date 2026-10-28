//! The nucleos daemon, as a library.
//!
//! The binary is a `main` and a listener; everything worth testing — the translation between the
//! kernel and the wire, and the service's behaviour when no deployment is configured — lives
//! here so it can be tested without a socket.

pub mod service;
pub mod translate;

pub use service::KernelService;
