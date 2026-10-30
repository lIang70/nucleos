//! A real policy: a document, checked deny-first, that fails closed.
//!
//! The kernel asks one question — may this run? — and this crate answers it from a file the
//! deployment ships. Four properties are the whole design:
//!
//! - **Deny by default.** A rule that matches nothing is the normal case; the default decides what
//!   happens then, and it is `deny` unless a document says otherwise in so many words.
//! - **Deny is evaluated before allow, and before ask.** The order is by decision, not by file
//!   order, so moving a line cannot widen access. A deployment cannot reorder its way into an
//!   outage or into an escalation.
//! - **A rule must name something.** A rule with no `tool`, `model` or `program` matches not at
//!   all — a rule that matches everything by omission is how a policy silently becomes an
//!   allow-list of one entry.
//! - **The program is matched as a fact, not as a sentence.** `argv` is not a shell: the kernel
//!   spawns it directly, so `program` is compared against the basename of `argv[0]` and
//!   `argv_prefix` against the following arguments. No command-line parsing, so no parser to
//!   disagree with the one that runs the command.
//!
//! What is deliberately absent: an expression language, rule priorities, time windows, quotas.
//! Each of those turns a document a human can read into a program, and the kernel already has a
//! place for programs — the host that implements [`nucleos::adapter::Policy`].

mod document;
mod ruleset;

pub use document::{Decision, Document, Rule, Unmatched};
pub use ruleset::Ruleset;
