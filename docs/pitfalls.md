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

**Decision.** Nothing mid-turn is persisted. A driver only writes state at a boundary it
chose.

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

## 9. The kernel does not know the transport

**Decision.** The kernel's types *are* the wire format: `contract.rs` derives `JsonSchema` beside
`Serialize`. What stays out of the kernel is the **transport** — `axum`, `hyper`, `tower`, sockets,
authentication — and a structural test fails if any of them appears in its manifest.

**Why the line moved.** It used to run between the kernel and the wire, and the price was a
623-line translation layer plus a second type tree. JSON has none of protobuf's odd corners
(`uint64` as a string, `oneof` as `Option`) that made the separation pay for itself, so the
cheaper line is the one between *what a message is* and *how it travels*.

**What it still buys.** A host that embeds the kernel inherits no web framework, and the
transport can be replaced without touching a single domain type.

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

## 15. The schema is committed, and a test fails when it drifts

**Decision.** `schema/v1/schema.json` is generated from the kernel's types and committed.
Regeneration is explicit (`NUCLEOS_SCHEMA=1 cargo test -p nucleos --test schema`), and the
default run only reads — so CI fails instead of quietly rewriting the file.

**Why.** Under C1 the kernel's serialized shape is a promise to every client, and a promise with
no artefact is a rumour. The committed schema is what an SDK generates from; the drift test is
what keeps it honest.

**What replaced the compiler.** `protoc` used to reject a circular import and `buf` used to catch
a breaking change; both are gone. In their place: a `$ref`-resolves test, a duplicate-definition
test, and the drift test. They are weaker than `buf breaking`, which is why §5 of [`wire.md`] is a
written rule rather than a tool.

**Failure it prevents.** A field renamed in Rust that silently renames itself on the wire, and an
SDK generated from a schema nobody regenerated.

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

## 20. An OS does not ship a model gateway

**Decision.** No stateless "invoke a model" endpoint. `Generate` was added, argued for, and
removed.

**Why.** The argument for it — normalization, routing, retries and cost accounting come as one
package — is an argument about *inside a session*, which is where all four already are. Outside a
session the same argument becomes a second product with its own scope: once `/generate` exists,
"why can't it embed?" has no answer except "yet".

**What it would have cost.** Coherence. The honest answer — embeddings are a tool, because an
embedding model belongs with the index that uses it — stops being sayable.

**The one thing it bought.** Cost attribution for calls a tool makes. That is fixed at the
smaller scale: [`ToolOutput::usage`] lets a tool report what it spent, and a tool that reports
nothing stays unreported.

**Failure it prevents.** A kernel whose endpoint list is a roadmap of modalities, each one
half-implemented.

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

## 24. Generate a schema from one root, not by merging roots

**Decision.** `nucleos::wire::schema` takes a single `schema_for!(Abi)` and publishes that.

**Why.** Merging a document per type looks equivalent and is not: schemars describes the same
reachable type twice — once as a root, once as another root's definition — and the two
descriptions are free to differ in ways nothing reconciles. The first attempt did exactly that
and died on "two different schemas share the name ToolResult".

**What it also fixed.** A recursive type's root is a `$ref` to its own definition, so inserting it
as a body collides with the definition already collected. One pass has one version of each type by
construction, and neither problem exists.

## 25. An `always` outside the ceiling is refused at the door

**Decision.** `configure` rejects an `always` entry naming a tool that `tools` does not allow; a
*tool's* request for such a name is silently dropped instead.

**Why.** The two are different acts. An application asking for a contradictory configuration has
made a mistake, and the error is the useful answer. A search tool asking is just a ranker doing its
job — it has no session, so it cannot know the ceiling — and turning one over-eager hit into a
failed call would punish the caller for the implementation's ignorance.

**What it prevents.** Showing the model a tool it may never execute. The model would call it, the
gate would refuse, and the refusal would be unexplainable from where the model sits: it was told
this tool exists.

## 26. A search tool never sees the session

**Decision.** `ToolOutput.expose` is a request the kernel answers; there is no `&mut Session` in
`ToolCtx`.

**Why.** A tool that could mutate its own visibility is a tool that can widen its own permissions,
and the ceiling becomes decoration. Keeping the widening at the kernel means there is exactly one
place that decides, and that place already knows the ceiling and writes the event.

