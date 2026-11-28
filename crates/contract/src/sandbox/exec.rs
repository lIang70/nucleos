use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a command may be run.
///
/// The ceiling, not the request: a call may ask for a shorter timeout through
/// `ExecOptions`, never a longer one. `shell` names the shell a command tool
/// should use; empty means the backend's own.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExecPolicy {
    /// Ceiling for one command, in milliseconds. Absent means the backend's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_millis: Option<u64>,
    /// The shell a command tool should use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

impl ExecPolicy {
    /// Whether `next` permits no more than `self` does.
    ///
    /// A shorter timeout narrows; naming a different shell does not narrow or
    /// widen, it changes the program, so it is only allowed to stay the same or
    /// be left to the backend.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        let shortens = match (self.timeout_millis, next.timeout_millis) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(ceiling), Some(asked)) => asked <= ceiling,
        };
        let same_shell = next.shell.is_none() || next.shell == self.shell;
        shortens && same_shell
    }
}

/// Caps on what one command may consume.
///
/// Zero means unbounded here; a backend that cannot enforce a non-zero value must
/// refuse the session rather than ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Resources {
    /// CPU share, in thousandths of a core.
    #[serde(default)]
    pub cpu_millicores: u32,
    /// Memory ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Disk ceiling. No local backend reports this today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// Cap on captured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

impl Resources {
    /// The effective caps for one call: the ceiling, lowered by what the call
    /// asked for. A call may ask for **less**; asking for more gets the ceiling.
    #[must_use]
    pub fn narrowed_to(&self, call: &Self) -> Self {
        Self {
            cpu_millicores: if call.cpu_millicores == 0 {
                self.cpu_millicores
            } else if self.cpu_millicores == 0 {
                call.cpu_millicores
            } else {
                self.cpu_millicores.min(call.cpu_millicores)
            },
            memory_bytes: lower(self.memory_bytes, call.memory_bytes),
            disk_bytes: lower(self.disk_bytes, call.disk_bytes),
            max_output_bytes: lower(self.max_output_bytes, call.max_output_bytes),
        }
    }
}

fn lower(ceiling: Option<u64>, asked: Option<u64>) -> Option<u64> {
    match (ceiling, asked) {
        (None, asked) => asked,
        (ceiling, None) => ceiling,
        (Some(ceiling), Some(asked)) => Some(ceiling.min(asked)),
    }
}

// ---------------------------------------------------------------------------
// Confine
// ---------------------------------------------------------------------------

/// What one command asks for.
///
/// The working directory is here and not in [`Confine`] because it is a per-call
/// argument: a session has no single directory, and a default would silently
/// become the daemon's own.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ExecOptions {
    /// Where it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    /// Bytes written to its stdin before it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<Vec<u8>>,
    /// Ceiling for this command, never above the confine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_millis: Option<u64>,
    /// The caps this call asks for; they may only lower the ceiling.
    #[serde(default)]
    pub resources: Resources,
}

/// One session to start.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SessionSpec {
    /// Caller-chosen id.
    pub id: String,
    /// The command line. No shell: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Whether to allocate a pty.
    #[serde(default)]
    pub tty: bool,
    /// Rows, for a pty.
    #[serde(default)]
    pub rows: u16,
    /// Columns, for a pty.
    #[serde(default)]
    pub cols: u16,
    /// Whether an input side is wanted.
    #[serde(default)]
    pub stdin: bool,
    /// Per-command options.
    #[serde(default)]
    pub options: ExecOptions,
}
