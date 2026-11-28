//! Start and manage process sessions.
//!
//! The seam is **sessions**, not one-shot calls: a backend implements "start,
//! read by cursor, write, signal, wait", and one-shot execution is what a caller
//! does to a session. A backend therefore has one code path, not two to keep in
//! step.

use async_trait::async_trait;
use nucleos_contract::{BackendCapabilities, ExecOptions, Result, SessionExit, SessionSpec};

/// Which output stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// A pty, which merges the two.
    Tty,
}

/// One piece of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    /// The sequence number this chunk occupies.
    pub seq: u64,
    /// Which stream it came from.
    pub stream: SessionStream,
    /// The bytes, already cut at a UTF-8 boundary by the backend.
    pub bytes: Vec<u8>,
}

/// A page of output, read from a cursor.
///
/// A log rather than a stream: a caller that reconnects — a restarted client, a
/// dropped connection, a supervisor adopting its own process table — resumes from
/// the sequence number it has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionOutput {
    /// The cursor to read from next time.
    pub next_seq: u64,
    /// The chunks after the requested cursor.
    pub chunks: Vec<OutputChunk>,
    /// Whether the process has finished writing.
    pub eof: bool,
}

/// A running session.
#[async_trait]
pub trait Session: Send + Sync {
    /// Its id.
    fn id(&self) -> &str;

    /// Its process id, when it has one.
    fn pid(&self) -> Option<u32>;

    /// Read output after `after_seq`, up to `max_bytes`.
    async fn read(&self, after_seq: u64, max_bytes: usize) -> Result<SessionOutput>;

    /// Write to its input.
    async fn write(&self, bytes: &[u8]) -> Result<()>;

    /// Close its input after everything has been written.
    async fn close_input(&self) -> Result<()>;

    /// Terminate it.
    async fn terminate(&self) -> Result<()>;

    /// Wait for it to end.
    async fn wait(&self) -> Result<SessionExit>;

    /// Release the handle. Terminates a still-running session, because a handle
    /// that outlived its process is worse than a handle that ended it.
    async fn close(&self) -> Result<()>;
}

/// A snapshot of one managed session, for a listing.
///
/// The specification is deliberately reduced: `env` never leaves the session it
/// belongs to, so a listing cannot become a credential dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Which session.
    pub id: String,
    /// The command line.
    pub argv: Vec<String>,
    /// Its process id.
    pub pid: Option<u32>,
    /// Whether it is still running.
    pub running: bool,
    /// How it ended, when it has.
    pub exit: Option<SessionExit>,
}

/// A whole backend.
#[async_trait]
pub trait Runner: Send + Sync {
    /// What it can enforce, and which session features it provides.
    fn capabilities(&self) -> BackendCapabilities;

    /// Start one session under the confine the instance was built with.
    async fn start(&self, spec: SessionSpec) -> Result<Box<dyn Session>>;

    /// Everything it is managing.
    async fn list(&self) -> Result<Vec<SessionInfo>>;

    /// Terminate one session by id.
    async fn terminate(&self, id: &str) -> Result<()>;

    /// Release every resource it owns: active sessions, forked daemons, sockets,
    /// backend state.
    ///
    /// Part of the contract rather than a convention: a decorator that swallows it
    /// leaks, and a decorator that swallows it while still advertising the
    /// capability makes the declaration a lie.
    async fn close(&self) -> Result<()>;
}

/// The part of a runner a tool may use: it starts a session and nothing else.
///
/// The split is authority, not convenience. Managing sessions is a different
/// authority from using one: a tool that may add a command channel needs `start`;
/// only the boundary's owner may enumerate or terminate what is running. Handing a
/// tool a whole [`Runner`] would quietly hand it both.
#[async_trait]
pub trait SessionStarter: Send + Sync {
    /// Start one session.
    async fn start(&self, spec: SessionSpec) -> Result<Box<dyn Session>>;
}

#[async_trait]
impl<T: Runner + ?Sized> SessionStarter for T {
    async fn start(&self, spec: SessionSpec) -> Result<Box<dyn Session>> {
        Runner::start(self, spec).await
    }
}

/// One command, run to completion.
///
/// It is a derived view over a session, exported because every caller would
/// otherwise write the same drain-and-truncate loop — and one of them would get
/// the UTF-8 boundary or the timeout wrong.
pub async fn exec(
    runner: &dyn Runner,
    id: impl Into<String>,
    argv: Vec<String>,
    mut options: ExecOptions,
) -> Result<(SessionExit, SessionOutput)> {
    let spec = SessionSpec {
        id: id.into(),
        argv,
        options: ExecOptions {
            stdin: options.stdin.take(),
            ..options
        },
        ..SessionSpec::default()
    };
    let session = runner.start(spec).await?;
    let mut collected = SessionOutput::default();
    let exit = loop {
        let page = session.read(collected.next_seq, 64 * 1024).await?;
        collected.next_seq = page.next_seq;
        collected.chunks.extend(page.chunks);
        if page.eof {
            break session.wait().await?;
        }
    };
    collected.eof = true;
    session.close().await?;
    Ok((exit, collected))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use nucleos_contract::{ExitReason, SessionSpec};

    struct Fixed;

    #[async_trait]
    impl Session for Fixed {
        fn id(&self) -> &str {
            "s1"
        }
        fn pid(&self) -> Option<u32> {
            Some(42)
        }
        async fn read(&self, after_seq: u64, _max: usize) -> Result<SessionOutput> {
            if after_seq >= 1 {
                return Ok(SessionOutput {
                    next_seq: 1,
                    chunks: Vec::new(),
                    eof: true,
                });
            }
            Ok(SessionOutput {
                next_seq: 1,
                chunks: vec![OutputChunk {
                    seq: 0,
                    stream: SessionStream::Stdout,
                    bytes: b"hello".to_vec(),
                }],
                eof: false,
            })
        }
        async fn write(&self, _bytes: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn close_input(&self) -> Result<()> {
            Ok(())
        }
        async fn terminate(&self) -> Result<()> {
            Ok(())
        }
        async fn wait(&self) -> Result<SessionExit> {
            Ok(SessionExit {
                code: Some(0),
                signal: None,
                reason: ExitReason::Exited,
            })
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    struct One;

    #[async_trait]
    impl Runner for One {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::default()
        }
        async fn start(&self, _spec: SessionSpec) -> Result<Box<dyn Session>> {
            Ok(Box::new(Fixed))
        }
        async fn list(&self) -> Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }
        async fn terminate(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn one_shot_is_a_session_drained() {
        let (exit, output) = exec(&One, "s1", vec!["echo".to_owned()], ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(exit.code, Some(0));
        assert_eq!(output.chunks.len(), 1);
        assert!(output.eof);
    }

    #[tokio::test]
    async fn a_runner_is_a_starter() {
        let starter: &dyn SessionStarter = &One;
        let spec = SessionSpec {
            id: "s1".to_owned(),
            ..SessionSpec::default()
        };
        assert_eq!(starter.start(spec).await.unwrap().id(), "s1");
    }
}
