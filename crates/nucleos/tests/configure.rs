//! Changing a session while it runs.
//!
//! Two invariants are what make this safe, and both are tested here: every change is an event (so a
//! replay knows which system prompt was in force at turn 42), and the tool allow-list only narrows
//! (so an application cannot route around the generation it is pinned to).

#![allow(clippy::unwrap_used)]

use nucleos::adapter::{AllowAll, FakeModel, FixedClock, MemoryStore, Store};
use nucleos::contract::{
    ConfigPatch, ErrorCode, EventKind, GenerateParams, Hints, Principal, Routing, ServiceTier,
    SessionId, StopReason, Verbosity,
};
use nucleos::kernel::{Kernel, SessionSpec};
use std::sync::Arc;

fn principal() -> Principal {
    Principal {
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }
}

fn kernel() -> (Kernel, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let kernel = Kernel::builder()
        .model("primary", Arc::new(FakeModel::text(["unused"])))
        .model("cheap", Arc::new(FakeModel::text(["unused"])))
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

fn kinds(store: &MemoryStore, id: &SessionId) -> Vec<&'static str> {
    store
        .recorded(id)
        .iter()
        .map(|event| event.kind.type_name())
        .collect()
}

#[tokio::test]
async fn a_change_is_recorded_and_survives_a_reload() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(&principal(), id.clone(), SessionSpec::default())
        .await?;

    let changed = kernel
        .configure(
            &mut session,
            ConfigPatch {
                system: Some(Some("You are a careful reviewer.".to_owned())),
                ..ConfigPatch::default()
            },
        )
        .await?;

    assert_eq!(changed, vec!["system"]);
    // A replay has to know which system prompt was in force at this point; without the event the
    // log rebuilds a conversation that never happened.
    assert!(kinds(&store, &id).contains(&"nucleos.v1.session_configured"));

    let reloaded = kernel.load(&principal(), &id).await?;
    assert_eq!(
        reloaded.system(),
        Some("You are a careful reviewer."),
        "the checkpoint carries it, or a resume changes behaviour"
    );
    Ok(())
}

#[tokio::test]
async fn a_system_prompt_can_be_cleared() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    system: Some(Some("first".to_owned())),
                    ..ConfigPatch::default()
                },
                ..SessionSpec::default()
            },
        )
        .await?;

    // Doubly optional: the outer `Some` means "this was in the patch", the inner `None` means
    // "clear it". A single `Option` could not tell those apart.
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                system: Some(None),
                ..ConfigPatch::default()
            },
        )
        .await?;
    assert_eq!(session.system(), None);
    Ok(())
}

#[tokio::test]
async fn hints_merge_field_by_field() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await?;

    kernel
        .configure(
            &mut session,
            ConfigPatch {
                hints: Some(Hints {
                    verbosity: Some(Verbosity::Low),
                    ..Hints::default()
                }),
                ..ConfigPatch::default()
            },
        )
        .await?;
    kernel
        .configure(
            &mut session,
            ConfigPatch {
                hints: Some(Hints {
                    service_tier: Some(ServiceTier::Flex),
                    ..Hints::default()
                }),
                ..ConfigPatch::default()
            },
        )
        .await?;

    // The second patch set one field; overwriting the block would have quietly reset the first.
    assert_eq!(session.hints().verbosity, Some(Verbosity::Low));
    assert_eq!(session.hints().service_tier, Some(ServiceTier::Flex));
    Ok(())
}

#[tokio::test]
async fn parameters_are_replaced_whole() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    params: Some(GenerateParams {
                        temperature: Some(0.9),
                        max_output_tokens: Some(4096),
                        ..Default::default()
                    }),
                    ..ConfigPatch::default()
                },
                ..SessionSpec::default()
            },
        )
        .await?;

    kernel
        .configure(
            &mut session,
            ConfigPatch {
                params: Some(GenerateParams {
                    temperature: Some(0.2),
                    ..Default::default()
                }),
                ..ConfigPatch::default()
            },
        )
        .await?;

    // Replace, not merge: every field of `GenerateParams` is already optional, so a merge would make
    // "unset the ceiling" impossible to express.
    assert_eq!(session.params().temperature, Some(0.2));
    assert_eq!(session.params().max_output_tokens, None);
    Ok(())
}

#[tokio::test]
async fn the_tool_allow_list_narrows_and_never_widens() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(
            &principal(),
            SessionId::new("s1"),
            SessionSpec {
                config: ConfigPatch {
                    tools: Some(vec!["read_file".to_owned(), "write_file".to_owned()]),
                    ..ConfigPatch::default()
                },
                ..SessionSpec::default()
            },
        )
        .await?;

    kernel
        .configure(
            &mut session,
            ConfigPatch {
                tools: Some(vec!["read_file".to_owned()]),
                ..ConfigPatch::default()
            },
        )
        .await?;
    assert_eq!(session.tools(), ["read_file"]);

    // Adding a name back is a widening, and a widening would let an application route around the
    // generation it is pinned to.
    let error = kernel
        .configure(
            &mut session,
            ConfigPatch {
                tools: Some(vec!["read_file".to_owned(), "write_file".to_owned()]),
                ..ConfigPatch::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::Denied);
    assert!(error.reason().contains("narrow"));

    // And an empty list means "everything", which is the widest change there is.
    let error = kernel
        .configure(
            &mut session,
            ConfigPatch {
                tools: Some(Vec::new()),
                ..ConfigPatch::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::Denied);
    Ok(())
}

#[tokio::test]
async fn a_route_that_names_no_model_is_refused_before_it_is_stored() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await?;

    let error = kernel
        .configure(
            &mut session,
            ConfigPatch {
                routing: Some(Routing {
                    chain: vec!["nobody".to_owned()],
                    retries: 0,
                }),
                ..ConfigPatch::default()
            },
        )
        .await
        .err()
        .unwrap();

    // Discovered now, not on the next turn: a route to a model that does not exist is a
    // configuration error, and finding out mid-conversation is finding out too late.
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(session.routing().chain, ["primary"]);
    Ok(())
}

#[tokio::test]
async fn a_closed_session_cannot_be_configured() -> nucleos::Result<()> {
    let (kernel, _) = kernel();
    let mut session = kernel
        .open(&principal(), SessionId::new("s1"), SessionSpec::default())
        .await?;
    kernel.close(&mut session, StopReason::Completed).await?;

    let error = kernel
        .configure(
            &mut session,
            ConfigPatch {
                system: Some(Some("too late".to_owned())),
                ..ConfigPatch::default()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code(), ErrorCode::Invalid);
    Ok(())
}

#[tokio::test]
async fn an_empty_patch_changes_nothing_and_records_nothing() -> nucleos::Result<()> {
    let (kernel, store) = kernel();
    let id = SessionId::new("s1");
    let mut session = kernel
        .open(&principal(), id.clone(), SessionSpec::default())
        .await?;
    let before = store.recorded(&id).len();

    let changed = kernel
        .configure(&mut session, ConfigPatch::default())
        .await?;

    assert!(changed.is_empty());
    assert_eq!(
        store.recorded(&id).len(),
        before,
        "an empty patch is not an event"
    );
    Ok(())
}

#[test]
fn the_event_carries_the_patch() {
    // Replay needs the values, not just "something changed".
    let event = EventKind::SessionConfigured {
        patch: ConfigPatch {
            system: Some(Some("later".to_owned())),
            ..ConfigPatch::default()
        },
    };
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded["kind"], "session_configured");
    assert_eq!(encoded["patch"]["system"], "later");
}
