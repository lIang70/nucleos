//! The process supervisor: the one writer of a process session.
//!
//! An agent session is written by whoever drives it, one call at a time. A process session cannot
//! be: its output arrives on a pipe, whenever the program decides to write. So the supervisor owns
//! that session's event log outright — it holds the sequence counter, appends every chunk, and is
//! the only thing that does. That is what keeps "one writer per session" true even though a
//! process is not a function call.
//!
//! It is also why a process session's checkpoint is a *record*, not a resumable state. The log
//! says how the command was started and what came out; it cannot say what is in the process's
//! memory. A supervisor that outlived a restart would have to admit the process is gone, and
//! [`ProcessStatus::Lost`] is that admission.

use super::{Kernel, Session, SessionSpec};
use crate::contract::{
    EventKind, ExecOutput, ExecStream, OutputChunk, ProcessSpec, ProcessState, ProcessStatus,
    SandboxPolicy, SessionId,
};
use crate::contract::{Principal, StopReason};
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

/// How much of one stream is kept before the rest is dropped.
///
/// Not a memory limit — a *context* limit: this text goes back to a model, and a command that
/// prints a hundred megabytes has to be shortened somewhere.
const OUTPUT_BUDGET_BYTES: usize = 64 * 1024;

/// How many output chunks one append carries.
///
/// Eight reads of 8 KiB is 64 KiB per commit: a chatty build costs a handful of transactions instead
/// of one per read, and the events stay small enough to stream.
const OUTPUT_BATCH: usize = 8;

/// ...and how long output may wait, so a command that prints a line and then thinks still shows it.
const OUTPUT_FLUSH_AFTER: std::time::Duration = std::time::Duration::from_millis(8);

/// What the supervisor can tell a running process to do.
enum Control {
    /// Write to its stdin.
    ///
    /// The only thing that queues. A kill is a `watch` signal beside this, because a kill that waits
    /// behind a stalled write is not a kill.
    Stdin(String),
}

/// What a process wrote, kept so that `wait` can answer with it.
///
/// The event stream already carries this text incrementally, for a client watching it happen. A
/// caller that waits wants the result instead of a reassembly job, and an exit code with no output
/// is not a result.
#[derive(Debug, Default)]
struct Captured {
    stdout: String,
    stderr: String,
    truncated_after_lines: Option<u32>,
}

/// One running process: a mailbox, and the exit it will eventually report.
struct Running {
    control: mpsc::Sender<Control>,
    /// A kill is a state change, not a message.
    ///
    /// It used to be `Control::Kill` in the same bounded queue as stdin writes, so a child that
    /// stopped reading its input filled the queue and the kill sat behind four hundred stalled
    /// chunks — unkillable exactly when killing is what you want. A `watch` send never blocks and
    /// never queues; the value only says "stop".
    kill: watch::Sender<bool>,
    /// The pid, kept so a listing can report it without touching the child.
    pid: Option<u32>,
    /// Taken by the first `wait`; `None` afterwards, so two waits cannot both consume one exit.
    exited: Option<oneshot::Receiver<ExecOutput>>,
}

/// The processes this kernel is supervising.
///
/// In memory on purpose: a process is not durable state. If the daemon stops, these are gone and
/// the sessions that own them say `Lost`.
#[derive(Default)]
pub(crate) struct Supervisor {
    processes: Mutex<BTreeMap<String, Running>>,
}

/// What a caller gets back when it starts a process.
#[derive(Debug, Clone)]
pub(crate) struct Spawned {
    /// The pid, when the platform gave one.
    pub pid: Option<u32>,
}

/// Everything a launch needs that the supervisor does not own.
pub(crate) struct Launch<'a> {
    pub store: Arc<dyn crate::adapter::Store>,
    pub session: &'a SessionId,
    pub tenant: &'a str,
    pub spec: &'a ProcessSpec,
    pub workspace: Option<&'a String>,
    pub sandbox: &'a SandboxPolicy,
    /// The session's log head: the supervisor is a second writer to the same log, and it must be the
    /// same counter and the same lock or the two writers renumber each other.
    pub head: Arc<super::log::LogHead>,
    pub clock: Arc<dyn crate::adapter::Clock>,
}

