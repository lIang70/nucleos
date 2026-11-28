//! Who owns a session.
//!
//! Identity has exactly two coordinates: the application credential and the caller-chosen
//! session name. There is deliberately no tenant or principal layer here. An organisation may
//! be an accounting attribute of an application; it is not part of a session's identity.

mod id;
#[cfg(test)]
mod tests;

pub use id::{AppId, MAX_NAME_LEN, SessionId, SessionKey};
