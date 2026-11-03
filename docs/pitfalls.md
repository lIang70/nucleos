# Pitfalls

Each entry is a decision plus the failure it prevents. Written down because the same
mistake is cheap to make twice and expensive to undo.

## 1. A session is a process, not a conversation

**Decision.** Session owns resources: budget, tool generation, isolation, checkpoint.
Conversation is a stream inside it.

**Why.** Pausing a conversation for thirty minutes and resuming it raises questions that
must be answered explicitly — is the context retained, the temp directory, the token
quota? If the session _is_ the conversation, the answer is implicit and usually wrong. If
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

**Why.** A budget checked _before_ a call cannot stop a call that overshoots. A budget
checked _after_ only reports the damage. Credit makes the two the same operation.

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

**Why.** The moment a tool _definition_ travels over the ABI, the ABI becomes a plugin
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

**Decision.** The kernel's types _are_ the wire format: `contract.rs` derives `JsonSchema` beside
`Serialize`. What stays out of the kernel is the **transport** — `axum`, `hyper`, `tower`, sockets,
authentication — and a structural test fails if any of them appears in its manifest.

**Why the line moved.** It used to run between the kernel and the wire, and the price was a
623-line translation layer plus a second type tree. JSON has none of protobuf's odd corners
(`uint64` as a string, `oneof` as `Option`) that made the separation pay for itself, so the
cheaper line is the one between _what a message is_ and _how it travels_.

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

**Why content.** A tool returns _content_: text, an image, a reference. A string forces
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

| Refused                                                           | Why                                                                                                                                               |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Splitting a conversation id from a task id                        | Two ids means two lifecycles. The split exists to serve cross-organisation multi-agent orchestration, which is out of scope; one session is both. |
| A UI-shaped event vocabulary (state snapshots, JSON-Patch deltas) | Most of it is a client state machine. The kernel emits facts; deriving state from them is the client's job.                                       |
| JSON-RPC over stdio as the transport                              | Copy semantics, not transports. stdio is optimised for being embedded in an editor; we serve long-lived sessions.                                 |
| Versioning by header                                              | The version lives in the path. A header version is the field every SDK eventually forgets to set.                                                 |
| A polling handle for long operations                              | The event stream already answers "is it done". A second mechanism is a second truth.                                                              |
| Offset/limit pagination                                           | Offsets break when rows are created and retired, which is exactly what sessions do.                                                               |
| A second extension point                                          | Two hatches means everyone uses the other one. `meta` is the only one.                                                                            |

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
package — is an argument about _inside a session_, which is where all four already are. Outside a
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
_tool's_ request for such a name is silently dropped instead.

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

**What it also buys.** The request is _data_, so it survives a batch: eight calls run, eight
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

**The trap in the other direction.** With everything deferred, the _only_ way in is `tool_search`,
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
absent: it reads as a checked invariant.

**Where the concept went.** Generation compatibility still has to compare it — same name, same
schema, weaker isolation is a silent permission downgrade that every other check passes. It returns
then, _derived_ by the daemon from how it reached the source, and it is not a tool's to declare: a
tool describing its own isolation is the claim the check was supposed to be verifying.

## 30. No size threshold decides behaviour

**Decision.** The kernel never branches on how many tools are registered. Whether the search tool
appears follows from the policy alone.

**Why.** An earlier version skipped discovery below twelve tools: no `tool_search`, and the policy
not consulted at all. That quietly reversed the application's explicit configuration — a tool the
app declared `hidden` was shown, because the catalog happened to be small — and it made the same
configuration behave differently in development and in production, where more sources connect.

**The rule underneath.** A kernel may choose a _default_; it may not ignore a value the caller set.
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

## 33. A fake in the production crate becomes the deployment

**Decision.** The test doubles live in `nucleos-testkit`, a crate no binary depends on. The store
is `nucleos-sqlite`, the same implementation in a test as in production, with `in_memory()` as the
only difference. `AllowAll` and `DenyAll` stay in the kernel, because they are policies with a
meaning rather than substitutes for one.

**Why.** The daemon had reached for `MemoryStore` and `AllowAll` — the fakes — as its actual
configuration. Nothing in the build said so: `POST /v1/sessions` succeeded, and the session was gone
after a restart, which from the caller's side is indistinguishable from data loss. A crate boundary
makes that mistake a compile error instead of a silent one.

**What it costs.** `in_memory()` opens a real database, so a test that assumed `MemoryStore`'s
leniency finds out sooner. That is the point: the store rejects a sequence gap, refuses a snapshot
claiming more events than the log holds, and does it identically in both places.

## 34. The store is the deployment, so it is tested as one

**Decision.** `SqliteStore` is exercised by reopening the file: a kernel writes a turn, the process
holding it is dropped, a second kernel on the same path loads the session.

**Why.** Durability is not a property a unit test of `append` can observe. The failure it prevents
is the one that made this change necessary — a store that answers every call and keeps nothing —
and the only way to see it is to stop pretending the process stays alive.

## 35. A policy evaluates deny first, and is not a language

**Decision.** `nucleos-policy` reads a JSON document: rules match by tool name, model name or
program, and the evaluation order is `deny` → `ask` → `allow` → the document's `default`. There is
no expression language, no priorities, no time windows.

**Why deny first.** Ordering by decision rather than by line means moving a rule in the file cannot
widen access. A deployment can make a policy stricter by adding a `deny` and nothing else; the
reverse requires editing the rule that allowed it, which is what a reviewer looks at.

**Why not a language.** Every policy language grows until it is a program nobody reviews, and its
bugs are silent in the permissive direction. The kernel already has a place for programs: the host
that implements `Policy`.

**Why commands are matched as arguments, not text.** `program` is compared to the basename of
`argv[0]` and `argv_prefix` to the leading arguments. There is no shell between the policy and the
process, so there is no command-line syntax to parse — and therefore no parser that disagrees with
the one that spawns.

**Why the daemon refuses to start without a policy.** An absent policy that silently means "allow"
is the failure this replaced: the deployment believed it had a gate. `"default": "allow"` costs one
line and is reviewable; forgetfulness costs nothing and is invisible.

## 36. A reconnected source may not change its tools

**Decision.** Tools are pinned at connect time; a restart that advertises a different set is refused
with "that is a new generation, not a reconnect".

**Why.** A session is pinned to a generation, and the generation describes these declarations. If a
restart silently adopted a different set, the session would be calling names its prompt never
offered, and the record would describe tools that no longer exist. Making the failure loud keeps the
two mechanisms apart: reconnection restores a process, generations change a catalog.

## 37. A crashed call is reported, not retried

**Decision.** A transport failure marks the connection dead, rebuilds it, and returns failure
content with the reason. The retry, if any, is the model's next call.

**Why.** A tool call is not assumed idempotent, and a closed pipe does not say whether the server ran
the call first. Silently retrying turns one ambiguous write into two writes — the class of bug where
the log says one thing and the database says another. The model can read "the connection was
restored, so this call can be retried" and decide.

## 38. Backoff is part of the source, not of the caller

**Decision.** Failed restarts double a delay from 250ms to 30s, and the window is shared through the
same mutex that serialises reconnects.

**Why.** Without it, a source whose command was deleted is spawned once per call for the life of the
session — a fork bomb that looks like an unhealthy tool. Without the mutex, a batch of eight
concurrent calls to a dead source produces eight children and seven orphans.

## 39. Compaction is not a kernel feature, and neither is recording it

**Decision.** The kernel measures a prompt and refuses one that cannot fit. It does not summarise,
window, evict or rank — and it does not accept a `history_compacted` event either. That syscall was
added and then deleted.

**Why the syscall was wrong.** The kernel cannot verify it. `from`/`to` index the caller's message
list, which the kernel has never seen; the summary is the caller's word for what it sent; and nothing
prevents reporting a compaction that did not happen. That is a claim in the log that claims to be the
record — the same defect as `Tool::isolation` and as `tools_exposed` reporting the pool instead of
the prompt.