impl Supervisor {
    /// Start a process for a session, and write its events as they happen.
    pub(crate) async fn spawn(&self, launch: Launch<'_>) -> Result<Spawned> {
        let Launch {
            store,
            session,
            tenant,
            spec,
            workspace,
            sandbox,
            head,
            clock,
        } = launch;
        if spec.argv.is_empty() {
            return Err(Error::invalid("a process needs an argv"));
        }
        // No mechanism, no promise: this refuses rather than running unconfined, so a policy that
        // says "confined" is never quietly a policy that does nothing.
        let confinement = super::sandbox::confine(sandbox, &spec.permissions, workspace)?;

        let mut command = match &confinement {
            Some(confinement) => {
                // The mechanism wraps the command rather than replacing it: the supervisor keeps
                // owning one child, and the child happens to be `sandbox-exec`.
                let mut command = Command::new(&confinement.program);
                command
                    .args(&confinement.prefix)
                    .arg(&spec.argv[0])
                    .args(&spec.argv[1..]);
                command
            }
            None => {
                let mut command = Command::new(&spec.argv[0]);
                command.args(&spec.argv[1..]);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A child must not outlive the kernel's interest in it, and a killed kernel must not
            // leave a process nobody can reap.
            .kill_on_drop(true);
        if let Some(cwd) = spec.cwd.clone().or_else(|| workspace.cloned()) {
            command.current_dir(cwd);
        }
        command.envs(&spec.env);

        let mut child = command.spawn().map_err(|error| {
            Error::unavailable(format!("cannot start {:?}: {error}", spec.argv))
        })?;
        let pid = child.id();

        // The counter starts where the session's log ends: this task is the only writer from here.
        let log = super::writer::EventWriter::new(
            Arc::clone(&store),
            crate::adapter::SessionKey {
                tenant: tenant.to_owned(),
                id: session.clone(),
            },
            // The log's head, not a copy: this task and the caller both append to one log.
            Arc::clone(&head),
            clock,
        );
        let captured = Arc::new(Mutex::new(Captured::default()));
        let (drained_tx, drained_rx) = mpsc::channel::<()>(2);
        let (exited_tx, exited_rx) = oneshot::channel();

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(pipe) = stdout {
            tokio::spawn(reader(
                log.clone(),
                Arc::clone(&captured),
                drained_tx.clone(),
                ExecStream::Stdout,
                pipe,
            ));
        }
        if let Some(pipe) = stderr {
            tokio::spawn(reader(
                log.clone(),
                Arc::clone(&captured),
                drained_tx.clone(),
                ExecStream::Stderr,
                pipe,
            ));
        }

        let (control_tx, control_rx) = mpsc::channel(64);
        let (kill_tx, kill_rx) = watch::channel(false);
        tokio::spawn(supervise(
            child,
            log.clone(),
            Arc::clone(&captured),
            control_rx,
            kill_rx,
            drained_rx,
            exited_tx,
        ));

        self.processes.lock().await.insert(
            session.as_str().to_owned(),
            Running {
                control: control_tx,
                kill: kill_tx,
                pid,
                exited: Some(exited_rx),
            },
        );

        log.append(EventKind::ExecStarted {
            pid: pid.unwrap_or(0),
            argv: spec.argv.clone(),
            policy: sandbox.clone(),
            // What actually happened, next to what was asked for. A policy in the checkpoint plus a
            // refusal in the output is an unexplainable pair without this.
            mechanism: confinement
                .as_ref()
                .map(|confinement| confinement.mechanism.to_owned()),
            profile: confinement
                .as_ref()
                .and_then(|confinement| confinement.profile.clone()),
        })
        .await?;

        Ok(Spawned { pid })
    }

    /// Write to a process's stdin.
    pub(crate) async fn write_stdin(&self, session: &SessionId, chunk: &str) -> Result<()> {
        // The sender is cloned out and the map released before the send. `send` waits when the
        // channel is full, and the channel fills exactly when the child is not reading its stdin — at
        // which point the map lock must not be held, or `signal` blocks behind the writer and nothing
        // on this node can be killed.
        let control = {
            let processes = self.processes.lock().await;
            processes
                .get(session.as_str())
                .ok_or_else(|| Error::not_found(format!("no process for session {session}")))?
                .control
                .clone()
        };
        control
            .send(Control::Stdin(chunk.to_owned()))
            .await
            .map_err(|_| Error::unavailable("the process has already ended"))
    }

    /// Ask a process to end.
    pub(crate) async fn signal(&self, session: &SessionId) -> Result<()> {
        // A kill is a state change, not a queued message: a child that stopped reading its input
        // fills the stdin queue, and a kill behind it is a kill that never arrives. `send` on a
        // watch cannot block, so this returns whether or not the supervisor is stuck.
        let kill = {
            let processes = self.processes.lock().await;
            processes
                .get(session.as_str())
                .ok_or_else(|| Error::not_found(format!("no process for session {session}")))?
                .kill
                .clone()
        };
        kill.send(true)
            .map_err(|_| Error::unavailable("the process has already ended"))
    }

    /// Wait for a process to end, and say how it ended.
    pub(crate) async fn wait(&self, session: &SessionId) -> Result<ExecOutput> {
        // The exit handle is taken, not the entry. Removing the entry first made a client that
        // disconnected mid-`wait` orphan a live process: `signal`, `stdin` and `state` all answered
        // "no process" while the child kept running.
        let exited = {
            let mut processes = self.processes.lock().await;
            processes
                .get_mut(session.as_str())
                .ok_or_else(|| Error::not_found(format!("no process for session {session}")))?
                .exited
                .take()
                .ok_or_else(|| {
                    Error::unavailable(format!("session {session} is already being waited on"))
                })?
        };
        let output = exited
            .await
            .map_err(|_| Error::internal("the process ended without reporting an exit"))?;
        // Reaped, so the entry has nothing left to report; a second `wait` finds the log instead.
        self.processes.lock().await.remove(session.as_str());
        Ok(output)
    }

    /// The processes this supervisor is holding, in session order.
    ///
    /// What it answers is "what is running on this node right now". A process session that outlived
    /// a restart is *not* here — its own view says `Lost` — because this map is in memory and a
    /// process is not durable state.
    pub(crate) async fn list(&self) -> Vec<(SessionId, Option<u32>)> {
        let processes = self.processes.lock().await;
        processes
            .iter()
            .map(|(session, running)| (SessionId::new(session.clone()), running.pid))
            .collect()
    }

    /// Whether the supervisor still holds this session, and where that leaves it.
    pub(crate) async fn state(
        &self,
        session: &SessionId,
        spec: &ProcessSpec,
        pid: Option<u32>,
    ) -> ProcessState {
        let alive = self.processes.lock().await.contains_key(session.as_str());
        ProcessState {
            spec: spec.clone(),
            status: match (alive, pid) {
                (true, Some(pid)) => ProcessStatus::Running { pid },
                // Not held and never exited through us: the supervisor lost it (a restart, a
                // crash). `Lost` is the honest answer — the alternative is a session waiting
                // forever on a process that is already gone.
                _ => ProcessStatus::Lost,
            },
        }
    }
}

/// One task owns the child: it reads the pipes, writes stdin, kills, reaps, and appends.
///
/// Ownership is the point. Two things touching `Child` at once is what makes a supervisor
/// thread-unsafe; one task with a mailbox is what makes it a single writer for the session.
async fn supervise(
    mut child: Child,
    log: super::writer::EventWriter,
    captured: Arc<Mutex<Captured>>,
    mut control: mpsc::Receiver<Control>,
    mut kill: watch::Receiver<bool>,
    mut drained: mpsc::Receiver<()>,
    exited: oneshot::Sender<ExecOutput>,
) {
    let started = std::time::Instant::now();
    let mut stdin = child.stdin.take();

    // Reap and answer control messages at once: a process that is waiting for input still has to
    // be killable.
    let status = loop {
        tokio::select! {
            message = control.recv() => match message {
                Some(Control::Stdin(chunk)) => {
                    // Recorded before it is sent: if the write fails halfway, the log still says
                    // what we tried to run, which is the part an audit needs.
                    let _ = log
                        .append(EventKind::ExecStdin {
                            chunk: chunk.clone(),
                        })
                        .await;
                    if let Some(stdin) = stdin.as_mut() {
                        // The write races the kill. `write_all` waits for a pipe that a child which
                        // is not reading will never drain, and while it waits this loop is not
                        // selecting — so a kill sent during it would be observed only after the
                        // child exited on its own, which is the bug this test caught.
                        tokio::select! {
                            _ = stdin.write_all(chunk.as_bytes()) => {
                                let _ = stdin.flush().await;
                            }
                            // A change is *consumed* by `changed()`: interrupting the write without
                            // acting on it would leave the outer branch waiting for a change that has
                            // already been seen, and the child would run to completion unkilled.
                            _ = kill.changed() => {
                                if *kill.borrow() {
                                    let _ = child.start_kill();
                                }
                            }
                        }
                    }
                }
                None => {}
            },
            changed = kill.changed() => {
                // `Err` means the sender is gone, which is the supervisor being torn down; the child
                // dies with it (`kill_on_drop`).
                if changed.is_ok() && *kill.borrow() {
                    let _ = child.start_kill();
                }
                let _ = changed;
            }
            reaped = child.wait() => break reaped,
        }
    };

    // A process that has exited has not necessarily been *read*. Wait for both pipes to reach EOF,
    // or `wait` answers with an exit code and half the output.
    //
    // Bounded, though. A grandchild that inherited stdout (`sh -c 'sleep 100 &'`) keeps the pipe open
    // long after the child is gone, and an unbounded wait turns "the process exited" into a request
    // that never returns. Everything within the output budget has been read by then; the rest is
    // being discarded anyway.
    drop(control);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while drained.recv().await.is_some() {}
    })
    .await;

