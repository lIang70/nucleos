//! Tool discovery, end to end: what the model sees is what the ceiling allows, and a search widens
//! it only as far as that ceiling.
//!
//! The invariant under test is the one that makes dynamic loading safe: exposure is not permission.
//! A tool can ask for anything; the kernel intersects with the ceiling, records the widening as an
//! event, and the gate in front of execution does not move.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{AllowAll, Store, Tool, ToolCtx, ToolOutput};
use nucleos::contract::{
    ConfigPatch, DiscoveryPolicy, Exposure, Principal, Routing, SessionId, ToolPolicy, ToolSpec,
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

/// A tool that does nothing, so the catalog can be made large enough to need discovery.
fn inert(name: &str, description: &str) -> Arc<dyn Tool> {
    Arc::new(Inert {
        spec: ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema_json: r#"{"type":"object"}"#.to_owned(),
            isolation: Some(nucleos::contract::Isolation::InProcess),
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
fn kernel(count: usize) -> (Kernel, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
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

/// A session with room to spend. The budget gate is real now, and a corpus test that ran on zero
/// credit would be testing the gate instead of discovery.
fn funded() -> SessionSpec {
    SessionSpec {
        credit_tokens: 1_000_000,
        ..SessionSpec::default()
    }
}

fn spec(names: &[&str]) -> SessionSpec {
    SessionSpec {
        config: ConfigPatch {
            tools: Some(names.iter().map(|name| (*name).to_owned()).collect()),
            ..ConfigPatch::default()
        },
        ..funded()
    }
}

fn policy(overrides: &[(&str, Exposure)]) -> ToolPolicy {
    ToolPolicy {
        default: Exposure::Deferred,
        exposures: overrides
            .iter()
            .map(|(name, exposure)| ((*name).to_owned(), *exposure))
            .collect(),
        ..ToolPolicy::default()
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
async fn everything_starts_deferred_so_the_prompt_carries_only_the_door() {
    let (kernel, _store) = kernel(20);
    let session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();

    // Twenty-one tools and the model sees one: the one that finds the others. Showing everything
    // is the behaviour discovery exists to replace, so it is not the default.
    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names, ["tool_search"]);
}

#[tokio::test]
async fn an_always_exposure_is_the_configured_baseline() {
    let (kernel, _store) = kernel(20);
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(policy(&[("read_file", Exposure::Always)])),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names, ["read_file", "tool_search"]);
}

#[tokio::test]
async fn a_hidden_tool_is_never_in_the_prompt_but_is_still_callable() {
    let (kernel, _store) = kernel(20);
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(policy(&[("read_file", Exposure::Hidden)])),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(!names.contains(&"read_file".to_owned()), "{names:?}");

    // Hiding is a display decision. The gate is unmoved, so the call still runs.
    let output = kernel
        .invoke_tool(&mut session, &call("read_file", "{}"))
        .await
        .unwrap();
    assert!(!output.is_error);
}

#[tokio::test]
async fn a_catalog_shown_in_full_costs_no_search_schema() {
    let (kernel, _store) = kernel(2);
    // The application says: show everything. There is then nothing to find, so the door goes too.
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(ToolPolicy {
                        default: Exposure::Always,
                        ..policy(&[])
                    }),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();
    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    assert!(!names.contains(&"tool_search".to_owned()), "{names:?}");
    assert_eq!(names.len(), 3, "{names:?}");
}

#[tokio::test]
async fn a_hidden_tool_stays_hidden_however_small_the_catalog() {
    let (kernel, _store) = kernel(1);
    // Two tools, so nothing about this catalog looks like it needs a policy. It still gets one:
    // the size of the registry is not a reason to override what the application declared.
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["read_file".to_owned(), "inert_00".to_owned()]),
                    tool_policy: Some(ToolPolicy {
                        default: Exposure::Always,
                        exposures: [("inert_00".to_owned(), Exposure::Hidden)]
                            .into_iter()
                            .collect(),
                        ..ToolPolicy::default()
                    }),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names, ["read_file"], "{names:?}");
}

#[tokio::test]
async fn a_large_catalog_gets_one_search_tool() {
    let (kernel, _store) = kernel(20);
    let session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();
    let specs = kernel.tool_specs(&session);

    assert_eq!(
        specs
            .iter()
            .filter(|spec| spec.name == "tool_search")
            .count(),
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
    let (kernel, store) = kernel(20);
    let session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();
    assert_eq!(
        session.tool_policy().default,
        Exposure::Deferred,
        "not configured means not shown"
    );
    let mut session = session;

    let output = kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"read a file"}"#),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(output.expose, ["read_file"]);

    // Found, therefore shown: from the next prompt on, without asking again.
    let visible: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(visible.contains(&"read_file".to_owned()), "{visible:?}");

    // And the pool is in the record, because a replay has to know what turn 42 could see.
    let kinds: Vec<&str> = store
        .events_since(&key(&session.id().clone()), 0)
        .await
        .unwrap()
        .iter()
        .map(|event| event.kind.type_name())
        .collect();
    assert!(kinds.contains(&"nucleos.v1.tools_exposed"), "{kinds:?}");
}

#[tokio::test]
async fn the_ceiling_still_wins_over_a_search() {
    let (kernel, _store) = kernel(20);
    // The application says: this session may search, and may read files. Nothing else exists.
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["tool_search".to_owned(), "read_file".to_owned()]),
                    tool_policy: Some(policy(&[("read_file", Exposure::Always)])),
                    ..ConfigPatch::default()
                },
                ..funded()
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
        visible
            .iter()
            .all(|name| name == "tool_search" || name == "read_file"),
        "{visible:?}"
    );
}

