//! The nucleos daemon, as a library.
//!
//! The binary is a `main` and a listener; everything worth testing — the router, the error body,
//! the stream framing — lives here so it can be exercised without a socket.

pub mod admin;
pub mod auth;
pub mod configure;
pub mod http;
pub mod layout;
pub mod metrics;
pub mod retention;
pub mod serve;
pub mod sources;
pub mod telemetry;

pub use http::{App, router};
pub use layout::Layout;
pub use telemetry::install as install_telemetry;