    let captured = captured.lock().await;
    let output = ExecOutput {
        exit_code: status
            .as_ref()
            .map_or(-1, |status| status.code().unwrap_or(-1)),
        duration_millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        timed_out: false,
        stdout: OutputChunk {
            text: captured.stdout.clone(),
            truncated_after_lines: captured.truncated_after_lines,
        },
        stderr: OutputChunk {
            text: captured.stderr.clone(),
            truncated_after_lines: captured.truncated_after_lines,
        },
    };
    drop(captured);

    // The record is the events, so the exit event carries the summary and leaves the text where it
    // already is: a log that repeats sixty-four kilobytes is a log with two truths.
    let mut summary = output.clone();
    summary.stdout.text.clear();
    summary.stderr.text.clear();
    let _ = log
        .append(EventKind::ExecExited {
            output: summary.clone(),
        })
        .await;
    // Close the session here, not in the request path: the process ended, and a session that stays
    // `open` for a command that is gone is a session nobody can list honestly.
    let _ = log
        .append(EventKind::StateChanged {
            state: crate::contract::SessionState::Closed,
            stop: Some(crate::contract::StopReason::Completed),
        })
        .await;
    // Read, mutate, write — retried, because the checkpoint is versioned and a request may have
    // advanced it while this task was reading. Three attempts is enough for a race of exactly two
    // writers; a persistent conflict means something else is wrong and a log line is the honest
    // answer rather than a lost exit.
    for _ in 0..3 {
        let Ok(Some(mut snapshot)) = log.store.snapshot(&log.session).await else {
            break;
        };
        snapshot.state = crate::contract::SessionState::Closed;
        snapshot.seq = log.head.next().saturating_sub(1);
        snapshot.version = snapshot.version.saturating_add(1);
        if let Some(process) = snapshot.process.as_mut() {
            process.status = crate::contract::ProcessStatus::Exited {
                output: summary.clone(),
            };
        }
        match log.store.write_snapshot(snapshot).await {
            Ok(()) => break,
            Err(error) if error.code() == crate::contract::ErrorCode::Conflict => continue,
            Err(error) => {
                tracing::warn!(event = "exit_checkpoint_failed", detail = %error.reason());
                break;
            }
        }
    }
    let _ = exited.send(output);
}

