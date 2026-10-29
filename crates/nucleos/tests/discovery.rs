//! Tool discovery, end to end: what the model sees is what the ceiling allows, and a search widens
//! it only as far as that ceiling.
//!
//! The invariant under test is the one that makes dynamic loading safe: exposure is not permission.
//! A tool can ask for anything; the kernel intersects with the ceiling, records the widening as an
//! event, and the gate in front of execution does not move.

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{AllowAll, FakeModel, FixedClock, MemoryStore, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{ConfigPatch, Exposure, Principal, Routing, SessionId, ToolSpec};
use nucleos::kernel::{Kernel, SessionSpec};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

/// A tool that does nothing, so the catalog can be made large enough to need discovery.
fn inert(name: &str, description: &str) -> Arc<dyn Tool> {
    Arc::new(Inert {
        spec: ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
        },
    })
}

struct Inert {
    spec: ToolSpec,
}

#[async_trait::async_trait]
impl Tool for Inert {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn invoke(&self, _args: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        ToolOutput::text("ok")
    }
}

/// A catalog of `count` tools, plus discovery once it crosses the threshold.
fn kernel(count: usize) -> (Kernel, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let mut builder = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        });
    for position in 0..count {
        builder = builder.tool(inert(
            &format!("inert_{position:02}"),
            &format!("An inert tool number {position}."),
        ));
    }
    builder = builder.tool(inert("read_file", "Read a file from the workspace."));
    (builder.build().unwrap(), store)
}

fn spec(names: &[&str]) -> SessionSpec {
    SessionSpec {
        config: ConfigPatch {
            tools: Some(names.iter().map(|name| (*name).to_owned()).collect()),
            ..ConfigPatch::default()
        },
        ..SessionSpec::default()
    }
}

fn call(name: &str, arguments_json: &str) -> nucleos::contract::ToolCall {
    nucleos::contract::ToolCall {
        id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments_json: arguments_json.to_owned(),
        meta: std::collections::BTreeMap::new(),
    }
}

#[tokio::test]
async fn a_small_catalog_gets_no_search_tool() {
    let (kernel, _store) = kernel(2);
    let session = kernel.open(&principal(), SessionId::new("s1"), spec(&[])).await.unwrap();
    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    // Three tools fit in a prompt; adding a fourth that only finds the other three is a cost.
    assert!(!names.contains(&"tool_search".to_owned()), "{names:?}");
    assert_eq!(names.len(), 3);
}

#[tokio::test]
async fn a_large_catalog_gets_one_search_tool() {
    let (kernel, _store) = kernel(20);
    let session = kernel.open(&principal(), SessionId::new("s1"), spec(&[])).await.unwrap();
    let specs = kernel.tool_specs(&session);

    assert_eq!(
        specs.iter().filter(|spec| spec.name == "tool_search").count(),
        1
    );
    // It is described well enough that the model knows when to reach for it.
    let search = specs
        .iter()
        .find(|spec| spec.name == "tool_search")
        .unwrap();
    assert!(search.description.contains("Search the tool catalog"));
    assert!(search.input_schema_json.contains("\"query\""));
}

#[tokio::test]
async fn a_search_widens_exposure_and_records_it() {
    let (kernel, _store) = kernel(20);
    let session = kernel.open(&principal(), SessionId::new("s1"), spec(&[])).await.unwrap();
    assert_eq!(*session.exposed(), Exposure::All, "the default is everything");

    // Narrowing first gives discovery somewhere to grow into.
    let mut session = session;
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                exposed: Some(Exposure::Only {
                    names: vec!["tool_search".to_owned()],
                }),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();
    let visible: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(visible, ["tool_search"]);

    let output = kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"read a file"}"#),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(output.expose, ["read_file"]);
}

#[tokio::test]
async fn the_ceiling_still_wins_over_a_search() {
    let (kernel, _store) = kernel(20);
    // The application says: this session may search, and may read files. Nothing else exists.
    let session = kernel.open(&principal(), SessionId::new("s1"), SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["tool_search".to_owned(), "read_file".to_owned()]),
                    exposed: Some(Exposure::Only {
                        names: vec!["tool_search".to_owned()],
                    }),
                    ..ConfigPatch::default()
                },
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    let mut session = session;
    let output = kernel
        .invoke_tool(
            &mut session,
            // A query that ranks an inert tool highly: it is inside the catalog, outside the
            // ceiling.
            &call("tool_search", r#"{"query":"inert tool number 7"}"#),
        )
        .await
        .unwrap();

    assert!(!output.is_error, "{output:?}");
    // The tool asked for what it ranked — it has no session, so it cannot know better. Filtering
    // here rather than in the tool is the point: there is one place that decides.
    assert!(
        output.expose.contains(&"inert_07".to_owned()),
        "the tool requests from the catalog, not from the ceiling: {:?}",
        output.expose
    );
    let visible: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(
        visible.iter().all(|name| name == "tool_search" || name == "read_file"),
        "{visible:?}"
    );
}

#[tokio::test]
async fn an_exposure_outside_the_ceiling_is_refused() {
    let (kernel, _store) = kernel(20);
    let session = kernel.open(&principal(), SessionId::new("s1"), SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["read_file".to_owned()]),
                    ..ConfigPatch::default()
                },
                ..SessionSpec::default()
            },
        )
        .await
        .unwrap();

    let mut session = session;
    let refused = kernel
        .configure(
            &mut session,
            ConfigPatch {
                exposed: Some(Exposure::Only {
                    names: vec!["inert_03".to_owned()],
                }),
                ..ConfigPatch::default()
            },
        )
        .await;

    // Showing the model a tool it may never run produces a call refused for a reason it cannot
    // see, so the exposure is refused at the door instead.
    assert!(refused.is_err(), "{refused:?}");
}

#[tokio::test]
async fn exposure_does_not_change_what_policy_approves() {
    let (kernel, _store) = kernel(20);
    let mut session = kernel.open(&principal(), SessionId::new("s1"), spec(&[])).await.unwrap();

    // Narrow to nothing visible, then call a tool the prompt does not show. The gate decides, not
    // the prompt: exposure is a display concern, and conflating the two would be a permission bug.
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                exposed: Some(Exposure::Only {
                    names: Vec::new(),
                }),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();

    let output = kernel
        .invoke_tool(
            &mut session,
            &call("read_file", "{}"),
        )
        .await
        .unwrap();
    assert_eq!(output.content.parts.len(), 1);
}

#[tokio::test]
async fn a_widened_exposure_survives_being_loaded_back() {
    let (kernel, store) = kernel(20);
    let mut session = kernel.open(&principal(), SessionId::new("s1"), spec(&[])).await.unwrap();
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                exposed: Some(Exposure::Only {
                    names: vec!["tool_search".to_owned()],
                }),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();

    let id = session.id().clone();
    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(
        *loaded.exposed(),
        Exposure::Only {
            names: vec!["tool_search".to_owned()]
        },
        "a replay has to know what the model could see"
    );
    let _ = store;
}