**Why deleting it costs nothing.** Replay rebuilds the session from `messages`, which the kernel owns
and a caller's compaction never touches. Audit of "what the model was told" belongs to whatever built
the prompt, because only that thing can say it truthfully.

**Why there is no generic emit-my-own-event syscall.** A caller who could emit arbitrary events could
forge `budget_exhausted`, and every later replay would be wrong. `compact` was a back door around
that prohibition, aimed at exactly one event kind.

**What the kernel still owes.** Two things a caller cannot do for itself: `count_tokens` before the
call — the contract method that existed while nothing called it — and a refusal that names the
numbers, so the caller knows whether to compact or to lower the reserve.

## 40. A model with no declared window is not unlimited

**Decision.** `context_window == 0` fails the turn.

**Why.** The alternative is to treat "no stated limit" as "send anything", which is the silent
truncation the check exists to prevent — just attributed to the provider instead of the kernel. A
limit that cannot be shown to hold is not a limit.

## 41. A budget enforced by one cooperative caller is not a budget

**Decision.** `inference` and `invoke_tools` refuse to run when the session is out of room. They
close it with the reason and write `budget_exhausted` themselves.

**Why.** `Session::exhausted` had exactly one caller, in the reference loop, and that loop wrote the
event. Every other driver could run a thousand turns with zero credit — and did: turning the gate on
made sixty existing tests fail because their sessions had never been granted anything. The tests were
honest about what the kernel allowed; the kernel was not.

**What it also fixed.** Refusing twice is now impossible: the first refusal closes the session, and a
second call reads `Closed` rather than writing a second identical event. A log with two stops for one
stop is a log a replay has to interpret.

## 42. A commit message is a claim, and it needs a test

**Decision.** The daemon's policy wiring has a unit test, and `load_policy` takes its path as an
argument rather than reading the environment.

**Why.** Commit `1696f9a` says "the daemon now refuses to start without `NUCLEOS_POLICY`". It did not.
The edit that was meant to wire it silently matched nothing, and the tests passed, because no test
ever called `from_env`. The documentation said the same thing. Three artefacts agreed with each
other and none of them was the code.

**Why the signature changed.** A test that sets `NUCLEOS_POLICY` mutates process-wide state that other
tests read in parallel. Taking the path as a parameter makes the decision testable and keeps the
reading of the environment in one line.

## 43. A sandbox profile names resolved paths

**Decision.** Every writable root is written into the seatbelt profile twice when it resolves to
something else: as given, and as `canonicalize` returns it.

**Why.** Seatbelt matches against the resolved path. On macOS `std::env::temp_dir()` is
`/var/folders/...` and `/var` is a symlink to `/private/var`, so a profile naming only the first form
denies writes to the directory it was written to allow — the sandbox fails closed, for a reason
nobody can see, on the most ordinary path in the system. The first end-to-end test caught it; no
amount of reading the profile would have.

**A root that does not exist yet is kept as written.** A caller may be granting a directory the
command is about to create, and refusing to name it would refuse a legitimate grant.

## 44. A sandbox policy is enforced or refused, never ignored

**Decision.** `SandboxPolicy` is either carried out by a mechanism or refused with `Unavailable`.
There is no third path where the command runs unconfined.

**Why.** The previous version refused everything but `danger_full_access`, which was honest and
useless. The tempting fix — run the command and log that the sandbox was unavailable — is worse than
the original: the caller believes it has isolation, the log says otherwise only to whoever reads it,
and the command that most needs confining runs unconfined because a platform is incomplete.

**What the platform changes.** macOS enforces; Linux refuses until bubblewrap and landlock exist. The
tests assert the disjunction rather than one branch, so "enforced" and "refused" both count and
"ignored" does not.

## 45. A sandbox that cannot say what it did is a sandbox nobody can debug

**Decision.** `exec_started` carries the mechanism and the profile text. `mechanism: null` means the
kernel applied nothing.

**Why.** A refusal from seatbelt is `Operation not permitted` and no explanation: the profile is the
only document that knows which rule was missing. Recording the policy instead is not enough — the
policy is an input, the profile is the decision, and rebuilding it from a policy that may have
changed is a second implementation that can disagree with the one that ran.

**Why the null matters.** "No mechanism" has three causes — `danger_full_access`, an external
sandbox, and an escalation the gate approved — and all three mean nobody confined the command. A log
that omitted the field for all of them would read the same as one where a confining policy was
quietly applied to nothing.

## 46. The network is tested against a port, not against the internet

**Decision.** The network tests bind a listener on `127.0.0.1:0` and run `exec 3<>/dev/tcp/…` under
both settings.

**Why.** A test that reaches out to a real host is a test that fails in CI, in an air-gapped build,
and on the day the host changes its certificate. A loopback port exercises the same rule — the
sandbox either permits the `network*` operation or it does not — and it asserts the positive case
too, so a refusal cannot be mistaken for a broken fixture.

## 47. A sandbox may narrow, never widen

**Decision.** `ConfigPatch.sandbox` applies when `SandboxPolicy::narrows_to` holds and is refused
otherwise.

**Why the first version was wrong.** It declared the policy immutable after the session opened. That
is one rule stricter than the design: the codebase already says "narrowing is a configuration change,
widening is an escalation" for tools, and a sandbox is the same shape. The cost of the stricter
version is paid at the worst moment — an operator watching a session misbehave cannot take room away
without killing it and losing the history that explains what happened.

**Why widening stays refused.** An escalation is one command asking for more, decided by the gate
with a human in the loop. A session that could grant itself more room through `configure` would have
a door around that gate, and the second door is the one nobody watches.

**Why the order is by capability, not by name.** `read_only` is narrower than `workspace_write`
despite being a "level" that sounds larger, because it grants no writes at all. `external_sandbox` is
off the ladder: the kernel enforces nothing there, so entering it from an enforced policy widens and
leaving it narrows.

## 48. One log, one sequence counter

**Decision.** `Session.seq` is an `Arc<AtomicU64>` shared with the process supervisor.

**Why.** The kernel's handle and the supervisor both append to the same event log, and they each
counted from their own copy of where it ended. A process that wrote sixteen chunks left the caller's
handle believing two events existed, so the next `configure` or `close` failed a gap check on a call
that did nothing wrong — `event seq 1 does not continue the log at 4`. A copy of a counter is a
second truth about the same log, and the store is right to refuse it.

## 49. A call is answered by the generation that built the prompt

**Decision.** `execute_calls` resolves every tool through `tool_in(session, name)` — the session's
pinned set — and `tool_specs` builds the prompt from the same set.

**Why.** The first version wired the pinning into the prompt and left the execution path resolving
against the newest set. The test that caught it publishes a generation without `read_file` and then
calls the tool from a session pinned before the publish: the prompt had it, the registry did not, and
the model got "no tool named". Prompt and execution reading two registries is a bug that only appears
when a source changes under a running session — which is exactly the case generations exist for.

**A second error the same test found.** The refusal said `no tool named "<call id>"`: the message
formatted the correlation id where the tool name belonged, so the one line a model reads to recover
named something it had never seen.

## 50. An incompatible generation is not an error

**Decision.** A session whose tool set is incompatible with the current one stays pinned, records
nothing, and keeps working until it ends.

**Why.** The alternative — refusing turns, or closing the session — turns somebody else's deployment
into this session's outage. A model midway through a task did not ask for the tool list to change, and
the set it is holding still answers every call it can make.

## 51. The socket's permissions are the authentication

**Decision.** `nucleosd` creates its socket in a `0700` directory, `chmod`s the socket to `0600`, and
optionally requires `NUCLEOS_TOKEN` on every route.

**Why not peer credentials.** They are the right answer for a unix socket and they need `unsafe`,
which the workspace forbids. A comment claiming "the peer is already the local user" is not a check;
`0600` on the socket inside an owner-only directory is one the kernel enforces.