async fn reader(
    log: super::writer::EventWriter,
    captured: Arc<Mutex<Captured>>,
    drained: mpsc::Sender<()>,
    stream: ExecStream,
    mut pipe: impl AsyncReadExt + Unpin,
) {
    let mut buffer = [0u8; 8192];
    let mut written = 0usize;
    // Output is batched, like the model's fragments: a command that prints a megabyte should not cost
    // a commit per 8 KiB read. The buffer is bounded by count and by time, so a slow trickle is still
    // streamed, and EOF flushes whatever is left.
    let mut pending: Vec<EventKind> = Vec::new();
    let mut last_flush = std::time::Instant::now();
    loop {
        let read = tokio::time::timeout(OUTPUT_FLUSH_AFTER, pipe.read(&mut buffer)).await;
        let chunk = match read {
            // A pause: flush what is buffered, then keep waiting. A command that prints one line and
            // then thinks must not hold that line until it exits.
            Err(_elapsed) => {
                if !pending.is_empty() && log.append_batch(&pending).await.is_err() {
                    break;
                }
                pending.clear();
                last_flush = std::time::Instant::now();
                continue;
            }
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(read)) => {
                if written >= OUTPUT_BUDGET_BYTES {
                    continue; // drain the pipe; the budget is spent
                }
                let take = read.min(OUTPUT_BUDGET_BYTES - written);
                written += take;
                String::from_utf8_lossy(&buffer[..take]).into_owned()
            }
        };
        {
            let mut captured = captured.lock().await;
            match stream {
                ExecStream::Stdout => captured.stdout.push_str(&chunk),
                ExecStream::Stderr => captured.stderr.push_str(&chunk),
            }
        }
        pending.push(EventKind::ExecOutput { stream, chunk });
        if pending.len() >= OUTPUT_BATCH || last_flush.elapsed() >= OUTPUT_FLUSH_AFTER {
            if log.append_batch(&pending).await.is_err() {
                break;
            }
            pending.clear();
            last_flush = std::time::Instant::now();
        }
    }
    // EOF or a failed append: whatever is left goes out, so the log ends with the output the process
    // actually produced rather than with a gap a reader has to explain.
    if !pending.is_empty() {
        let _ = log.append_batch(&pending).await;
    }
    // EOF: every byte this stream will ever produce has been appended.
    let _ = drained.send(()).await;
}