#[tokio::test]
async fn an_exposure_outside_the_ceiling_is_refused() {
    let (kernel, _store) = kernel(20);
    let session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["read_file".to_owned()]),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    let mut session = session;
    let refused = kernel
        .configure(
            &mut session,
            ConfigPatch {
                tool_policy: Some(policy(&[("inert_03", Exposure::Always)])),
                ..ConfigPatch::default()
            },
        )
        .await;

    // An `always` outside the ceiling would put a tool in the prompt that may never run: the model
    // would call it, the gate would refuse, and the refusal would be unexplainable from where the
    // model sits. So it is refused at the door instead.
    assert!(refused.is_err(), "{refused:?}");
}

#[tokio::test]
async fn exposure_does_not_change_what_policy_approves() {
    let (kernel, _store) = kernel(20);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();

    // Narrow to nothing visible, then call a tool the prompt does not show. The gate decides, not
    // the prompt: exposure is a display concern, and conflating the two would be a permission bug.
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                tool_policy: Some(policy(&[("read_file", Exposure::Hidden)])),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();

    let output = kernel
        .invoke_tool(&mut session, &call("read_file", "{}"))
        .await
        .unwrap();
    assert_eq!(output.content.parts.len(), 1);
}

#[tokio::test]
async fn a_widened_exposure_survives_being_loaded_back() {
    let (kernel, store) = kernel(20);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                tool_policy: Some(policy(&[("read_file", Exposure::Always)])),
                ..ConfigPatch::default()
            },
        )
        .await
        .unwrap();
    kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"inert tool 4"}"#),
        )
        .await
        .unwrap();

    let id = session.id().clone();
    let loaded = kernel.load(&principal(), &id).await.unwrap();
    assert_eq!(
        loaded.tool_policy().exposure_of("read_file"),
        Exposure::Always,
        "a replay has to know the policy that shaped turn 42"
    );
    assert!(
        loaded.discovered().contains_key("inert_04"),
        "and what had been found by then: {:?}",
        loaded.discovered()
    );
    let _ = store;
}

#[tokio::test]
async fn a_search_tool_cannot_be_configured_away() {
    let (kernel, _store) = kernel(20);
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), spec(&[]))
        .await
        .unwrap();

    // A catalog whose only door is hidden is a catalog the model can never enter.
    let refused = kernel
        .configure(
            &mut session,
            ConfigPatch {
                tool_policy: Some(policy(&[("tool_search", Exposure::Deferred)])),
                ..ConfigPatch::default()
            },
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
}

