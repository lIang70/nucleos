#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::content::ToolName;
use crate::identity::AppId;
use crate::inference::{Mode, ModelRef};
use crate::sandbox::{Confine, Files};
use crate::skill::SkillRootName;

fn model() -> ModelRef {
    ModelRef::new("openai/gpt-5").unwrap()
}

fn tool(name: &str) -> ToolName {
    ToolName::new(name).unwrap()
}

fn root(name: &str) -> SkillRootName {
    SkillRootName::new(name).unwrap()
}

fn empty_authority() -> Authority {
    Authority {
        models: BTreeMap::new(),
        tools: BTreeSet::new(),
        sandbox: Confine::default(),
        skills: BTreeSet::new(),
    }
}

#[test]
fn native_requests_query_native_resources_directly() {
    let mut authority = empty_authority();
    authority
        .models
        .insert(model(), BTreeSet::from([Mode::Text]));
    authority.tools.insert(tool("read"));
    authority.skills.insert(root("engineering"));

    assert!(authority.allows_model(&model(), Mode::Text));
    assert!(!authority.allows_model(&model(), Mode::Embed));
    assert!(authority.allows_tool(&tool("read")));
    assert!(!authority.allows_tool(&tool("write")));
    assert!(authority.allows_skill(&root("engineering")));
    assert!(!authority.allows_skill(&root("personal")));
}

#[test]
fn a_session_is_a_subset_of_the_application_authority() {
    let mut application = empty_authority();
    application
        .models
        .insert(model(), BTreeSet::from([Mode::Text, Mode::ToolCall]));
    application.tools.extend([tool("read"), tool("write")]);
    application.skills.insert(root("engineering"));
    let envelope = Envelope {
        app: AppId::new("writer").unwrap(),
        version: 7,
        authority: application,
    };

    let mut session = empty_authority();
    session.models.insert(model(), BTreeSet::from([Mode::Text]));
    session.tools.insert(tool("read"));
    assert!(envelope.validate(&session).is_ok());

    session.tools.insert(tool("exec"));
    let error = envelope.validate(&session).unwrap_err();
    assert_eq!(error.field.as_deref(), Some("authority.tools"));
}

#[test]
fn meeting_authorities_intersects_each_native_domain() {
    let mut left = empty_authority();
    left.models
        .insert(model(), BTreeSet::from([Mode::Text, Mode::ToolCall]));
    left.tools.extend([tool("read"), tool("write")]);
    left.skills.extend([root("engineering"), root("personal")]);

    let mut right = empty_authority();
    right.models.insert(model(), BTreeSet::from([Mode::Text]));
    right.tools.extend([tool("read"), tool("exec")]);
    right.skills.extend([root("engineering"), root("finance")]);

    let met = left.meet(&right).unwrap();
    assert_eq!(met.models[&model()], BTreeSet::from([Mode::Text]));
    assert_eq!(met.tools, BTreeSet::from([tool("read")]));
    assert_eq!(met.skills, BTreeSet::from([root("engineering")]));
}

#[test]
fn a_sandbox_boundary_cannot_exceed_the_envelope() {
    let application = Authority {
        sandbox: Confine {
            files: Files::read_only(),
            ..Confine::default()
        },
        ..empty_authority()
    };
    let session = Authority {
        sandbox: Confine {
            files: Files {
                read: vec!["/srv".to_owned()],
                write: vec!["/srv".to_owned()],
                read_only: false,
            },
            ..Confine::default()
        },
        ..empty_authority()
    };
    let error = session.fits_in(&application).unwrap_err();
    assert_eq!(error.field.as_deref(), Some("authority.sandbox"));
}

#[test]
fn replacing_the_current_envelope_restricts_the_next_native_lookup() {
    let app = AppId::new("writer").unwrap();
    let mut original = empty_authority();
    original.tools.insert(tool("read"));
    let original = Envelope {
        app: app.clone(),
        version: 1,
        authority: original,
    };
    let tightened = Envelope {
        app,
        version: 2,
        authority: empty_authority(),
    };
    let call = tool("read");

    assert!(original.authority.allows_tool(&call));
    assert!(!tightened.authority.allows_tool(&call));
}

#[test]
fn incomparable_sandbox_boundaries_are_not_silently_mapped_to_one_side() {
    let writable = |path: &str| Confine {
        files: Files {
            read: vec![path.to_owned()],
            write: vec![path.to_owned()],
            read_only: false,
        },
        ..Confine::default()
    };
    let left = Authority {
        sandbox: writable("/srv/left"),
        ..empty_authority()
    };
    let right = Authority {
        sandbox: writable("/srv/right"),
        ..empty_authority()
    };
    assert!(left.meet(&right).is_err());
}
