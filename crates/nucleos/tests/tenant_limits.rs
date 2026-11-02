//! What one tenant may use, when the deployment says so.
//!
//! Two limits, both intersections with what the session already has: the tool ceiling and the model
//! allow-list. They live in the kernel rather than in the session because a tightening has to reach
//! sessions that are already running — otherwise an operator's response to an incident would be "wait
//! for the next session", which is no response at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{
    ConfigPatch, Exposure, Principal, Routing, SessionId, ToolPolicy, ToolSpec,
};
use nucleos::kernel::{Kernel, SessionSpec};
use nucleos_sqlite::SqliteStore;
use nucleos_testkit::{FakeModel, FixedClock};
use std::sync::Arc;

struct Inert(&'static str);

#[async_trait::async_trait]
impl Tool for Inert {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.to_owned(),
            description: format!("The {} tool.", self.0),
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
        }
    }
    async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        ToolOutput::text("ok")
    }
}

fn principal(tenant: &str) -> Principal {
    Principal {
        tenant: tenant.to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["ok"])))
        .model("cheap", Arc::new(FakeModel::text(["ok"])))
        .tool(Arc::new(Inert("read_file")))
        .tool(Arc::new(Inert("send_email")))
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
            // Everything visible, so the test is about the tenant limit rather than about exposure.
            tool_policy: Some(ToolPolicy {
                default: Exposure::Always,
                ..Default::default()
            }),
            ..ConfigPatch::default()
        },
        ..SessionSpec::default()
    }
}

fn tool_names(kernel: &Kernel, session: &nucleos::kernel::Session) -> Vec<String> {
    kernel
        .tool_specs(session)
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

#[tokio::test]
async fn a_tenant_ceiling_hides_a_tool_from_that_tenant_only() {
    let (kernel, _store) = kernel();
    let mut scoped = kernel
        .open(&principal("acme"), SessionId::new("a1"), spec())
        .await
        .unwrap();
    let other = kernel
        .open(&principal("other"), SessionId::new("b1"), spec())
        .await
        .unwrap();

    kernel
        .set_tenant_tools("acme", Some(vec!["read_file".to_owned()]))
        .unwrap();

    // Read again: the ceiling is consulted when the prompt is built, not when the session opens.
    assert_eq!(tool_names(&kernel, &scoped), ["read_file"]);
    assert_eq!(
        tool_names(&kernel, &other),
        ["read_file", "send_email"],
        "another tenant is untouched"
    );

    // And the engine refuses the call too — hiding a tool is not the same as forbidding it, and the
    // ceiling is a permission, so both have to agree.
    let output = kernel
        .invoke_tool(
            &mut scoped,
            &nucleos::contract::ToolCall {
                id: "call-1".to_owned(),
                name: "send_email".to_owned(),
                arguments_json: "{}".to_owned(),
                meta: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(output.is_error, "{output:?}");
}

#[tokio::test]
async fn an_empty_tenant_ceiling_allows_nothing_and_clearing_it_restores() {
    let (kernel, _store) = kernel();
    let session = kernel
        .open(&principal("acme"), SessionId::new("a1"), spec())
        .await
        .unwrap();

    kernel.set_tenant_tools("acme", Some(vec![])).unwrap();
    assert!(
        tool_names(&kernel, &session).is_empty(),
        "an explicit empty list means nothing"
    );

    kernel.set_tenant_tools("acme", None).unwrap();
    assert_eq!(tool_names(&kernel, &session).len(), 2, "clearing restores");
}

#[tokio::test]
async fn a_tenant_model_list_filters_a_route_rather_than_killing_it() {
    let (kernel, _store) = kernel();
    kernel
        .set_tenant_models("acme", Some(vec!["cheap".to_owned()]))
        .unwrap();

    // A chain naming a forbidden primary and an allowed fallback still runs: it is filtered, which is
    // what a deployment with per-tenant access expects from a chain that has a fallback.
    let session = kernel
        .open(
            &principal("acme"),
            SessionId::new("a1"),
            SessionSpec {
                config: ConfigPatch {
                    routing: Some(Routing {
                        chain: vec!["primary".to_owned(), "cheap".to_owned()],
                        retries: 0,
                    }),
                    ..spec().config
                },
                ..spec()
            },
        )
        .await
        .unwrap();
    kernel.inference(&mut { session }, &[]).await.unwrap();
}

#[tokio::test]
async fn a_tenant_model_list_that_leaves_nothing_refuses_the_route() {
    let (kernel, _store) = kernel();
    kernel
        .set_tenant_models("acme", Some(vec!["a-model-it-does-not-have".to_owned()]))
        .unwrap();

    let refused = kernel
        .open(&principal("acme"), SessionId::new("a1"), spec())
        .await;
    let error = refused.expect_err("no model in the route survives the filter");
    assert!(error.reason().contains("acme"), "{error:?}");
}
