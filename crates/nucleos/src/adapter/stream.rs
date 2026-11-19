//! What a sequence of deltas has to be true of, checked as it arrives.
//!
//! This is the one place that decides whether a delta stream is *shaped* correctly, and both readers
//! use it: the kernel, which turns deltas into events, and [`crate::testing::conformance`], which an
//! adapter runs against itself. Keeping one implementation is the point — a rule enforced only in the
//! test helper is a rule a real adapter breaks in production, and that is the state this file
//! replaces.
//!
//! The invariants are structural, not semantic. Whether the answer is any good is a different
//! question, answered by `ProviderResponseUnusable` and by the caller.

use crate::adapter::{FinishReason, ModelDelta};
use crate::contract::ErrorCode;
use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Watches a delta stream and fails the first structural violation.
///
/// Feed every delta to [`DeltaGuard::accept`] before acting on it. A guard that has seen a violation
/// stays in that state conceptually — the stream is unusable — but the caller is expected to stop at
/// the first error, so no latch is kept.
#[derive(Debug, Default)]
pub struct DeltaGuard {
    /// What occupies each part index, so two different things cannot claim one position.
    parts: BTreeMap<u32, Slot>,
    /// Tool-call ordinals announced so far, for correlating argument fragments.
    started: BTreeSet<u32>,
    finish: Option<FinishReason>,
}

/// What a part index holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A content part: text, reasoning, or a media fragment. Many deltas may share it.
    Content,
    /// A tool call, named by its ordinal. Exactly one call may hold a position.
    ToolCall(u32),
}

impl DeltaGuard {
    /// A guard for one stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check one delta against the stream so far.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::AdapterContractViolation`] when the delta cannot be part of a well-formed stream:
    /// anything after the terminal event, an argument fragment for a call that was never announced,
    /// or two things claiming one part index.
    pub fn accept(&mut self, delta: &ModelDelta) -> Result<()> {
        if self.finish.is_some() {
            return Err(violation(
                "the model produced a delta after its terminal `Finished` event: the terminal event \
                 is terminal, and content after it belongs to no answer",
            ));
        }

        match delta {
            ModelDelta::PartAppend { part_index, .. }
            | ModelDelta::PartReplace { part_index, .. } => {
                if let Some(slot) = self.parts.get(part_index) {
                    if *slot != Slot::Content {
                        return Err(violation(format!(
                            "part {part_index} carries a tool call and a content fragment at once: \
                             a call is a part at its own position, not a second sequence"
                        )));
                    }
                } else {
                    self.parts.insert(*part_index, Slot::Content);
                }
            }
            ModelDelta::ToolCallStarted {
                index, part_index, ..
            } => {
                if !self.started.insert(*index) {
                    return Err(violation(format!(
                        "tool call {index} was announced twice: fragments after the second \
                         announcement cannot be attributed to one call"
                    )));
                }
                match self.parts.insert(*part_index, Slot::ToolCall(*index)) {
                    Some(Slot::Content) => {
                        return Err(violation(format!(
                            "part {part_index} carries a content fragment and tool call {index} at \
                             once: the two would overwrite each other in the assembled message"
                        )));
                    }
                    Some(Slot::ToolCall(other)) => {
                        return Err(violation(format!(
                            "tool calls {other} and {index} claim part {part_index}: the second would \
                             overwrite the first, and one of them would vanish from the answer"
                        )));
                    }
                    None => {}
                }
            }
            ModelDelta::ToolCallArgs { index, .. } => {
                if !self.started.contains(index) {
                    return Err(violation(format!(
                        "argument fragments arrived for tool call {index}, which was never announced: \
                         the assembled call would have no name and would be dropped"
                    )));
                }
            }
            ModelDelta::Finished { reason } => self.finish = Some(reason.clone()),
            _ => {}
        }
        Ok(())
    }

    /// The finish reason, once the stream has said why it stopped.
    #[must_use]
    pub fn finish(&self) -> Option<&FinishReason> {
        self.finish.as_ref()
    }

    /// Whether the terminal event has arrived.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finish.is_some()
    }
}