impl Kernel {
    /// A page of the exec sessions this node knows about, and where each one is.
    ///
    /// The store says which sessions exist and answers the cheap half — kind, order, page. The
    /// supervisor says which are alive. Neither can answer alone: their disagreement *is* the `Lost`
    /// case, a session in the store that the supervisor has never heard of.
    ///
    /// A phase filter is applied here, so a page can be filled from more than one store page. That
    /// is the price of filtering on a status only the kernel can compute; it is not a price a store
    /// could pay by indexing.
    pub async fn processes(
        &self,
        tenant: &str,
        query: &crate::contract::ProcessQuery,
    ) -> Result<crate::contract::ProcessPage> {
        let page_size = query.page_size();
        let mut page = crate::contract::ProcessPage::default();
        let mut after = query.after.clone();
        let live = self.supervisor.list().await;

        loop {
            let store_page = self
                .store
                .sessions(&crate::adapter::SessionQuery {
                    tenant: tenant.to_owned(),
                    // The process listing is per tenant, not per subject: a process session is a
                    // supervisor's handle on a child, and which operator started it does not narrow
                    // what the node is running.
                    subject: None,
                    kind: Some(crate::contract::SessionKind::Process),
                    limit: page_size,
                    after: after.clone(),
                })
                .await?;

            let total = store_page.sessions.len();
            for (position, session) in store_page.sessions.into_iter().enumerate() {
                let key = crate::adapter::SessionKey {
                    tenant: tenant.to_owned(),
                    id: session.clone(),
                };
                // The head, not the whole session: a process listing needs `process`, and reading the
                // snapshot would reassemble every message and every attempt of every session on the
                // page to answer it.
                let Some(snapshot) = self.store.snapshot_head(&key).await? else {
                    continue;
                };
                let Some(process) = snapshot.process.clone() else {
                    continue;
                };
                let status = status_of(&session, &process, &live);
                if query.phase.is_some_and(|phase| phase != status.phase()) {
                    continue;
                }
                page.processes.push((
                    session.clone(),
                    crate::contract::ProcessState {
                        spec: process.spec,
                        status,
                    },
                ));
                if page.processes.len() >= page_size {
                    // The cursor is the last row *examined*, not the end of the store's page: a
                    // page that filled early would otherwise skip every row it never looked at.
                    page.next = if position + 1 == total {
                        store_page.next.clone()
                    } else {
                        Some(session)
                    };
                    return Ok(page);
                }
            }

            match store_page.next {
                Some(next) => after = Some(next),
                None => return Ok(page),
            }
        }
    }

