//! The driver.
//!
//! The engine is the only part of the kernel that is not a data type. It owns the turn
//! loop, the stop conditions, credit debiting and event emission — and it owns nothing
//! else. Models, tools, storage and policy arrive through [`crate::adapter`].
//!
//! # Invariants
//!
//! Three of them, and they are not negotiable:
//!
//! 1. **Checkpoints are written only at a turn boundary.** Nothing half-finished is ever
//!    persisted, which is what makes recovery a small problem instead of a research
//!    problem.
//! 2. **Exactly one writer per session.** A session is driven by one task and never hands
//!    out `&mut` state. No locks on the hot path, and "settled" means "at a boundary".
//! 3. **A tool call resolves against the generation that issued it.** Tool sets change at
//!    boundaries, never inside a turn.
//!
//! # The loop
//!
//! ```text
//! Observe  build the prompt: system + history + tool results
//! Think    model.invoke → stream deltas
//! Act      policy.check → tool.invoke   (concurrent within a turn, folded back in order)
//! Check    extend the ledger, test the four stop conditions
//! Repeat   advance state, or leave the loop and checkpoint
//! ```
//!
//! State transitions are serial; tool execution is parallel. Those are different axes and
//! must not be collapsed.

// The engine lands in P1. The module exists so the layering test has something to pin and
// so the boundary is visible in the tree rather than only in a document.