**Why the token covers `describe` too.** An unauthenticated endpoint that lists models and tools is
an unauthenticated inventory. The cost of guarding it is one header a real client already sends.

**Why one principal.** Deriving identity per token is six lines here and a redesign of the
`(tenant, session)` key in the ABI. Shipping the six lines and calling it multi-tenant would be the
kind of claim this file exists to prevent.

## 52. A library emits, the binary decides

**Decision.** Every crate depends on `tracing`; only `nucleosd` depends on `tracing-subscriber` and
installs a subscriber.

**Why.** A library that configures output cannot be embedded, and a kernel that decides where logs go
has an opinion about deployments. The facade is also what makes the logs testable: `tests/logging.rs`
installs a capturing subscriber and asserts the fields, so instrumentation is checked rather than
trusted.

**Why not `log`.** Fields. `session = %id, tool = %name` is the difference between a line an operator
can act on and a sentence they have to parse. Spans come with it and cost nothing when unused.

**What is not logged.** Message content, tool arguments, model output. The event log is the record;
a second copy in a log file has weaker retention rules than the data it duplicates.

## 53. A comment about behaviour needs an experiment that is not broken

**Decision.** The daemon keeps axum's `with_graceful_shutdown`, and the comment saying it "did
nothing" was removed.

**Why it was wrong.** Three test runs showed exit code 143 and no shutdown record, which looked like
axum not polling the future. The kill was aimed at the shell wrapping the daemon, so the daemon was
never signalled at all — it was still running afterwards, socket and all. Re-running against the real
pid showed `with_graceful_shutdown` working, and the hand-rolled `tokio::select!` unnecessary.

**The rule.** An observation strong enough to write into the code as a claim is strong enough to
verify twice: here, `pgrep` would have said the process was alive the whole time.

## 54. A mechanism tested on one platform is tested on one platform

**Decision.** The bubblewrap command line is a pure function tested everywhere; the end-to-end cases
are `#[cfg(target_os = "linux")]` and skip with a printed reason when user namespaces are unavailable.

**Why.** Writing Linux support on macOS means the part most likely to be wrong — flags, ordering,
which paths are bound writable — is exactly the part that cannot be exercised locally. Splitting it so
the command line is asserted on every platform and the execution is asserted where it runs is the most
that can be honestly claimed from here.

**Why the skip is loud.** A test that returns early in silence is a test that has never run and never
said so. The message names the test and the reason, so a green suite does not imply a verified
mechanism.

**Why `bwrap` is looked up first.** A missing binary otherwise surfaces as "No such file or directory"
from a spawn that reports the _inner_ command, which sends the reader to the wrong program entirely.

## 55. A checkpoint is a cache; the log is the truth

**Decision.** `load` folds every event after the snapshot's sequence number.

**Why.** The kernel checkpoints at turn boundaries, so a process that dies mid-turn leaves events
after the last checkpoint — and the old `load` ignored all of them. The session came back owing money
it had forgotten and missing a message it had completed. Nothing failed loudly: the numbers were
simply smaller than reality, which is the worst kind of wrong for a system whose whole claim is "the
log is the record".

**Why the classification is exhaustive.** With a catch-all arm, a new event kind silently becomes
"describes nothing, changes nothing". Without one, adding a kind forces the author to decide whether a
reloaded session is affected — a question with an answer, asked once, at compile time.

**Why an abandoned turn is recorded and not resumed.** Providers cannot resume a stream, and a
truncated assistant message is a fragment the model would reason from as if it had finished. So the
partial stays in the event stream (a caller can read it), the session does not adopt it, and one
`attempt_failed` says what happened. The next `load` appends nothing more: the record itself counts as
a completed unit, which is what makes recovery idempotent instead of cumulative.

## 56. A library that assigns the token decides who can subscribe

**Decision.** Progress from an MCP server is **not** forwarded. The kernel's own `progress` events
are wired (`ToolCtx::progress` → one event per report); the transport half is not.

**Why not.** `rmcp` 3.8 assigns the progress token itself, per request, and **overwrites** one a
caller sets in `_meta` — the wire shows the library's counter, not the caller's value. Reading the
assigned token back is possible (`RequestHandle::progress_token`) but only through
`send_cancellable_request`, which needs a `ClientRequest::CallToolRequest` value that the library does
not let a caller construct.

**What the alternatives were.** Guess the token from the library's counter (wrong the moment two calls
are in flight, which a batch always is), forward every notification to every running call (attributes
progress to the wrong call), or patch the dependency. None of them is worth shipping; the honest
output is a documented gap and a note in the code at the exact line that would need it.

**What this cost to learn.** Two wrong implementations: forwarding through a custom `ClientHandler`
(the subscription never matched, because the token was not ours) and reading the token off a request
handle (unreachable). Both compiled. Neither worked. A test against the real fixture caught the first;
reading the dependency's source caught the second.

## 57. An id is not an identity

**Decision.** The snapshot records its owner (`Principal`), `load` refuses a session belonging to
another principal with `NotFound`, and `open` refuses an id another tenant already owns.

**Why.** The store keys sessions by id alone and `load` took the *caller's* principal as the session's.
With one tenant that is invisible; with two it means the first caller to guess `s1` reads somebody
else's history, is billed for nothing, and can close their work. The multi-tenant token file made the
hole reachable, so it was fixed in the same change rather than after it.

**Why `NotFound` and not `Denied`.** "This exists but is not yours" answers a question the caller had
no right to ask: it enumerates the identifiers other tenants use. A missing row and a row that is not
yours look the same from outside, which is the point.

**Update: it is now structured, not checked.** The first version kept the store keyed by id alone and
refused a cross-tenant collision at `open`. That turned one tenant's ordinary choice of `s1` into
another tenant's error, and left the isolation depending on a check somebody had to remember. The key
is `(tenant, session)` now — in the tables, in the `Store` trait, in the listing query — so a lookup
that spans tenants does not exist to be written. The runtime check that remains is about *subjects*
inside one tenant, where it is a policy rather than a boundary.

## 58. A silent tool is not necessarily a hung one, and the kernel can say so

**Decision.** A call that has not reported after `heartbeat` emits `progress { done, total: 0,
message: "still running" }`, once per interval, until the batch ends or the tool reports.

**Why.** The tool-facing sink made progress possible and not guaranteed: a tool that reports nothing
produced no record, so the only feedback on a five-minute call was the deadline. A caller cannot tell
that from a hang, and the operator reading the log cannot either.

**Why it stops once the tool reports.** Two reasons, and the second is why the first is not enough on
its own. Interleaving "still running" with real percentages makes the stream noisier without adding
information; and a heartbeat that continues after a tool has spoken would be the kernel guessing that
the tool has stalled, which it cannot know. The deadline is the bound for a tool that goes quiet
forever; the heartbeat is for tools that never said anything.

**Why it is configurable rather than a constant.** Fifteen seconds is wrong for a build that takes
minutes and wrong for a database call that takes milliseconds. A constant would be one deployment's
guess baked into every other deployment's logs, and a test would either sleep for it or not exist.

## 59. The record of an abandoned turn must not erase it

**Decision.** `partial` terminates on a completed message, a finished tool or a state change — never on
`attempt_failed`.

**Why.** The first version reused `completes_a_unit`, which counts `attempt_failed` as ending a turn,
because for deciding *whether* a turn was abandoned it does. But `load` writes exactly that event when
it detects an abandonment, so reading the fragment afterwards found the writer's own annotation and
returned nothing: the recovery record had erased the thing it recorded. The test failed on the first
run, which is the only reason it was noticed.

**The general shape.** Two questions that look like one: "has this turn ended?" (yes) and "does this
event belong to the fragment?" (no). Sharing a predicate between them was the bug.

## 60. Configuration is a privilege, and a tool rule is not one

**Decision.** `Gated::Config { scope, subject }` gets its own rule field (`config`, narrowed by
`scope`), never a `tool` rule. An `ask` verdict is translated to a refusal.