    /// Start a process session and the command it runs.
    ///
    /// The supervisor becomes this session's only writer: everything the process produces is
    /// appended by the task that reads its pipes, with a counter the supervisor owns.
    pub async fn spawn(
        &self,
        principal: &Principal,
        id: SessionId,
        spec: SessionSpec,
    ) -> Result<Session> {
        let spec = SessionSpec {
            kind: crate::contract::SessionKind::Process,
            ..spec
        };
        let mut session = self.open(principal, id, spec).await?;
        let process = session
            .process
            .clone()
            .ok_or_else(|| Error::internal("a process session opened without a process"))?;

        // The gate runs before anything starts: an escalation is a question, not a formality.
        match self
            .policy()
            .check(crate::adapter::PolicyRequest {
                principal: &session.principal,
                session: Some(&session.id),
                subject: crate::adapter::Gated::Exec {
                    argv: &process.spec.argv,
                    policy: &session.sandbox,
                    permissions: &process.spec.permissions,
                },
            })
            .await
        {
            crate::adapter::Verdict::Allow => {}
            crate::adapter::Verdict::Deny { code, reason } => {
                self.close(&mut session, StopReason::Failed).await?;
                return Err(Error::new(code, reason));
            }
            crate::adapter::Verdict::AskHuman { question, .. } => {
                self.close(&mut session, StopReason::Cancelled).await?;
                // A human decision on a command that has not started is a refusal until somebody
                // says otherwise; the approval flow lands with the daemon.
                return Err(Error::denied(format!(
                    "a human must approve this: {question}"
                )));
            }
        }

        let started = self
            .supervisor
            .spawn(Launch {
                store: Arc::clone(&self.store),
                session: &session.id,
                spec: &process.spec,
                workspace: session.workspace.as_ref(),
                sandbox: &session.sandbox,
                head: session.log_head(),
                tenant: &session.principal.tenant,
                clock: Arc::clone(&self.clock),
            })
            .await?;

        session.process = Some(
            self.supervisor
                .state(&session.id, &process.spec, started.pid)
                .await,
        );
        Ok(session)
    }
    /// Write to a running process's stdin.
    pub async fn write_stdin(&self, session: &Session, chunk: &str) -> Result<()> {
        self.supervisor.write_stdin(&session.id, chunk).await
    }
    /// Ask a running process to end.
    pub async fn signal(&self, session: &Session) -> Result<()> {
        self.supervisor.signal(&session.id).await
    }
    /// Wait for a process to end, and say how it ended.
    /// Wait for a process, or answer for one this runtime never saw.
    ///
    /// A process session outlives the process: `kill -9`, redeploy, resume. The supervisor is
    /// memory, so after a restart it holds nothing — but the *answer* is in the log, in the chunks
    /// the reader wrote and the summary the supervisor appended when the child exited. Rebuilding it
    /// is the difference between "the session resumed" and "the session resumed and cannot tell you
    /// what it did".
    pub async fn wait(&self, session: &Session) -> Result<crate::contract::ExecOutput> {
        match self.supervisor.wait(&session.id).await {
            Ok(output) => Ok(output),
            Err(error) if error.code() == crate::contract::ErrorCode::NotFound => {
                self.wait_from_log(session).await
            }
            Err(error) => Err(error),
        }
    }

