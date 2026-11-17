//! Constructors for the shapes a test needs and a kernel test should not have to build by hand.

use nucleos::contract::{Attempt, Content, Part, PartKind, TextPart};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A text content, for tests and for tools that answer in prose.
#[must_use]
pub fn text_content(text: impl Into<String>) -> Content {
    Content::new([Part::new(PartKind::Text(TextPart { text: text.into() }))])
}

/// A ledger holding one successful attempt, for tests.
#[must_use]
pub fn one_attempt(model: &str) -> nucleos::contract::Ledger {
    use nucleos::contract::{AttemptOutcome, AttemptPhase, AttemptTrigger, Usage};
    let mut ledger = nucleos::contract::Ledger::default();
    ledger.record(Attempt {
        index: 0,
        model: model.to_owned(),
        phase: AttemptPhase::Execute,
        trigger: AttemptTrigger::Selection,
        outcome: AttemptOutcome::Ok,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            micros_usd: Some(30),
        },
        duration_millis: 5,
        degraded: Vec::new(),
    });
    ledger
}
