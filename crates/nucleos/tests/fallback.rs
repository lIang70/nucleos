//! What happens when the first model in a route cannot take the request.
//!
//! `docs/architecture.md` §11.1 promises that a prompt too large for one model still reaches the next
//! one. A preflight refusal used to leave `invoke_once` as an `Err`, which aborted the whole walk: no
//! fallback, no attempt in the ledger, and a caller left to discover the chain never mattered.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{
    AllowAll, FinishReason, Model, ModelDelta, ModelRequest, ModelStream, Store,
};
use nucleos::contract::{Capability, Principal, Routing, SessionId};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::FixedClock;
use std::sync::Arc;

/// A model with a window of `window` tokens that answers "ok" when it is asked at all.
struct Small {
    window: u32,
    calls: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl Model for Small {
    fn capability(&self) -> Capability {
        Capability {
            tools: true,
            tool_choice_required: true,
            structured_output: true,
            streaming: true,
            context_window: self.window,
            max_output_tokens: 4_096,
        }
    }
    fn count_tokens(&self, request: &ModelRequest) -> nucleos::error::Result<u64> {
        Ok(request.text_len().div_ceil(4).max(1))
    }
    async fn invoke(&self, _request: ModelRequest) -> nucleos::error::Result<ModelStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let deltas = vec![
            Ok(ModelDelta::Text {
                part_index: 0,
                text: "ok".to_owned(),
            }),
            Ok(ModelDelta::Finished {
                reason: FinishReason::Stop,
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(deltas)))
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

#[tokio::test]
async fn a_prompt_that_cannot_fit_the_first_model_reaches_the_second() {
    let narrow = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let wide = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model(
            "narrow",
            Arc::new(Small {
                // Small enough that a sentence cannot fit; the prompt below is ~50 tokens.
                window: 10,
                calls: Arc::clone(&narrow),
            }),
        )
        .model(
            "wide",
            Arc::new(Small {
                window: 128_000,
                calls: Arc::clone(&wide),
            }),
        )
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["narrow".to_owned(), "wide".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();

    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                credit_tokens: Some(1_000_000),
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();
    let message = nucleos::contract::Message {
        role: nucleos::contract::Role::User,
        content: nucleos::contract::Content::new([nucleos::contract::Part::new(
            nucleos::contract::PartKind::Text(nucleos::contract::TextPart {
                text: "a sentence long enough that ten tokens cannot hold it, repeated once"
                    .to_owned(),
            }),
        )]),
    };
    let outcome = kernel.inference(&mut session, &[message]).await.unwrap();
    assert_eq!(outcome.message.content.parts.len(), 1);

    assert_eq!(
        narrow.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the narrow model was never called: the refusal happens before the call"
    );
    assert_eq!(
        wide.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the fallback answered"
    );

    // And the refusal is in the ledger as a preflight failure, not as silence.
    let first = session.ledger().attempts.first().expect("an attempt");
    assert_eq!(
        first.phase,
        nucleos::contract::AttemptPhase::Preflight,
        "{:?}",
        session.ledger().attempts
    );
}