fn violation(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::AdapterContractViolation, detail)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::contract::Part;

    fn text_delta(index: u32) -> ModelDelta {
        ModelDelta::PartAppend {
            part_index: index,
            part: Part::text("hello"),
        }
    }

    fn tool_start(index: u32, part_index: u32) -> ModelDelta {
        ModelDelta::ToolCallStarted {
            index,
            part_index,
            name: "read_file".to_owned(),
            id: format!("call_{index}"),
        }
    }

    fn tool_args(index: u32) -> ModelDelta {
        ModelDelta::ToolCallArgs {
            index,
            fragment: "{\"path\":".to_owned(),
        }
    }

    #[test]
    fn a_well_formed_stream_is_accepted() {
        let mut guard = DeltaGuard::new();
        guard.accept(&text_delta(0)).unwrap();
        guard.accept(&text_delta(0)).unwrap();
        guard.accept(&tool_start(0, 1)).unwrap();
        guard.accept(&tool_args(0)).unwrap();
        guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap();
        assert!(guard.finished());
        assert_eq!(guard.finish(), Some(&FinishReason::Stop));
    }

    #[test]
    fn anything_after_finished_is_a_violation() {
        let mut guard = DeltaGuard::new();
        guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let error = guard.accept(&text_delta(0)).unwrap_err();
        assert!(error.to_string().contains("after its terminal"));
    }

    #[test]
    fn a_second_finished_is_a_violation() {
        let mut guard = DeltaGuard::new();
        guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let error = guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap_err();
        assert!(error.to_string().contains("after its terminal"));
    }

    #[test]
    fn args_for_an_unannounced_call_are_a_violation() {
        let mut guard = DeltaGuard::new();
        let error = guard.accept(&tool_args(7)).unwrap_err();
        assert!(error.to_string().contains("never announced"));
        assert!(error.to_string().contains('7'));
    }

    #[test]
    fn a_tool_call_announced_twice_is_a_violation() {
        let mut guard = DeltaGuard::new();
        guard.accept(&tool_start(0, 0)).unwrap();
        let error = guard.accept(&tool_start(0, 1)).unwrap_err();
        assert!(error.to_string().contains("announced twice"));
    }

    #[test]
    fn content_and_a_tool_call_cannot_share_a_part_index() {
        let mut guard = DeltaGuard::new();
        guard.accept(&text_delta(0)).unwrap();
        let error = guard.accept(&tool_start(0, 0)).unwrap_err();
        assert!(error.to_string().contains("at once"));
    }

    #[test]
    fn a_tool_call_and_content_cannot_share_a_part_index_the_other_way() {
        let mut guard = DeltaGuard::new();
        guard.accept(&tool_start(0, 0)).unwrap();
        let error = guard.accept(&text_delta(0)).unwrap_err();
        assert!(error.to_string().contains("at once"));
    }

    #[test]
    fn two_tool_calls_cannot_claim_the_same_part_index() {
        let mut guard = DeltaGuard::new();
        guard.accept(&tool_start(0, 0)).unwrap();
        let error = guard.accept(&tool_start(1, 0)).unwrap_err();
        assert!(error.to_string().contains("claim part 0"));
    }

    #[test]
    fn many_content_deltas_may_share_one_part_index() {
        let mut guard = DeltaGuard::new();
        for _ in 0..100 {
            guard.accept(&text_delta(0)).unwrap();
        }
    }

    #[test]
    fn distinct_part_indexes_do_not_interfere() {
        let mut guard = DeltaGuard::new();
        guard.accept(&text_delta(0)).unwrap();
        guard.accept(&tool_start(0, 1)).unwrap();
        guard.accept(&text_delta(2)).unwrap();
        guard.accept(&tool_start(1, 3)).unwrap();
        guard.accept(&tool_args(0)).unwrap();
        guard.accept(&tool_args(1)).unwrap();
        guard
            .accept(&ModelDelta::Finished {
                reason: FinishReason::Stop,
            })
            .unwrap();
    }

    #[test]
    fn a_part_replace_is_treated_as_content() {
        let mut guard = DeltaGuard::new();
        guard
            .accept(&ModelDelta::PartReplace {
                part_index: 0,
                part: Part::text("replaced"),
            })
            .unwrap();
        // A subsequent tool call on the same index is a violation, because the slot is Content.
        let error = guard.accept(&tool_start(0, 0)).unwrap_err();
        assert!(error.to_string().contains("at once"));
    }
}
