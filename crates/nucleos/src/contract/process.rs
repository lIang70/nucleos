//! A process session: one command, its phases, its output, and how it is listed.

use super::{ExecOutput, SandboxPermissions, SessionId};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a session is for.
///
/// A process session is not a second kind of object: it is the same lifecycle — single writer,
/// event log, budget, sandbox, checkpoint — with a different syscall face. In Unix a shell
/// session is a process, and the same reasoning applies here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Runs a model loop.
    #[default]
    Agent,
    /// Runs one command, and stays open while it does.
    Process,
}

/// How to start a process session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSpec {
    /// The command line. No shell by default: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Where to run it. Absent means the session's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What this command asks for on top of the session's policy.
    #[serde(default)]
    pub permissions: SandboxPermissions,
}

/// Where a process is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessStatus {
    /// No pid yet: the session exists, the process does not.
    #[default]
    Pending,
    /// Running, under whatever isolation the platform could give.
    Running {
        /// The process id.
        pid: u32,
    },
    /// It ended.
    Exited {
        /// How.
        output: ExecOutput,
    },
    /// The process is gone and we do not know how it ended.
    ///
    /// A live process cannot be reconstructed from a log — its memory, its file descriptors and
    /// its cursor are not in the event stream. So a session that outlives the runtime says this,
    /// rather than pretending the command is still running.
    Lost,
}

/// A process session's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessState {
    /// How it was started.
    pub spec: ProcessSpec,
    /// Where it is now.
    pub status: ProcessStatus,
}

/// Where a process is, without the payload.
///
/// A filter asks "which ended?", not "which ended with exit code 3", so the tag is its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPhase {
    /// No pid yet.
    Pending,
    /// Running.
    Running,
    /// It ended.
    Exited,
    /// It is gone and we do not know how it ended.
    Lost,
}

impl ProcessStatus {
    /// The phase, without the payload.
    #[must_use]
    pub fn phase(&self) -> ProcessPhase {
        match self {
            Self::Pending => ProcessPhase::Pending,
            Self::Running { .. } => ProcessPhase::Running,
            Self::Exited { .. } => ProcessPhase::Exited,
            Self::Lost => ProcessPhase::Lost,
        }
    }
}

impl ProcessState {
    /// Whether the process has ended, one way or another.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status,
            ProcessStatus::Exited { .. } | ProcessStatus::Lost
        )
    }
}

/// What a listing asks for.
///
/// A keyset cursor rather than an offset: sessions are created and retired constantly, and an
/// offset would skip or repeat rows exactly when a process ends — which is the moment a caller is
/// most likely to be paging.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessQuery {
    /// Only sessions in this phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ProcessPhase>,
    /// How many to return. Zero asks for the default.
    #[serde(default)]
    pub limit: u32,
    /// Continue after this session id. Absent starts at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<SessionId>,
}

impl ProcessQuery {
    /// The page size this query asks for.
    #[must_use]
    pub fn page_size(&self) -> usize {
        match self.limit {
            0 => 50,
            limit => usize::try_from(limit.min(500)).unwrap_or(50),
        }
    }
}

/// One process, as a listing reports it.
///
/// Not the whole [`ProcessState`]: a listing is a cross-session view, and the environment a caller
/// passed to its command is a credential as often as it is a setting. What an operator needs to
/// identify a process is what it runs and where it is, so that is what a page carries — the full spec
/// stays behind `GET /v1/sessions/{id}`, which already answers only to the session's owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// The session that owns it.
    pub session: SessionId,
    /// The command line.
    pub argv: Vec<String>,
    /// Where it runs, when the caller said.
    pub cwd: Option<String>,
    /// Where it is now.
    pub status: ProcessStatus,
}

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessPage {
    /// The sessions in this page, in id order.
    pub processes: Vec<ProcessSummary>,
    /// Where to continue. Absent means this was the last page.
    pub next: Option<SessionId>,
}
