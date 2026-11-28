//! Application proposals and user-owned envelope changes are separate native operations.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use nucleos_contract::wire::{ApproveEnvelope, RequestEnvelopeChange, RevokeCapabilities};
use nucleos_contract::{AppId, Authority, Confine, Envelope, ErrorKind, Mode, ModelRef, ToolName};

fn model() -> ModelRef {
    ModelRef::new("openai/gpt-5").unwrap()
}

fn authority(tools: &[&str]) -> Authority {
    Authority {
        models: BTreeMap::from([(model(), BTreeSet::from([Mode::Text]))]),
        tools: tools
            .iter()
            .map(|name| ToolName::new(*name).unwrap())
            .collect(),
        sandbox: Confine::default(),
        skills: BTreeSet::new(),
    }
}

fn current() -> Envelope {
    Envelope {
        app: AppId::new("writer").unwrap(),
        version: 7,
        authority: authority(&["read"]),
    }
}

#[test]
fn an_application_proposes_an_expansion_without_granting_it_to_itself() {
    let current = current();
    let proposal = RequestEnvelopeChange {
        base_version: current.version,
        authority: authority(&["read", "write"]),
        reason: "The user asked me to edit the draft".to_owned(),
    };
    let json = serde_json::to_value(&proposal).unwrap();
    assert!(json.get("app").is_none());
    let proposal: RequestEnvelopeChange = serde_json::from_value(json).unwrap();

    assert!(current.validate(&proposal.authority).is_err());
    proposal
        .validate(&current, &authority(&["read", "write"]))
        .unwrap();
    assert!(
        !current
            .authority
            .allows_tool(&ToolName::new("write").unwrap())
    );
}

#[test]
fn user_approval_advances_the_version_and_stale_approval_conflicts() {
    let current = current();
    let deployment = authority(&["read", "write"]);
    let approval = ApproveEnvelope {
        base_version: 7,
        authority: authority(&["read", "write"]),
    };
    let json = serde_json::to_string(&approval).unwrap();
    let approval: ApproveEnvelope = serde_json::from_str(&json).unwrap();
    let approved = approval.apply(&current, &deployment).unwrap();

    assert_eq!(approved.version, 8);
    assert_eq!(approved.app, current.app);
    assert!(
        approved
            .authority
            .allows_tool(&ToolName::new("write").unwrap())
    );
    assert_eq!(
        current.version, 7,
        "a proposal or approval body does not mutate its input"
    );

    let stale = approval.apply(&approved, &deployment).unwrap_err();
    assert_eq!(stale.kind, ErrorKind::Conflict);
    assert_eq!(stale.field.as_deref(), Some("base_version"));
}

#[test]
fn revocation_restricts_the_next_lookup_for_an_existing_session_plan() {
    let approved = Envelope {
        version: 8,
        authority: authority(&["read", "write"]),
        ..current()
    };
    let session = authority(&["read", "write"]);
    approved.validate(&session).unwrap();

    let revocation = RevokeCapabilities {
        base_version: 8,
        remaining: authority(&["read"]),
    };
    let json = serde_json::to_value(&revocation).unwrap();
    assert!(json.get("app").is_none());
    let revocation: RevokeCapabilities = serde_json::from_value(json).unwrap();
    let revoked = revocation.apply(&approved).unwrap();

    assert_eq!(revoked.version, 9);
    assert!(
        revoked
            .authority
            .allows_tool(&ToolName::new("read").unwrap())
    );
    assert!(
        !revoked
            .authority
            .allows_tool(&ToolName::new("write").unwrap())
    );
    assert_eq!(
        session
            .fits_in(&revoked.authority)
            .unwrap_err()
            .field
            .as_deref(),
        Some("authority.tools")
    );
}

#[test]
fn neither_approval_nor_revocation_can_escape_its_upper_boundary() {
    let current = current();
    let approval = ApproveEnvelope {
        base_version: 7,
        authority: authority(&["read", "exec"]),
    };
    assert!(approval.apply(&current, &authority(&["read"])).is_err());

    let revocation = RevokeCapabilities {
        base_version: 7,
        remaining: authority(&["read", "write"]),
    };
    assert!(revocation.apply(&current).is_err());
}

#[test]
fn envelope_management_operations_produce_native_schemas() {
    for (schema, fields) in [
        (
            serde_json::to_string(&schemars::schema_for!(RequestEnvelopeChange)).unwrap(),
            vec!["base_version", "authority", "reason", "Authority"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(ApproveEnvelope)).unwrap(),
            vec!["base_version", "authority", "Authority"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(RevokeCapabilities)).unwrap(),
            vec!["base_version", "remaining", "Authority"],
        ),
        (
            serde_json::to_string(&schemars::schema_for!(Envelope)).unwrap(),
            vec!["app", "version", "authority", "Authority"],
        ),
    ] {
        for field in fields {
            assert!(schema.contains(field), "{field} is missing from {schema}");
        }
    }
}
