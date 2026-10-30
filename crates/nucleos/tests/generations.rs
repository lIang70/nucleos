//! Generations: a session resolves tools through the set it was pinned to.
//!
//! The invariant is one sentence long — *a call is answered by the set that produced the prompt* —
//! and everything here is that sentence made observable: publishing does not move a running session,
//! migration happens at a turn boundary, and it happens only when the new set answers every call the
//! old one did.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{
    ConfigPatch, EventKind, Principal, Routing, SessionId, StopReason, ToolSpec,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

/// The store's key for a test tenant. The tests all use one tenant, so the helper keeps them about
/// what they are testing rather than about spelling out a key at every call.
fn key(id: &SessionId) -> nucleos::adapter::SessionKey {
    nucleos::adapter::SessionKey::new("acme", id.clone())
}


fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

struct Inert {
    name: String,
    schema: String,
    answer: String,
}

#[async_trait]
impl Tool for Inert {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: format!("The {} tool.", self.name),
            input_schema_json: self.schema.clone(),
        }
    }
    async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        ToolOutput::text(self.answer.clone())
    }
}

fn tool(name: &str, schema: &str, answer: &str) -> Arc<dyn Tool> {
    Arc::new(Inert {
        name: name.to_owned(),
        schema: schema.to_owned(),
        answer: answer.to_owned(),
    })
}

const TAKE_PATH: &str =
    r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;
const TAKE_PATH_AND_OFFSET: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"}},"required":["path"]}"#;

fn build() -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .tool(tool("read_file", TAKE_PATH, "from generation 1"))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap();
    (kernel, store)
}

fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: 1_000_000,
        config: ConfigPatch {
            // Every tool visible, so the tests are about generations and not about discovery.
            tool_policy: Some(nucleos::contract::ToolPolicy {
                default: nucleos::contract::Exposure::Always,
                ..Default::default()
            }),
            ..ConfigPatch::default()
        },
        ..SessionSpec::default()
    }
}

fn call(name: &str) -> nucleos::contract::ToolCall {
    nucleos::contract::ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments_json: "{}".to_owned(),
        meta: Default::default(),
    }
}

#[tokio::test]
async fn publishing_does_not_move_a_running_session() {
    let (kernel, _store) = build();
    let first = kernel.generation();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    assert_eq!(session.generation(), first);

    // A new set with an extra tool. The session is still on the old one.
    let second = kernel
        .publish(vec![
            tool("read_file", TAKE_PATH, "from generation 2"),
            tool("write_file", "{}", "only in 2"),
        ])
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(session.generation(), first, "publishing is not migrating");

    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(!names.contains(&"write_file".to_owned()), "{names:?}");

    // And the call it makes is answered by the generation that built its prompt.
    let output = kernel
        .invoke_tool(&mut session, &call("read_file"))
        .await
        .unwrap();
    assert_eq!(output.content.text(), "from generation 1");

    // A session opened now gets the new one.
    let fresh = kernel
        .open(&principal(), SessionId::new("s2"), spec())
        .await
        .unwrap();
    assert_eq!(fresh.generation(), second);
}

#[tokio::test]
async fn a_compatible_change_migrates_at_the_next_turn_and_is_recorded() {
    let (kernel, store) = build();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    let first = session.generation();

    // The same tool, with one more optional property: a request the old schema accepted is still
    // accepted.
    let second = kernel
        .publish(vec![tool("read_file", TAKE_PATH_AND_OFFSET, "from generation 2")])
        .unwrap();
    assert_ne!(first, second);

    // A turn boundary is where it moves: `inference` is the boundary.
    kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(session.generation(), second);

    let migrated = store
        .events_since(&key(session.id()), 0)
        .await
        .unwrap()
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::GenerationMigrated { from, to } => Some((*from, *to)),
            _ => None,
        })
        .expect("the migration is in the log");
    assert_eq!(migrated, (first, second));

    // And the tool that answers is now the new one.
    let output = kernel
        .invoke_tool(&mut session, &call("read_file"))
        .await
        .unwrap();
    assert_eq!(output.content.text(), "from generation 2");
}

#[tokio::test]
async fn an_incompatible_change_leaves_the_session_where_it_is() {
    let (kernel, store) = build();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    let first = session.generation();

    // The tool was withdrawn. A prompt built from the old set may still name it.
    kernel
        .publish(vec![tool("write_file", "{}", "only in 2")])
        .unwrap();

    kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(
        session.generation(), first,
        "incompatible means pinned, not migrated"
    );
    assert!(
        !store
            .events_since(&key(session.id()), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, EventKind::GenerationMigrated { .. })),
        "no migration is recorded for a migration that did not happen"
    );

    // The tool the session was promised still answers.
    let output = kernel
        .invoke_tool(&mut session, &call("read_file"))
        .await
        .unwrap();
    assert_eq!(output.content.text(), "from generation 1");
}

#[tokio::test]
async fn a_tool_withdrawn_from_the_current_generation_still_answers_a_pinned_session() {
    let (kernel, _store) = build();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    kernel.publish(vec![tool("other", "{}", "not read_file")]).unwrap();

    // Withdrawn from the newest set, still in the one this session is pinned to — which is the case
    // that makes pinning worth having: a tool source redeployed mid-session does not break a model
    // that is halfway through using it.
    let output = kernel
        .invoke_tool(&mut session, &call("read_file"))
        .await
        .unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(output.content.text(), "from generation 1");

    // A session that starts now cannot see it at all.
    let fresh = kernel
        .open(&principal(), SessionId::new("s2"), spec())
        .await
        .unwrap();
    assert!(kernel.tool_in(&fresh, "read_file").is_none());
}

#[tokio::test]
async fn a_session_that_never_migrates_keeps_working_until_it_ends() {
    let (kernel, _store) = build();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();

    // Three incompatible publishes in a row: the session stays on its own set the whole time.
    for round in 0..3 {
        kernel
            .publish(vec![tool(
                &format!("tool_{round}"),
                "{}",
                "somewhere else",
            )])
            .unwrap();
        kernel.inference(&mut session, &[]).await.unwrap();
    }

    assert_eq!(session.generation().0, 1);
    kernel
        .invoke_tool(&mut session, &call("read_file"))
        .await
        .unwrap();

    // Closing works, and the stop reason is the ordinary one.
    kernel.close(&mut session, StopReason::Completed).await.unwrap();
    assert!(session.is_closed());
}