**What it also buys.** The request is *data*, so it survives a batch: eight calls run, eight
requests come back, the kernel applies them once and records one event.

## 27. A single-character ASCII term is not a search term

**Decision.** Catalog queries drop one-character ASCII tokens after tokenizing; single CJK
characters stay.

**Why.** `a` and `1` appear in nearly every description, so a document frequency near the corpus
size makes them useless — and with prefix matching, every term starting with that letter is a
match too. One filler word in a query ("a tool that reads a file") would drag unrelated
definitions into the pool. A single CJK character is a whole word and is what the bigrams are
built from, so it is kept.

**Guarded by.** `tool_search` returns "query had no searchable words" rather than a ranking, so
the misfire is visible instead of looking like a bad index.

## 28. Everything is deferred, and the search tool is always

**Decision.** `ToolPolicy::default()` is `deferred`, and the kernel seeds `tool_search` to
`always` unless the caller configured it explicitly.

**Why.** The obvious default — show every tool — is the behaviour discovery exists to replace: the
twenty-ninth tool is as visible as the first, and the prompt pays for all of them on every call.
Deferring by default makes the catalog's size stop mattering.

**The trap in the other direction.** With everything deferred, the *only* way in is `tool_search`,
so hiding it silently turns the session into one that can see nothing and has no way to ask. A
policy that names it as anything but `always` is refused rather than obeyed.

## 29. A field nothing reads is a field that can lie

**Decision.** `Tool::isolation()` and `contract::Isolation` are deleted.

**Why.** They were a declaration with no consumer: nothing in the kernel read the value, and it
never reached the wire. The doc comment on the method described refusing to migrate a session into
a generation where isolation changed — a check that did not exist, because generations do not
exist yet.

**What it cost to keep.** Every tool implementor had to claim a level (the MCP source claimed
`Process`), and nothing verified the claim. A self-declaration that nobody reads is worse than
absent: it reads as a checked invariant. When generation migration is built, isolation returns as
something the *kernel* derives from how it spawned a source, not something a tool asserts about
itself.

## 30. No size threshold decides behaviour

**Decision.** The kernel never branches on how many tools are registered. Whether the search tool
appears follows from the policy alone.

**Why.** An earlier version skipped discovery below twelve tools: no `tool_search`, and the policy
not consulted at all. That quietly reversed the application's explicit configuration — a tool the
app declared `hidden` was shown, because the catalog happened to be small — and it made the same
configuration behave differently in development and in production, where more sources connect.

**The rule underneath.** A kernel may choose a *default*; it may not ignore a value the caller set.
If a small catalog should not pay for search, the application says so with `default: always`, and
the search tool disappears for a reason that is written down in the session's own state.

**What it costs.** A three-tool deployment that configures nothing does pay one schema and one round
trip for search. That is the price of the default being the safe one for scale, and it is visible
and adjustable — unlike the threshold, which was invisible and not.

## 31. A schema that does not fit is skipped, not the end of the list

**Decision.** The budget loop `continue`s on a byte overflow and `break`s only on a count overflow.

**Why.** Candidates are ordered by exposure rank and recency, never by size. One tool with a
gigabyte-scale schema early in that order would otherwise evict every smaller tool behind it — the
prompt would carry fewer tools than the budget allows, and the cause would be invisible from the
outside.

**Why the baseline is exempt.** `always` definitions are admitted whatever the budget says. An
application's declared baseline that disappears because discovery used the room is the configuration
being overridden by the mechanism meant to enforce it, and a prompt that loses its door has no way
to ask for it back.

## 32. `tools_exposed` reports the prompt, not the pool

**Decision.** The event carries `shown(after) - shown(before)`, computed after the pool is trimmed
and the budget applied.

**Why.** The two diverge exactly when the budget is full: a tool enters the pool, and the next
prompt cannot hold it. Reporting the pool would say the model can see something it cannot, and a
replay reading that event would reconstruct a prompt that never existed.

**What is still true of the difference.** A discovered tool the prompt omits remains callable by
name — the prompt is a view of the pool, not the whole of it. That is why the ceiling and the gate,
not the prompt, are what execution checks.