**Why it needs its own field.** The failure is quiet and total: a deployment whose policy says
`{"decision": "allow", "tool": "*"}` — a perfectly ordinary way to let a session use its tools — would
otherwise also grant whoever holds that token the right to **add a model**, which points every
session's traffic at a server of their choosing, and to **replace the policy** that was the only
reason the first grant was safe. Reusing the field would make one permission imply an unbounded one.

**Why `ask` is a denial here.** Everywhere else in this kernel `AskHuman` parks a session and waits.
Configuration has no session: there is nothing to park, nothing to wake, and a question with no
waiting asker is a decision the daemon would have to make on its own — after a timeout, silently, in
the permissive direction. Refusing and naming the alternative ("change it with the administrator's
credential instead") is the only answer that stays true when nobody is watching.

**Why the scope is in the request, not in the token.** The two authorities are different: the
deployment's configuration is the operator's, and a tenant's narrowing is the tenant's. A rule written
for `tenant` must not reach `system` even when the subject is spelled the same, which only holds if the
request says which scope it is being judged against.

## 61. A rate limiter inside the process is not a boundary

**Decision.** Recovery attempts are limited to five a minute, in memory, per process. The hash is
Argon2id at 64 MiB and three passes, about 1.5 seconds per attempt on a laptop.

**Why the limiter is not the protection.** Recovery runs locally, in a process that already has the
database file. Anyone holding that file can attack the hash offline at whatever rate their hardware
allows, and no counter in this process can reach them. The thing that actually bounds them is the
memory cost. The limiter addresses the case it can: a person at a keyboard, guessing, in the one
process that is running.

**Why say it out loud.** A limiter that is believed to be a boundary is worse than no limiter, because
it relaxes the choice of parameters — "five attempts a minute" sounds like it buys the right to a cheap
hash, and it does not.

**Why the parameters are in the stored string.** The PHC encoding carries algorithm and cost, so
raising them later does not invalidate existing hashes, and a deployment can be re-hashed lazily at the
next enrolment instead of at a migration.

## 62. A private key with a `Debug` derive is a private key in a log line

**Decision.** `KeyPair` implements no `Debug`, no `Display` and no `Serialize`. Its public half is
`PublicKey`, which derives all three.

**Why.** `#[derive(Debug)]` is one line, and it is the line that puts a secret into every `{:?}` in a
test, every `dbg!` someone adds while debugging, and every panic message that formats the value it was
holding. The protection is the absence of the impl, not a reminder to avoid it: `expect_err` on a
`Result<KeyPair, _>` does not compile, which is how the cost of having no `Debug` shows up exactly once,
at the place that would have needed it.

**The same reasoning, one layer up.** `PasswordHash` does implement `Display`, and prints
`<recovery password hash>` — a hash in a log line is a hash somebody can attack offline.

## 63. A missing signature is 401, a denied verdict is 403

**Decision.** The administrative extractor answers `401 Unauthorized` for a missing or invalid
signature; a policy verdict of deny stays `403`.

**Why.** They are different questions. `401` is "who are you", `403` is "not you" — and a client that
receives `403` for an absent credential goes looking for a permission it does not lack, which is time
spent in the wrong file. The first version of the endpoint returned `403` for both, because both are
`ErrorCode::Denied` inside the kernel; the status code is the transport's to decide, and it has the one
piece of information the kernel does not: whether a credential was presented at all.

**Why the signature covers the body hash and the path.** A signature over "this request" that omitted
either would be a signature over a verb. Captured on a `GET`, it would authorise a `PUT` that changes
the policy — which is why those two are lines in the canonical form and not assumptions about the
transport.

## 64. A configuration change is applied, then stored

**Decision.** `PUT /v1/policy` swaps the kernel's policy first and writes the store second; a store
failure leaves the daemon enforcing something it cannot explain.

**Why that order.** The other one leaves a daemon reporting a change that never took effect: the
operator reads `200 OK`, the endpoint still answers with the old policy, and the difference is invisible
until somebody checks. A daemon whose store disagrees with its behaviour is a loud, recoverable state —
it fails on restart, with a message — while a daemon that reports success it did not have is a lie with
a delay fuse.

**Why the history is in the system store.** The obvious place is the session event log, which already
has sequence numbers and a replay. But that log is per session and is pruned; a deployment's own audit
trail must outlive the sessions it audited. The two stores were split by lifecycle for exactly this
reason, and putting configuration history in the prunable one would undo the split.

**What is deliberately not in it.** API keys in the clear. The system store is backed up and copied, and
a backup that contains a credential is a credential dump — so a model key is either a `key_file` path or
a value **sealed** with a key that lives outside the store (`docs/config.md` §6.0). This paragraph said
"sealed credentials are designed and not built" for two commits after they were built; the pitfall is
recorded here because a document that describes a property it no longer has is the failure mode this file
exists to catch.

## 65. Revoking the last token must not reopen the door

**Decision.** Issuing a token sets a sticky flag (`auth/tokens_enabled`) that is never cleared, and the
authenticator asks "are tokens required" rather than "is there a token right now".

**Why.** "No tokens configured" and "all tokens revoked" are different states that look identical in
the table. The first is a development deployment where the socket's permissions are the whole
authentication; the second is a production deployment that just closed its last door. Treating them the
same means revoking a leaked token silently reopens the daemon to every local caller — during an
incident, in the one direction nobody wants.

**What the test shows.** Revoke the last token, then present nothing and then present the revoked value:
both are refused. The state after a revocation is "no valid credential exists", not "protection is off".

## 66. Two credentials, one middleware

**Decision.** The admission middleware enforces bearer tokens, and passes a request carrying
`Authorization: Nucleos-Admin …` straight through to the handler that verifies it. The `Caller`
extractor answers `401` — with a sentence naming the two schemes — when a route that wants a token is
handed a signature.

**Why it was needed.** With tokens configured, an administrator's signed request to `/v1/tokens` was
rejected before the signature was ever looked at: one middleware knew one credential type. The refusal
was `401 "this daemon needs a bearer token"`, which is exactly the right message for the wrong reason.

**Why the pass-through is not a bypass.** The signature covers the method, the path and the body hash,
and it is verified by the extractor on the route that needs it. A request that carries one to a route
with no administrator check gets `401` from `Caller`, not a handler that trusts it.

## 67. A list frozen at start-up is a list that lies

**Decision.** `describe` and the default route read the model registry when asked, not the values they
were given at construction.

**Why.** Two snapshots existed and both became wrong the moment `POST /v1/models` succeeded: the
advertised model list, and the routing chain a session inherits. The symptom was a deployment where a
model had been added, the API had said `200`, `/v1/models` listed it — and `/v1/describe` said the
deployment had none, and every session was refused for having no route. The change worked; two copies
of the old state did not.

**The rule.** Anything a caller reads to decide what to do next must be derived at read time. `describe`
is exactly such a thing, which is why it now reads the registry and the store rather than a `Vec` filled
in `from_env`.

**A related one, caught by the same test.** A session with an empty routing chain now takes the
deployment's first model instead of failing. The alternative — requiring every caller to name a model —
turns a deployment default into a per-call responsibility, and the error a client saw ("routing chain is
empty") was about the daemon's configuration, not about anything the client did.

## 68. A second source for a value is a value nobody rotates

**Decision.** `NUCLEOS_POLICY`, `NUCLEOS_ROOT`, `NUCLEOS_SOCKET` and `NUCLEOS_LOG` are gone. Policy,
models and tokens come from the system store; paths and the log level are flags.

**Why policy was the last one to go.** It had a file fallback, which read as flexibility and was a
second answer to "what is this daemon enforcing". Two answers disagree in exactly the situations people
care about: an incident, an upgrade, a config that was changed in one place and not the other. With one
source, a missing policy is not a configuration gap — it is an uninitialised deployment, and the daemon
says so and names `nucleos init`.

**Why the log level is a flag rather than a store row.** Verbosity is needed *before* the store is
readable — the first thing a broken deployment has to tell you is why it broke, and a setting it cannot
read yet is not a setting. A flag is in the unit file, in `ps`, and in the deployment's revision history.

## 69. A source that fails to start is a warning at boot and a failure by request

**Decision.** `SourceSet::load` logs a source that will not start and skips it; `add` fails the request
if the server does not come up.

**Why the asymmetry.** At boot, nobody is waiting. Refusing to start because one MCP server is down
turns a missing tool into an outage, and the sessions pinned to older generations would keep working
anyway. By request, somebody *is* waiting: storing a source that cannot start moves the failure to
every future boot, where it is read by whoever reads logs at 3am instead of by the person who typed the
command.

**What both share.** A source is never stored before it has started at least once, so the stored list is
a list of servers that exist. The tools are discovered rather than stored, for the same reason: a stored
tool list is a cache that goes stale the moment the server is upgraded.

## 70. A recovery path that is remote is an oracle

**Decision.** `nucleos admin recover` reads the store from the filesystem and never talks to a daemon.
Enrolment of a *public* key is over the ABI; recovery of a *lost* key is local.

**Why.** The two look similar and are not. Enrolling a public key is safe on the wire — the private half
never travels, and the requester must already hold a valid signature. Recovery is the opposite: its whole
purpose is to work when no key is valid, so a remote path would be a service that answers "is this
password right?" to anyone who can connect. Local access is the second factor, and the fsync cost of
reading a file is a cheaper check than any rate limiter could be.

**What recovery deliberately does not do.** It does not revoke the remaining keys. An operator who lost a
laptop is not an operator who wants every other administrator locked out, and a recovery tool that
quietly narrows access is one nobody runs during an incident.

**And it says what it did not need.** The daemon re-reads the key list on every request, so the recovered
key works immediately. The first version of this command printed "restart nucleosd if it is running" and
then explained that it re-reads per request — two sentences that contradicted each other, in the output
of the tool people run when they are already having a bad day.

## 71. A tenant's policy is a second answer, not a replacement

**Decision.** `Layered` evaluates the deployment's document and the tenant's, and the stricter verdict
wins. `scope: "own"` compares the scope against the caller.

**Why replacement would be a permission slip.** A tenant that wrote `{"default": "allow"}` would
otherwise be a tenant that escaped every rule the deployment made — the narrow scope with the *wider*
effect, which is the opposite of what narrowing means everywhere else in this design.

**Why `own` rather than a tenant identity layer.** Delegation needs one comparison — "is this scope the
caller's own tenant" — and a new identity concept would need a new column, a new token claim and a new
way to be wrong. One scope value that compares against the principal answers the same question, and the
policy document stays the only place authority is described.

**What the composition costs.** Every check now asks two policies instead of one. Cheap, and the
alternative is worse: cached per-tenant answers would have to be invalidated on every policy write, in a
daemon whose whole point is that changes take effect on the next call.

## 72. A ceiling that is only in the prompt is not a ceiling

**Decision.** A tool outside the session's ceiling or the tenant's is refused at execution, with content
saying so — not merely hidden from the prompt.

**Why.** The design already distinguishes exposure from permission, and hiding a tool is exposure. But
the *ceiling* is the permission: the application's allow-list, and now the deployment's per tenant. A
model that guessed a name would otherwise reach a tool it was never meant to use, and the refusal would
not exist to be logged.

**Why both halves need it.** The prompt is where a model learns a tool exists, so a hidden tool stays
hidden in practice — but "in practice" is not a permission model. The test asserts both: the tool is gone
from `tool_specs`, and calling it by name fails.

**Why the model list filters instead of refusing.** A chain is a list of fallbacks by design. Refusing the
call because one entry is forbidden would make per-tenant access incompatible with the routing that
exists for every other reason.

## 73. Encryption with the key beside it is decoration

**Decision.** Sealed values live in the system store; the sealing key lives outside it, resolved from
`$CREDENTIALS_DIRECTORY/seal.key` first and a configured path second. With no seal key, an API key cannot
be stored at all.

**Why the "outside" is the whole design.** The store is the thing that gets copied — backups, a restore
on another machine, a support bundle. A key stored beside the ciphertext it opens makes all three cases
identical to storing plaintext, with extra steps and a false sense of safety.

**Why nothing instead of a default key.** A built-in key would be a constant dressed as encryption: it
would make every deployment's ciphertext readable by anyone with the binary, and nobody would notice,
because the feature appears to work.

**Why the default location is still under the root.** It makes the *data directory* safe, which is the
common backup. The whole-machine case is answered by the first source — `systemd-creds` seals to the TPM
and never writes the key to disk — and `init` prints a line telling the operator to move the file when
they care about the second case. Saying it out loud beats choosing for them.

## 74. A default that makes the claim false is not a default

**Decision.** The seal key defaults to `/etc/nucleos/seal.key`, `init` refuses a path inside the data
directory, and `nucleosd --seal-key` exists for deployments that keep it elsewhere.

**Why it was wrong the first time.** The default was `<root>/seal.key`, chosen because it made
`init --root …` work without a second flag and the daemon find the file without a second parameter. The
paragraph above the table in the same document says a copy of the data directory contains no usable
credential — and with the key sitting inside that directory, the copy contains the one that opens
everything. A week of documentation discipline, undone to save one flag.

**The general shape.** When a design property and a convenience collide, the convenience is the thing
that gets a flag, not the property. The test that now guards it asks the only question worth asking: is
the key's path inside the root's path?

## 75. A rotation is two phases, and the first writes nothing

**Decision.** `nucleos seal rotate` opens every sealed row with the current key before it writes any of
them back, verifies each row with the new key afterwards, and requires a restart.

**Why the order.** A store half under the old key and half under the new is a store that loses models at
the next start, and the failure appears at boot rather than at the command — where nobody connects it to
the rotation. Opening first means the common failure (the operator named the wrong current key) leaves
everything exactly as it was.

**Why no hot swap.** The daemon resolves the seal key at start-up and keeps it. Swapping under a running
daemon would produce the same split store, with the added confusion that the daemon would keep serving
with a key the store no longer uses. A restart is a boundary the operator can see.

**Why verify the write.** Writing a value the new key cannot open is indistinguishable, at write time,
from writing one it can. The read-back is three lines and it is the difference between a rotation report
that means something and one that means "no error was raised".

## 76. Cancelling is not closing, and the signal must be armed before it is read

**Decision.** `POST /v1/sessions/{id}/cancel` stops the running turn and leaves the session open. The
signal is a flag plus a notification, and the waiter registers its interest *before* it checks the flag.

**Why not close.** Closing is the heaviest thing a session can do: the conversation is finished, the
caller must open another. Doing that to stop one call loses work to avoid spending it. The turn ends, the
session survives, and the caller decides next.

**Why the ordering.** `notified()` first, then `load()`:

```text
let waiting = notified();   // register
if flag { return; }         // then check
waiting.await
```

The other order loses a cancellation that lands in the gap between the load and the await. A turn that
ignores a cancel is worse than one that cannot be cancelled: the caller believes it stopped, and the bill
says otherwise. This is the same class of bug as a lost wakeup, and it is invisible in testing because it
only shows up under the timing the test does not have.

**Why `cancelled: false` is not an error.** A caller that cancels a turn which has just finished is
racing, not making a mistake. Both `404` and `500` would be false statements about the session.

**What cancelling does not do.** It does not retry the turn (a cancellation is a decision, and repeating
it undoes the decision), does not delete the deltas already streamed, and does not refund the spend that
was reported before it landed. The attempt is logged as `cancelled` rather than as a failure of the
provider, because the difference is what the next morning's reader needs.

## 77. A pin is a reference, not a number

**Decision.** A session holds its tool set (`Session::toolset`, an `Arc<ToolSet>`) instead of looking it
up by generation number. `POST /v1/tools/retire` forgets every number but the current one.

**Why the number was wrong.** Retirement was skipped for a long time with a written justification: freeing
a set requires a durable answer to "which sessions still reference it", and the store can only give that
by reading every snapshot. That reasoning was correct *about a map keyed by number*. The number is a pin
that only holds while the map keeps the entry, so the map cannot drop anything until it knows nobody will
ask. An `Arc` is a pin that holds because somebody is holding it — the refcount is the answer, so no scan
is needed and no query can be forgotten.

**The trap this avoids.** If `migrate` resolved the pinned side by number, then retiring a generation
would make that number resolve to the *current* set, `compatible(set, set)` would be true, and an
incompatible session would silently jump onto the new set — the exact failure pinning exists to prevent,
introduced by the cleanup intended to save memory. Compatibility is therefore computed against
`session.toolset`, which is the set the model actually saw.

**And the current generation is never retired**, because it is the target of every migration. A
retirement that takes away the only place a session can move to turns a memory saving into a stuck
session.

## 78. Two crypto providers in one process is a daemon that dies on its first caller

**Decision.** `serve_tls` installs `aws-lc-rs` as the process default before building the TLS config, and
`nucleosd` names `rustls` as a direct dependency so the choice is in the build graph rather than in
feature unification.

**The failure.** rustls refuses to pick a default when more than one provider is compiled in, and panics
at the first handshake. In this workspace both are: `ring` arrives through the HTTP client that talks to
models (`reqwest` → `hyper-rustls`), `aws-lc-rs` through the TLS server (`axum-server`). The daemon would
start, log `tls_listening`, and die on the first caller — a start-up that looks like success, which is the
worst shape a TLS failure can take.

**Found by the test that did not exist yet.** The integration test failed with rustls' message about
`CryptoProvider`, which read like a test-configuration problem. It was not: the test links the same crates
the daemon does. The fix belongs in the product.

**Same class, one line down.** A `std::net::TcpListener` is blocking, and registering one with the tokio
reactor is a panic rather than an error. `serve_tls` calls `set_nonblocking(true)` so the flag is set
where it is needed rather than in every caller.

## 79. A transport without its certificate is a refusal, not a warning

**Decision.** `--tcp` requires `--tls-cert` and `--tls-key`, and the certificate pair requires `--tcp`.
Either combination that leaves one half out is a start-up error that names the reason.

**Why not a warning.** A daemon listening on plaintext TCP with a log line saying so is a daemon that
leaks bearer tokens and administrative signatures to the network, and nothing in the system notices: the
requests succeed. A warning is a message somebody has to read at the moment they are least likely to. The
same applies in the other direction — a certificate nobody serves is a deployment whose operator believes
it is encrypted, and that belief is the thing that will not be true.

## 80. A second sender keeps a stream open forever

**Decision.** The streaming response owns its cancellation through the *drop of the body*
(`CancelOnDrop`), not through a task watching the channel.

**The deadlock.** The first version spawned a watcher holding a clone of the mpsc sender and awaited
`Sender::closed()`. That call resolves when every **receiver** is dropped, so the watcher's sender stayed
alive for as long as the receiver existed — and the receiver only ends when every sender is gone. The
response body therefore never finished, and the test hung with no error at all: no timeout, no panic, just
a request that never completed. `Sender::closed()` is a fine signal for "the last consumer went away"; it
is not a way to observe it while holding the channel open.

**The rule.** A body that wants to do something when it is dropped belongs in a `Drop` impl on the body.
Dropping the body is what a closed tab looks like from the server, and it arrives immediately — unlike a
task that is asleep on a channel nobody will ever write to again.

**Disarm on normal completion.** The guard must not fire a cancel after the stream ended by itself, or a
client that read everything will leave a `turn_cancelled_by_disconnect` in the log for a turn that had
already answered.

## 81. The commit point is an event, not the transport

**Decision.** A streaming inference answers with an HTTP status code when the turn fails before
`turn_started`, and with frames after it. `turn_started` is written only after the budget is spent, the
route resolved and the policy satisfied.

**Why not "the first frame".** The first version decided by the first frame it saw, and a request with no
credit came back `200` with an SSE body: `spend` writes `budget_exhausted` **before** it returns the
refusal, so a frame existed even though no turn had begun. "Something was written" and "the turn is
running" are different questions, and only the second one is the commit point.

**Why not "any error is a frame".** A caller that sends a bad request wants a status code, and SDKs,
proxies and retry policies all key on one. A `200` followed by an error frame is a request every middlebox
in the path treats as successful.

**The consequence for the log.** Nothing is lost by answering with an error instead of opening a stream:
`budget_exhausted` is already in the session's record, and `GET /events` will show it. The stream is a
convenience, the record is the truth.

## 82. `select!` chooses at random when both branches are ready

**Decision.** The branch that decides "stream or not" is `biased`, with the event feed first.

**The bug it prevents.** A turn on a fast fake model finishes almost immediately, so at the first
`select!` both the frame and the completion are ready. Without `biased`, `tokio::select!` picks a random
branch: the same request answers `200` + SSE in one run and `200` + JSON in the next — a flaky test that
would be dismissed as a flaky test, and a real client that gets the wrong content type one time in two.

## 83. A tagged enum inside a struct is nested unless the field says otherwise

**Decision.** `Event.kind` carries `#[serde(flatten)]`, like `Part::kind` already did.

**The shape it was shipping.** `{"kind":{"kind":"text_delta","part_index":0,"text":"Hel"}}` — the tag
twice, and every frame one level deeper than the documentation claimed. It survived because nothing read
it by hand: the tests asserted the nested path *after* writing it, and the docs were written from the
intent rather than from an example.

**How it was found.** By reading a live frame from a real daemon instead of from a test fixture — the
curl output did not look like the document, which is the check that tests written from the same
assumption cannot make.

**No compatibility path, deliberately.** A tolerant reader was written and then removed: nothing has
been released, so there is no old format to be tolerant of. A compat layer for a shape that never
shipped is code that will outlive its reason and be impossible to delete later, because by then nobody
will know whether something depends on it.

**Why it matters for streaming.** The SSE `event:` name and the JSON `kind` are supposed to be one fact
twice. Nested, they are two facts at two addresses, and a client that reads only `data` has to know about
the nesting the docs never described.

## 84. A turn that fails must fail in writing

**Decision.** `inference` records `turn_failed` when it returns an error and nothing was written after
`turn_started`. `message_completed`, `attempt_failed` and `turn_failed` are the three ways a turn ends,
and one of them always happens.

**The failure this fixes.** A policy refusal inside `call_model` returned `Err` straight out, emitting
nothing. The log then held `turn_started` and stopped — the same shape as a turn that is still running.
Found by streaming a real daemon whose policy denied the model: the client got one frame and then a
closed connection, with no way to tell a refusal from a dropped network.

**Why not a synthesized `attempt_failed`.** It would be a lie with a schema: no attempt ran, so inventing
one puts a model and a duration and a trigger in the record that never existed. `turn_failed` says what
is true — the turn ended, and here is why — which is also what a client needs.

**The rule is written where the many failures are not.** The check lives in `inference`, comparing the
sequence number against the one taken when the turn was announced, rather than in each of the places that
can fail. A rule enforced at every site is a rule with as many chances to be forgotten as there are
sites.

## 85. `cfg(target_os)` cannot describe a runtime capability

**Decision.** Tests that depend on a sandbox mechanism ask the *host* what it can do, and assert the
property rather than the platform: enforced (a start event naming a mechanism) or refused (a reason
naming the program to install or the flag to pass).

**The mistake, three times in one day.** Each of these was written as "macOS enforces, everything else
refuses", and each had to be discovered by CI on Linux:

- `process_run.rs` — a test spawn: Linux has bubblewrap, the spawn succeeded, and the test asserted an
  error that was not there.
- `sandbox.rs` — the refusal test, same assumption, same result.
- `http.rs` — the same thing over the ABI, and it *kept passing* for a year of imagination because
  macOS is where it ran. `503` was asserted; Linux answered `200`.

The pattern is the same every time: a compile-time split is a claim about the build, and the question
underneath ("can this host sandbox a command?") is a claim about the machine. On the developer's laptop
the answer was always the first branch, so the second was never executed anywhere.

**What replaced them.** One property, three assertions: either the process ran and its start event names
the mechanism, or it was refused with a reason naming what to install or the escape hatch. Both are true
statements on every platform, and the one failure the tests exist to catch — a policy silently ignored,
which produces a running process with *no* mechanism — fails on all of them.

**The corollary, learned the same day.** "bwrap is installed" and "bwrap can run" are different
questions, and only the first is the kernel's: it refuses a missing program, not a kernel without
unprivileged namespaces. A test that tried to cover the second failed in CI, where bubblewrap is present
and namespaces are not allowed.

## 86. A shared counter is not a shared order

**Decision.** Every writer to a session's log holds the session's `append_lock` while it takes a
sequence number **and** appends the event. `Kernel::emit`, the process supervisor's reader tasks, and
the progress writer all take the same lock.

**The bug.** The counter was already shared — `Session.seq` is one `Arc<AtomicU64>`, cloned into every
task — and the documentation said so in capitals: *the same counter, not a copy, because two counters
disagree*. That is true and it is not enough. Reserving a number and writing it were two steps:

```text
stdout: seq = fetch_add(1) → 5
stderr: seq = fetch_add(1) → 6
stderr: store.append(6)        ← arrives first
stdout: store.append(5)        ← the store has 6, refuses 5 as a gap
```

The store is right to refuse: a silently renumbered event makes every later resume cursor wrong. The
reader then breaks on the error and that chunk of output is gone.

**Why it survived.** It needs two writers and a scheduler that lets the second one overtake: stdout and
stderr are that pair, and on a fast local machine stdout almost always wins. CI lost the stderr of a
`printf` and the test that found it was asserting that both streams arrive — an assertion about the log,
which is where the loss was visible.

**The shape of the fix, and why there.** A lock around two lines, held by every writer, rather than a
retry in the store or a queue per stream. The invariant is "one log, one order"; the cheapest place to
enforce it is where the order is decided.

## 87. A lock that lives on the handle locks nothing

**Decision.** The sequence counter and the append lock live in one `LogHead` per `(tenant, session)`, owned
by the kernel; every writer resolves to the same `Arc`.

**The bug this replaced.** #86 put the lock *on the session*, which reads as "per session" and is not: a
`Session` is a handle, and `load` mints one per request. So the supervisor held one lock and the next request
held another, over one log. The append that #86 fixed stayed fixed only while exactly one handle existed for a
session — which is the case every unit test happened to be in.

**Why the test could not see it.** The regression test appended from one handle at a time, so it proved the
lock worked, not that there was one lock. The new test (`tests/log_head.rs`) takes two handles on one session
and has both write at once; with per-handle counters they reserve the same number and the store refuses the
loser as a gap.

**The generalisation worth keeping.** State that must be unique per *thing* cannot live on a type that is
constructed per *use*. A handle may own caches; it may not own a counter, a lock, or a lease.

## 88. One root has one writer, and nothing said so

**Decision.** `nucleosd` takes an advisory `flock` on `<root>/nucleosd.lock` before it binds anything, and
refuses to start if another process holds it.

**The failure it prevents.** Two daemons on one root both append to one event log, each with its own idea of
the last sequence number. The store refuses the second as a gap, the losing writer drops its event, and the
first symptom is a `load` that fails hours later on a session neither daemon is looking at. Nothing in the
system would ever attribute that to a second process.

**Why advisory and not mandatory.** The root is private to the daemon's user, and a mandatory lock on a file
somebody might legitimately inspect would turn a read into an outage. Release is automatic: the kernel drops
an `flock` when the process dies, so a crash leaves no stale lock for an operator to hunt down — which is the
one thing that makes lock files annoying.

## 89. Narrowing a page after the store chose it is a lie with a cursor

**Decision.** The subject is part of `SessionQuery`; the store filters, and the daemon does not.

**The failure.** `GET /v1/sessions` asked for `limit + 1` rows, then dropped the ones belonging to another
subject. A caller with one session of their own and three of somebody else's in the same tenant received **one
row and a `next_page_token`** — told "there is more" and "you have seen everything" in the same response. A
client that trusts the cursor loops forever on pages that contain nothing.

**The rule.** A filter the caller cannot express is a filter the store has to apply. Post-filtering is only
safe when it cannot change the size of the page, and "belongs to me" can.

## 90. A durable format needs a receptor before it needs an upcaster

**Decision.** `EventKind` has an `#[serde(other)] Unknown` arm, and `sessions.db` carries a
`schema_version` that refuses a store written by a newer build.

**The failure it prevents, in order of arrival.** A log is shared by whichever builds run against it —
during a rolling upgrade, or after a downgrade somebody did to escape a bad release. Without a receptor,
one event kind added in the new build makes every `load` of every session containing it fail with
`invalid type`, so the *old* build cannot read the *new* build's work at all. That is not a schema
incompatibility; it is a total one.

**Why the receptor came before the upcaster.** The docs promised an upcaster table "from day one" and
there is none, because no version has been incremented — there is nothing to upcast. Promising both at
once would have produced an empty table and a false claim. The receptor is the part that has to be true
first, and the document now says exactly that.

**What the `other` arm costs.** The original kind name is lost on read: serde keeps the event, not the
tag. That is acceptable only because this kernel never rewrites an event — the stored body stays the
only copy — and it is worth stating, because a reader that assumed it could re-serialise an event
faithfully would be wrong.

**The store's version, and why it is a separate one.** `system.db` and `sessions.db` have two
lifecycles; a single version would refuse to open the session store because the configuration store had
not caught up, or the reverse. The refusal names the fix — "upgrade nucleosd rather than downgrading
the store" — because a parse error at the first `load` names nothing.

## 91. A budget supplied per request is not a budget

**Decision.** `Snapshot` carries the `Budget` a session was opened with, and the wall clock carries
`spent_wall_millis` — the time the session has actually run, accumulated at each checkpoint.

**The bug.** The checkpoint had `credit_tokens` but not `budget`, and `load` passed
`Budget::default()` into the restored session. Over the ABI every request loads, so `max_steps`,
`max_tokens`, `max_micros_usd` and `max_wall_millis` applied to the first turn and then forgot
themselves. The comment on `credit_tokens` — "a budget that resets on reload is not a budget" —
described the field next to the one that was missing.

**Why the wall clock needed its own accounting.** Restoring the original start time would charge every
idle gap between requests to a session whose budget is about how long it *runs*: a session parked on a
human verdict would exhaust its wall budget while nobody was looking at it. Restoring `0` would make
the limit unreachable over the ABI, which is where it was. Accumulating at the checkpoint, and
restarting the clock at load, makes the number mean what it says and keeps the original intent.

**The test that shows both.** Four seconds of running, a minute of nothing, then a turn: allowed. Two
more seconds of running: refused. Either half alone would pass with a broken implementation.

## 92. An ending must retire the evidence that something began

**Decision.** `completes_a_unit` includes `TurnFailed` and `Failed`, and a turn that writes
`turn_failed` checkpoints immediately.

**The bug, and whose it was.** `turn_failed` was added so a failure that never became an attempt still
says why. It was not added to `completes_a_unit`, and the turn did not checkpoint after writing it — so
the very next `load` folded `turn_started, turn_failed`, saw the announcement as evidence of an
abandoned turn, and appended `attempt_failed{reason: ABANDONED}`. Recovery invented a model, a trigger
and a duration for a turn that had already said what happened: the "lie with a schema" §84 forbids,
produced by the code that added `turn_failed`, on the path that runs after every failed turn.

**The rule.** Every ending retires the beginning: `message_completed`, `attempt_failed`, `turn_failed`,
`failed`, `state_changed`. A unit test asserts the set, because the failure mode is a missing member
and the compiler cannot see a `matches!` that is merely incomplete.

## 93. A parked call is consumed by running it

**Decision.** `invoke_approved` removes the `PendingCall` whose id it is about to run, and checkpoints
if it removed anything.

**The bug.** `Session::pending` only ever grew. After an answer, the entry stayed, so the session
reported "waiting on a human" for the rest of its life — and a retried verdict (the client never saw the
first response, so it sends the same one again) found the entry and ran the tool a second time. Tools
are not idempotent; the log then held two results for one call id.

**Why removal comes before execution.** If the process dies mid-call, the log says a call started and
never finished, and `partial` reports it — recoverable truth. An entry left behind would invite the
retry that runs side effects twice, which is the failure this removes. Between "a crash leaves a
visible half-call" and "a retry runs `rm` twice", the first is the one to choose.

## 94. A kill is not a message in a queue

**Decision.** The kill path is a `tokio::sync::watch`, not `Control::Kill` in the supervisor's mailbox;
the map lock is released before any send; `wait` takes the exit handle instead of removing the entry;
and the post-exit pipe drain is bounded.

**The failure, in three layers.** The audit found the first one — `write_stdin` and `signal` held the
supervisor's map across `send`, and `send` waits when the channel is full — but fixing only that made
the test fail with the *same* timeout, which is how the other two were found:

1. **The lock.** A full channel plus a held mutex meant `signal` queued behind a writer, so nothing on
   the node could be killed.
2. **The queue.** Even with the lock released, a kill still queued behind four hundred stdin chunks —
   a kill at the back of a queue is a kill that arrives after the reason for it. A watch send never
   queues.
3. **The blocking write.** `write_all` on a pipe the child is not reading waits forever, and while the
   supervisor waited it was not selecting at all, so nothing could observe the kill.

**The trap in the fix.** `watch::changed()` *consumes* the change. The first version raced the write
against the kill and then ignored it (`_ = kill.changed() => {}`), so the outer branch waited for a
change that had already been seen and the child ran to completion. Both places now act on it, and the
test measures the wait end-to-end (5 ms) rather than asserting that `signal` returned.

**Two orphans removed alongside.** `wait` removed the process entry before awaiting the exit, so a
client that disconnected mid-wait left a live child that `signal`, `stdin` and `state` all reported as
gone; the entry now survives until the exit is reaped. And the post-exit pipe drain was unbounded, so a
grandchild that inherited stdout (`sh -c 'sleep 100 &'`) turned "the process exited" into a request
that never returned — a two-second bound is enough for everything inside the output budget.

**The SSE half of the same idea.** The terminal drain in the streaming handler stopped on `Lagged`,
which is precisely the case where the client is behind and about to miss the frame that ends the turn.
The catch-up path the main loop already used is now a function both call, because "the store is the
record" has to be true in the drain too or the terminal-frame promise is only true when nobody is slow.

## 95. A configuration needs a version before it needs a UI

**Decision.** The configuration is one document with a revision, applied in a single transaction, and
every apply is recorded with the key that signed it.

**What "five write endpoints over a key-value table" cost.** The store had `config/policy`,
`config/models`, `config/sources`, two tenant prefixes and a tokens table — five shapes, five handlers,
no object called "the configuration". The consequences were all operational rather than technical: no
export, so disaster recovery was `sqlite3 system.db .dump`; no revision, so a stale document silently
reverted what an on-call engineer had just changed; and an audit table nothing read, with `key_id`
written as an empty string even though the verifier knew it.

**The validation rule that a test forced.** The first version allowed a document without a policy, and
applying it deleted `config/policy` — after which the running daemon could not rebuild its policy and
refused the next request with "this deployment has no policy". The store's own rule is that a deployment
without one is not a deployment; the apply now says so up front instead of discovering it one request
later.

**And the guard test earned its keep on the same day.** `every_published_route_refuses_a_signature_that
_verifies_nothing` — written for the four unguarded routes — failed on `GET /v1/config/history`, a route
added in the same commit that added the document. It was the first thing that noticed.

## 96. A deployment change is not a conversation's problem

**Decision.** Removing a model a session's route names is allowed; the next turn fails with `not_found`,
a message that names the model and lists the ones that exist, and the session stays open. Applying the
model back makes the same session work again.

**Why allow it at all.** A document is the deployment's truth. Refusing to remove a model because some
session somewhere mentions it would make the configuration a function of history nobody can enumerate —
and the alternative (removing it and failing silently, or keeping an adapter the operator deleted) is
worse. What a caller needs is not "this cannot happen" but "when it happens, I can read what happened".

**The distinction the test pins.** 404 (the model is not there) is a different instruction from 503
(the model is there and could not be reached). An operator reading the first goes to the document; one
reading the second goes to the provider.

**And a validation that came from writing the test.** The first version of the test restored a model
without `context_window`, and the turn failed with "the model declares no context window, so a prompt
cannot be shown to fit" — a correct answer to a question the document should have refused. `0` is not a
default for that field: it means no prompt fits, so an apply now rejects it as a whole rather than
installing a model that fails every session at its first turn.

## 97. A signature that ignores the query covers half the request

**Decision.** The administrative signature covers the request target, query string included. The
verifier uses `uri.path_and_query()`; `uri.path()` drops the query, and that is what it used before.

**The failure.** A captured `GET /v1/config/history?limit=5` could be replayed as `?limit=500` — or
`from_seq`, or whatever filter a later route adds — because the canonical string was built from the path
alone and the two requests signed the same bytes. Nobody would have noticed from the code: the client
signed the URL it sent, the server verified the URL it parsed, and only one of them included the query.

**How it was found.** By writing the CLI's `config history`, which put a query on an admin route for the
first time. The request answered `401` and the instinct is to blame the client's signing — the bug was
the server verifying a shorter string.

**The test.** Sign for `?limit=5`, verify against `?limit=500`: refused as a signature failure (not as a
replay), and the nonce is still unspent, so the request it *was* signed for still works. Asserting the
second half matters: a consumed nonce would have hidden which check failed.

## 98. A history that reads oldest-first answers a question nobody asked

**Decision.** `SystemStore::history` orders newest first, and every writer names the key that signed.

**Two small lies in one table.** The query ordered by `at_unix_millis, rowid` ascending, so `--limit 3`
returned the three *oldest* changes a deployment ever had — while the route's own summary said "newest
first". And `Change.key_id` was written as `String::new()` by the per-endpoint writers even though the
verifier had just established the id, so an audit could prove a change was authorised and not by whom.

Both were invisible because nothing read the table. The first thing that read it was the CLI, and being
the first reader is how a schema's assumptions get checked.

## 99. A timestamp is not a query

**Decision.** Every response carries `nucleos-request-id`, and every request line carries the same id
plus `tenant`, `subject` and `session`.

**The failure it prevents, at 3am.** The line already had method, route, status and duration — and
nothing that joins it to anything. "The inference failed at 14:02" is a timestamp and a guess: there is
no field to search by, no way to tell which session, and no way to correlate the caller's report with the
line. The counter that ignores refusals is deliberate for the same reason: a refused request is the one
somebody will ask about, so the id is minted *outside* authentication.

**Two details that are the whole implementation.**

- **The session comes from the URI, not from the handler.** A handler that sets the correlation is a
  correlation missing from the route somebody added in a hurry, and the sweep test does not cover it.
- **The principal travels on the response.** `observe` wraps `authenticate`, so by the time `next`
  returns, the request's extensions are gone — the principal is copied onto the response in the inner
  layer for the outer one to read. The alternative, reading the header again in the log layer, would
  re-parse a credential to write a log line.

**What it is not.** Not a trace. The event log remains the trace for anything a session owns; this id
answers "which request was that" and stops.