    /// The output of a process that ended before this runtime started.
    async fn wait_from_log(&self, session: &Session) -> Result<crate::contract::ExecOutput> {
        let events = self.store.events_since(&session.key(), 0).await?;

        // The summary is the exit event's, because that is the only place the exit status, the
        // duration and the truncation count are written down.
        let summary = events.iter().rev().find_map(|event| match &event.kind {
            EventKind::ExecExited { output } => Some(output.clone()),
            _ => None,
        });
        let Some(mut output) = summary.or_else(|| {
            session
                .process
                .as_ref()
                .and_then(|process| match &process.status {
                    crate::contract::ProcessStatus::Exited { output } => Some(output.clone()),
                    _ => None,
                })
        }) else {
            // Running, pending, or lost without an exit: the log has no ending to report, and
            // inventing one — an exit code of zero, an empty stdout — is the failure this method
            // exists to avoid.
            return Err(Error::unavailable(format!(
                "session {} has no recorded exit: the process either outlived this runtime or is \
                 still running, so nothing can be said about how it ended",
                session.id
            )));
        };

        // The text is in the stream, chunk by chunk, in the order the reader wrote it. The exit
        // event deliberately carries no text — repeating sixty-four kilobytes would be two truths —
        // so this is where the two halves are put back together.
        for event in &events {
            let EventKind::ExecOutput { stream, chunk } = &event.kind else {
                continue;
            };
            let target = match stream {
                crate::contract::ExecStream::Stdout => &mut output.stdout,
                crate::contract::ExecStream::Stderr => &mut output.stderr,
            };
            target.text.push_str(chunk);
        }
        Ok(output)
    }
}

/// Where a process is, from the two sources that each know half of it.
fn status_of(
    session: &SessionId,
    process: &crate::contract::ProcessState,
    live: &[(SessionId, Option<u32>)],
) -> crate::contract::ProcessStatus {
    match live.iter().find(|(id, _)| id == session) {
        Some((_, Some(pid))) => crate::contract::ProcessStatus::Running { pid: *pid },
        Some((_, None)) => crate::contract::ProcessStatus::Pending,
        None => match &process.status {
            // The checkpoint recorded an exit: it ended, and we know how.
            crate::contract::ProcessStatus::Exited { output } => {
                crate::contract::ProcessStatus::Exited {
                    output: output.clone(),
                }
            }
            // It never started, or it outlived the runtime. Either way nothing is running, and
            // claiming `Pending` would make it look like it is about to.
            _ => crate::contract::ProcessStatus::Lost,
        },
    }
}
