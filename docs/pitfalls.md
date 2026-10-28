# Pitfalls

Each entry is a decision plus the failure it prevents. Written down because the same
mistake is cheap to make twice and expensive to undo.

## 1. A session is a process, not a conversation

**Decision.** Session owns resources: budget, tool generation, isolation, checkpoint.
Conversation is a stream inside it.

**Why.** Pausing a conversation for thirty minutes and resuming it raises questions that
must be answered explicitly — is the context retained, the temp directory, the token
quota? If the session *is* the conversation, the answer is implicit and usually wrong. If
the session owns resources, every one of those questions has a home.

**Failure it prevents.** "User reconnected and the quota was already spent", "resume
brought back a file handle that no longer exists".

## 2. Checkpoints only at turn boundaries

**Decision.** Nothing mid-turn is persisted. The engine never exposes a partially folded
state.

**Why.** A checkpoint is a promise: "replay from here is equivalent". That promise is only
keepable at a boundary where no call is in flight and no result is half-applied.

**Failure it prevents.** A restored session with a tool call that has no result, or two
results for one call, or a ledger that disagrees with the message history.

## 3. Budget is flow control, not a post-hoc check

**Decision.** Credit is granted by the caller and debited by `usage` events. Exhaustion
drains the session.

**Why.** A budget checked *before* a call cannot stop a call that overshoots. A budget
checked *after* only reports the damage. Credit makes the two the same operation.

**Failure it prevents.** A runaway loop that discovers it exceeded budget after the
invoice.

## 4. Four stop conditions, never one

**Decision.** `finish_reason`, step cap, wall clock, cost ledger — all four, all the time.

**Why.** Each is defeatable alone. A model that never emits `stop` defeats the first; a
provider that hangs defeats the second; a loop with no model call defeats the third; a
cheap model with huge token counts defeats the fourth.

## 5. Under-reporting cost is the unforgivable direction

**Decision.** A failed attempt that already streamed tokens is still charged. An
unreported counter stays unreported rather than being counted as zero.

**Why.** Over-reporting annoys someone. Under-reporting hides a runaway until the bill
arrives.

## 6. Tools are sources, not definitions on the wire

**Decision.** `Register` takes a transport descriptor. Tool specs are discovered over MCP.

**Why.** The moment a tool *definition* travels over the ABI, the ABI becomes a plugin
protocol — and then it needs code carrying, sandbox descriptors, version negotiation, hot
loading and crash isolation. That is a different product.

**Bonus.** Execution is in another process by construction, so a crashing tool is an
unhealthy source, not a crashed kernel.

## 7. Never swap tools inside a turn

**Decision.** Generations change only at turn boundaries, and only when compatible.

**Why.** A `tool_call` that has been issued must resolve against the generation that
issued it. Anything else produces results from a tool that no longer exists.

**Failure it prevents.** "The tool worked when the model called it" bugs, which are
unreproducible by definition.

## 8. Compatibility is structural, not numbered

**Decision.** Two generations are compatible when tool names match and input schemas are
structurally equal (new optional properties allowed) and the isolation descriptor is
identical.

**Why.** A version number is a human claim, and humans get it wrong. A schema diff is
machine-checkable and cannot lie about itself.

## 9. The kernel does not know the wire

**Decision.** `nucleos` has no dependency on `nucleos-proto`. Values that only exist on
the wire are translated in `nucleosd`.

**Why.** Every abstraction that exists because "the wire needs it" is a constraint the
kernel will carry forever. Keeping the dependency out is one line in a manifest and a
structural test; keeping it out later is a refactor.

## 10. No multi-bearer abstraction

**Decision.** One ABI, reached over gRPC. In-process use calls the library directly.

**Why.** Making in-process, child-process and remote callers present one interface forces
every method to the most conservative shape. The abstraction costs more than the three
implementations it replaces.

## 11. Reload is explicit

**Decision.** No filesystem watching, no SIGHUP. A caller invokes `Reload`.

**Why.** An automatic trigger turns a half-written configuration into a generation, and
then a session runs against a half-written configuration. Explicit reload costs one call.

## 12. Kernel configuration is not reloadable

**Decision.** Listener address, TLS material, authentication and the store connection
change only with a restart. Tool sources, model routing and default budgets reload.

**Why.** A half-old, half-new configuration is a state nobody can diagnose from the
outside. The split is drawn where a reload can be atomic.

## 13. Session migration is recorded, not inferred

**Decision.** A migration emits `generation_migrated{from, to}`.

**Why.** Replay must reproduce which tool set each turn ran against, or a replayed
incident cannot be trusted.

## 14. One writer per session

**Decision.** A session is driven by exactly one task. State is never handed out as
`&mut`.

**Why.** Locks on the hot path invite reentrancy, and reentrancy makes checkpointing
ambiguous — the writer cannot know whether a state is settled. Single-writer makes
"settled" mean "the task is at a boundary".

## 15. The schema's file layout is not the contract

**Decision.** Six `.proto` files, split by reason to change; imports run
`{common, content} ← {session, event}`, `common ← source`, everything `← kernel`.

**Why.** The compatibility unit is the `package` and the field numbers, not the file a
message sits in. That means the layout is free to reorganise — and it also means splitting
files buys no independent versioning, which is the thing people expect it to buy.

**What it does buy.** proto3 forbids circular imports, so `protoc` fails on a schema that
reaches upward. The import graph becomes a layering test we did not have to write, and an
event-only consumer can link `event.proto` without the session control surface.

