//! Retiring generations.
//!
//! A generation is retired by forgetting its number, and that is safe because a session *holds* the set
//! its calls are answered from. These tests pin the two halves of that claim: the number is really gone,
//! and a session pinned to it still works — including the case that would be a disaster the other way
//! round, a retired set being silently swapped for the current one under an incompatible session.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use nucleos::adapter::{AllowAll, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{Principal, Routing, SessionId, ToolSpec};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

struct Named {
    name: &'static str,
    schema: &'static str,
}

#[async_trait]
impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_owned(),
            description: format!("{} tool", self.name),
            input_schema_json: self.schema.to_owned(),
        }
    }
    async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        ToolOutput::text(self.name)
    }
}

fn tool(name: &'static str, schema: &'static str) -> Arc<dyn Tool> {
    Arc::new(Named { name, schema })
}

fn kernel() -> Kernel {
    Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok"])))
        .store(Arc::new(SqliteStore::in_memory().unwrap()) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .tool(tool("read_file", r#"{"type":"object","properties":{"path":{}}}"#))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .build()
        .unwrap()
}

/// Enough credit that a test about generations never fails about budgets.
fn spec() -> SessionSpec {
    SessionSpec {
        credit_tokens: 1_000_000,
        ..SessionSpec::default()
    }
}

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

#[tokio::test]
async fn a_session_pinned_to_a_retired_generation_keeps_its_tools() {
    let kernel = kernel();
    // s1 pins generation 1, which has `read_file` and nothing else.
    let mut pinned = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    assert_eq!(pinned.generation(), nucleos::contract::Generation(1));

    // Generation 2 removes `read_file` and adds `write_file`: incompatible by design.
    kernel
        .publish(vec![tool(
            "write_file",
            r#"{"type":"object","properties":{"path":{}}}"#,
        )])
        .unwrap();

    let retired = kernel.retire().unwrap();
    assert_eq!(retired, vec![nucleos::contract::Generation(1)]);
    assert_eq!(kernel.generation().0, 2);

    // The session is still answered by the set it was pinned to, even though the kernel no longer has
    // an entry for that generation: the session holds it.
    let found = kernel
        .tool_in(&pinned, "read_file")
        .expect("a retired generation still answers its own session");
    assert_eq!(found.spec().name, "read_file");
    assert!(
        kernel.tool_in(&pinned, "write_file").is_none(),
        "and it does not gain the new set's tools"
    );

    // The incompatible migration is still refused, and refused for the right reason: the pinned side is
    // the session's own set, not a number that resolves to the current set once the number is gone.
    let before = pinned.generation();
    assert!(kernel.inference(&mut pinned, &[]).await.is_ok());
    assert_eq!(pinned.generation(), before, "no migration onto an incompatible set");

    // A session opened now is on generation 2 and sees the opposite.
    let fresh = kernel
        .open(&principal(), SessionId::new("s2"), SessionSpec::default())
        .await
        .unwrap();
    assert!(kernel.tool_in(&fresh, "read_file").is_none());
    assert!(kernel.tool_in(&fresh, "write_file").is_some());
}

#[tokio::test]
async fn retiring_twice_forgets_nothing_the_second_time() {
    let kernel = kernel();
    kernel.publish(vec![tool("a", "{}")]).unwrap();
    assert_eq!(kernel.retire().unwrap().len(), 1);
    // Nothing left but the current one: a second call is not an error, it is a no-op that says so.
    assert!(kernel.retire().unwrap().is_empty());
    assert_eq!(kernel.generation().0, 2);
}

#[tokio::test]
async fn a_kernel_with_one_generation_has_nothing_to_retire() {
    let kernel = kernel();
    assert!(kernel.retire().unwrap().is_empty());
    assert_eq!(kernel.generation().0, 1);
}

#[tokio::test]
async fn a_compatible_session_still_migrates_after_the_old_set_is_retired() {
    // Retirement must not strand a session that *could* move: the current set is never retired, so the
    // target of every migration is always there.
    let kernel = kernel();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec())
        .await
        .unwrap();
    kernel
        .publish(vec![tool(
            "read_file",
            r#"{"type":"object","properties":{"path":{},"offset":{}}}"#,
        )])
        .unwrap();
    kernel.retire().unwrap();

    kernel.inference(&mut session, &[]).await.unwrap();
    assert_eq!(
        session.generation(),
        nucleos::contract::Generation(2),
        "an optional property is compatible, and migration still happens"
    );
    // And it kept a tool to call: the migration moved it onto the current set, which was never retired.
    assert!(kernel.tool_in(&session, "read_file").is_some());
}