#[tokio::test]
async fn the_pool_forgets_what_has_gone_quiet() {
    let (kernel, _store) = kernel(20);
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(ToolPolicy {
                        discovery: DiscoveryPolicy {
                            idle_rounds: 2,
                            ..DiscoveryPolicy::default()
                        },
                        ..policy(&[])
                    }),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"inert tool 4"}"#),
        )
        .await
        .unwrap();
    assert!(session.discovered().contains_key("inert_04"));

    // Three turns with no use of it: the pool is not a second registry that only grows. A turn,
    // not a test hook, is what expires an entry — that is the clock the policy names.
    for _ in 0..3 {
        kernel.inference(&mut session, &[]).await.unwrap();
    }
    assert!(
        !session.discovered().contains_key("inert_04"),
        "{:?}",
        session.discovered()
    );
}

#[tokio::test]
async fn one_enormous_schema_does_not_evict_the_smaller_tools_behind_it() {
    let store = Arc::new(SqliteStore::in_memory().expect("an in-memory database"));
    let bulky: Arc<dyn Tool> = Arc::new(Inert {
        spec: ToolSpec {
            // Sorts first among equals, so it is considered before the small one.
            name: "aaa_bulky".to_owned(),
            description: "A tool with an enormous schema.".to_owned(),
            input_schema_json: format!(
                r#"{{"type":"object","description":"{}"}}"#,
                "x".repeat(4096)
            ),
            isolation: Some(nucleos::contract::Isolation::InProcess),
        },
    });
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .store(Arc::clone(&store) as Arc<dyn Store>)
        .policy(Arc::new(AllowAll))
        .clock(Arc::new(FixedClock::at(0)))
        .routing(Routing {
            chain: vec!["primary".to_owned()],
            retries: 0,
        })
        .tool(bulky)
        .tool(inert("zzz_small", "A tool with a small schema."))
        .build()
        .unwrap();

    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(ToolPolicy {
                        // Room for the door and one small schema, and nothing near 4 KiB.
                        budget: nucleos::contract::DefinitionBudget {
                            max_definitions: 8,
                            max_bytes: 900,
                        },
                        ..policy(&[])
                    }),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"tool schema"}"#),
        )
        .await
        .unwrap();
    let names: Vec<String> = kernel
        .tool_specs(&session)
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    // The oversized schema is skipped, not treated as the end of the list.
    assert!(names.contains(&"zzz_small".to_owned()), "{names:?}");
    assert!(!names.contains(&"aaa_bulky".to_owned()), "{names:?}");
}

#[tokio::test]
async fn the_event_names_what_reached_the_prompt_not_what_entered_the_pool() {
    let (kernel, store) = kernel(20);
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tool_policy: Some(ToolPolicy {
                        // Exactly one definition: the door takes it, so nothing found can show.
                        budget: nucleos::contract::DefinitionBudget {
                            max_definitions: 1,
                            max_bytes: u64::MAX,
                        },
                        ..policy(&[])
                    }),
                    ..ConfigPatch::default()
                },
                ..funded()
            },
        )
        .await
        .unwrap();

    kernel
        .invoke_tool(
            &mut session,
            &call("tool_search", r#"{"query":"read a file"}"#),
        )
        .await
        .unwrap();

    // It is remembered, and callable by name — but it did not reach the prompt, so no event says
    // it did.
    assert!(session.discovered().contains_key("read_file"));
    let exposed: Vec<&str> = store
        .events_since(&key(&session.id().clone()), 0)
        .await
        .unwrap()
        .iter()
        .filter(|event| event.kind.type_name() == "nucleos.v1.tools_exposed")
        .map(|_| "tools_exposed")
        .collect();
    assert!(
        exposed.is_empty(),
        "the budget could not fit it: {exposed:?}"
    );
}
