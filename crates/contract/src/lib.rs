//! The vocabulary every layer shares.
//!
//! No I/O, no async, no runtime. This crate is data plus the pure functions that
//! decide what the data may be — which is what lets an SDK generator, a billing
//! job and the kernel itself agree on the same shapes.
//!
//! # What it is the bottom of
//!
//! Three seams a deployment implements, and the nouns they speak between them:
//!
//! ```text
//!             content          Message · Content · Part — what travels
//!          ┌────┴────┐
//!       tool      inference    ToolCall → ToolResult · model → answer
//!          │           │
//!         —      capability · extension · outcome
//!          │
//!      sandbox               Confine · ExecOptions · SessionExit
//! ```
//!
//! `sandbox` depends on nothing but `error`: it is a parallel leaf, not something built on
//! content. Accounting is part of `inference`, because nothing else has a price: a tool spends
//! wall-clock time inside a sandbox boundary and reports no meter, while a session's spend is the
//! sum of its model calls. Above those seam nouns, `identity` names an application's sessions and
//! `authority` defines the deployment → application envelope → session plan permission hierarchy.
//!
//! # What is deliberately not here
//!
//! Session state, sequence numbers, checkpoints and the fact log. Those are runtime nouns and
//! return with the layer that owns persistence. This crate contains only data and pure functions
//! over those native shapes.

pub mod authority;
pub mod content;
pub mod error;
pub mod fact;
pub mod identity;
pub mod inference;
pub mod sandbox;
pub mod skill;
pub mod tool;
pub mod wire;

pub use authority::{Authority, Envelope};
pub use content::{
    Arguments, Content, MediaSource, Message, Metadata, Part, PartKind, Role, ToolCall, ToolName,
    ToolResult,
};
pub use error::{Error, ErrorKind, Result};
pub use fact::{BlobId, CloseReason, EffectStatus, Fact, FactKind, Seq};
pub use identity::{AppId, SessionId, SessionKey};
pub use inference::{
    Billing, CacheTtl, Capabilities, DecideRequest, Decision, DecisionResponse, Details,
    EmbedRequest, EmbedResponse, ExtensionId, ExtensionSet, Field, FinishReason, GenerateResponse,
    InputTokenUsage, Lifecycle, Limits, Modality, Mode, ModelDescriptor, ModelId, ModelRef,
    ModelRequest, Money, Outcome, Output, OutputTokenUsage, Probability, Question, QuestionKind,
    ReasoningAccounting, StreamEvent, Usage, Warning, WorkKind,
};
pub use sandbox::{
    BackendCapabilities, Confine, Enforcement, EnvPolicy, ExecOptions, ExitReason, Files, Mitm,
    NetAction, NetMode, NetPolicy, NetRule, Resources, SessionExit, SessionFeatures, SessionSpec,
};
pub use skill::{Skill, SkillRootName};
pub use tool::{Approval, ApprovalOption, ApprovalRequired, Exposure, ToolDefinition};
pub use wire::{
    ApproveEnvelope, CallTools, ConfigureSession, OpenSession, RequestEnvelopeChange,
    RevokeCapabilities, SessionView, ToolBatchResult, ToolCatalog,
};
