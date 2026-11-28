//! The client-facing protocol, independent of HTTP or another transport.
//!
//! Session lifecycle envelopes live here because only clients and the runtime need them. Native
//! model calls are **not** wrapped or redefined: inference consumes [`crate::ModelRequest`] and
//! returns [`crate::GenerateResponse`] or [`crate::StreamEvent`].
//!
//! Authentication is transport context, not request data. An [`crate::AppId`] is derived from the
//! caller's credential; no session operation accepts an application id in its body.

mod envelope;
mod session;
mod tool;

pub use envelope::{ApproveEnvelope, RequestEnvelopeChange, RevokeCapabilities};
pub use session::{ConfigureSession, OpenSession, SessionView};
pub use tool::{CallTools, ToolBatchResult, ToolCatalog};
