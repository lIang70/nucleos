//! The nucleos daemon, as a library.
//!
//! The binary is a `main` and a listener; everything worth testing — the router, the error body,
//! the stream framing — lives here so it can be exercised without a socket.

pub mod http;

pub use http::{App, router};
