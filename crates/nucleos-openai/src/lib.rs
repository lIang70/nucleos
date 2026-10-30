//! An OpenAI-compatible model, as a [`Model`] adapter — over the **Responses API**.
//!
//! One implementation covers the whole family that speaks `/v1/responses`: OpenAI itself, and the
//! compatible endpoints that followed it. What they disagree about is capability, so capability is
//! configuration rather than a guess: an adapter that assumed tool support and found none would
//! fail somewhere much less obvious than its own constructor.
//!
//! The crate exists because the kernel must not depend on an HTTP client. `nucleos` owns the
//! mechanism; this owns one provider's dialect.

pub mod adapter;
pub mod decode;
pub mod wire;

pub use adapter::{Config, OpenAi};