**Failure it prevents.** One 450-line file where a diff that changes an event payload and a
diff that adds a method look identical to a reviewer, and where nothing stops the event
vocabulary from depending on session control.

## 16. A tool result is content, and failure is a flag

**Decision.** A tool returns `Content` plus `is_error` — not a JSON string, and not a
success/failure branch.

**Why content.** A tool returns *content*: text, an image, a reference. A string forces
every SDK to write a second renderer for text and makes an image-returning tool impossible
without a side channel. Reusing the content model means the result folds into history with
no adapter at all.

**Why a flag and not a branch.** A failed call must still carry content, because the error
text is the thing the model needs in order to recover; a `oneof` that put failure outside
the content model would leave the model with a message it cannot see. The cost is real —
`is_error` is a runtime flag a client can forget to check, where a `oneof` would have been
enforced by types.

**Why the nesting rule.** `Content` holds `Part`, and `Part` holds `ToolResult`, so the
schema is recursive and the runtime would be unbounded. A tool result's content may contain
any kind except a tool call or another tool result.

**Failure it prevents.** Two content renderers that drift apart; an agent that cannot read
its own tool's error; and a tool returning a tool call returning a tool call, discovered
when a renderer runs out of stack.

## 17. A delta must say which part it belongs to

**Decision.** Every streaming delta carries a `part_index`.

**Why.** A turn can emit text, then a tool call, then more text. Without an index the client
cannot tell a second block from a continuation of the first, so the stream could only
describe single-block messages — a limitation nobody notices until a model produces two text
blocks, and then it is a rendering bug that looks like a model bug.

## 18. Deltas are a rendering protocol; the record is separate

**Decision.** `message_completed` carries the assembled message. Deltas remain the
incremental view.

**Why.** Some content exists only after assembly — a reasoning signature arrives as its own
delta, and history must hold the whole block. Reassembling from deltas is also what a client
that joins mid-stream has to do, and it is the step every client gets subtly wrong.

**Failure it prevents.** A session whose stored history cannot be reproduced from its own
stream; and a second turn rejected by a provider because the signature a client forgot to
accumulate is now missing.

## 19. Seven shapes we deliberately refused

**Decision.** Each of these was considered and turned down. They are written here so the
next person does not have to consider them again.

| Refused | Why |
| --- | --- |
| Splitting a conversation id from a task id | Two ids means two lifecycles. The split exists to serve cross-organisation multi-agent orchestration, which is out of scope; one session is both. |
| A UI-shaped event vocabulary (state snapshots, JSON-Patch deltas) | Most of it is a client state machine. The kernel emits facts; deriving state from them is the client's job. |
| JSON-RPC over stdio as the transport | Copy semantics, not transports. stdio is optimised for being embedded in an editor; we serve long-lived sessions. |
| Versioning by header | The version lives in the path. A header version is the field every SDK eventually forgets to set. |
| A polling handle for long operations | The event stream already answers "is it done". A second mechanism is a second truth. |
| Offset/limit pagination | Offsets break when rows are created and retired, which is exactly what sessions do. |
| A second extension point | Two hatches means everyone uses the other one. `meta` is the only one. |

**Why this is written down.** Each is a plausible proposal that will be made again, usually
by somebody reasonable. A refusal without a reason is indistinguishable from an oversight,
and an oversight gets quietly adopted.

**Where the reason for an adopted shape lives.** In the `.proto` comment beside the field —
that is where a reader is standing when the question occurs to them. Not in a separate
document, and not in a list of precedents.

## 20. A gateway needs a per-call ceiling

**Decision.** `Generate` carries `max_micros_usd`; a call without one is refused unless the
daemon configured a default.

**Why.** Exposing model invocation over an ABI turns the daemon into a gateway, and a gateway
with no per-call limit is a cost leak with an API. The caller who forgets the limit is exactly
the caller who needed it — that is not an argument for trusting them.

**Failure it prevents.** A loop in somebody's script, a weekend, and a number nobody can
explain on Monday.

## 21. A field the provider cannot honour must be reported

**Decision.** `CapabilityDegraded` is emitted whenever a request field could not be carried,
and the attempt records it too.

**Why.** A `tool_choice = REQUIRED` that quietly became `AUTO` does not fail — it behaves
differently. The session then looks like a model that will not call tools, and the actual
cause is two layers below, in a capability nobody checked.

**Failure it prevents.** Debugging a prompt for an hour when the answer was in the adapter.

## 22. Retries belong to whoever keeps the ledger

**Decision.** Routing — chain, retries, fallback — lives in the kernel, and every attempt is
recorded with its usage.

**Why.** Split the two and one of them lies. A host that retries but does not record makes the
kernel's total wrong; a kernel that records but does not retry makes the total right and the
availability poor. One owner, one ledger.

**Failure it prevents.** A bill that does not match the attempts, and a "flaky" provider that
was never actually retried.

## 23. An attempt that produced content is never retried

**Decision.** A retry or a fallback happens only when the failed attempt emitted nothing. Once
a text or tool fragment has gone out, the turn fails instead.

**Why.** The fragments are already in the event log, and the log is the record. A retry after a
partial answer puts two half-answers in one turn's history, and no replay can tell which one the
model was supposed to be continuing. The cost of a failed turn is one turn; the cost of a
duplicated history is a transcript nobody can trust.

**What it costs.** A flaky provider that drops mid-stream is not silently papered over. That is
the point: the failure is visible, and the operator can see which provider dropped.
