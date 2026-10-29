# Architecture

The kernel's job is to be **correct**: a session's state never goes backwards, a budget
is never exceeded, an event is never lost. Everything about *policy* — which model,
which tools, who may call what, how much anything costs — is injected.

## 1. Three principles

| Principle | Means | Consequence |
| --- | --- | --- |
| **Mechanism in the kernel, policy outside it** | The kernel guarantees invariants; it never decides what is allowed | The same kernel serves products with different rules without a fork |
| **The session is the process** | The unit of scheduling, quota, isolation and snapshot is the session — not the agent, not the conversation | Resource boundaries are explicit, and crash recovery has an obvious landing spot |
| **Every state change is an event** | No "mutate memory, persist occasionally". Append-only | Recovery, replay, audit and resume are one mechanism instead of four |

An agent is stateless decision logic. A session is the thing that owns resources. Drawing
that line in the wrong place makes everything downstream ambiguous.

## 2. Process and trust boundary

```text
SDK ──HTTP/JSON + SSE (Unix socket by default; TCP + TLS optional)──▶ nucleosd
                                                            │
                          ┌─────────────────────────────────┤
                          │  authn → Principal{tenant, subject}
                          │  proto ↔ kernel translation
                          │  listener / TLS / connection limits
                          └─────────────────────────────────┤
                                                            ▼
                                              nucleos::Kernel  (library)
                                                 ├─ Store   → event log + snapshots
                                                 ├─ Model   → provider adapter
                                                 ├─ Policy  → allow / deny / ask
                                                 └─ Tools   → MCP tool sources
```

Two rules keep the daemon thin:

- **The kernel has no `proto` dependency.** It never sees a wire type. If a value only
  exists on the wire, it is translated in `nucleosd`.
- **The kernel authenticates nobody.** It receives `Principal` as data and passes it to
  policy. There is no signature verification below the daemon.

### 2.1 Tenancy

A tenant is a **label**, not a partition. The kernel carries `tenant` on every session and
passes it to policy; it does not shard storage, does not isolate event logs, and does not
decide that two tenants may not see each other.

That is deliberate. Isolation between tenants is a property of the *deployment* — separate
databases, separate accounts, separate processes — and a kernel that pretended to provide
it through a field would be offering a guarantee it cannot keep.

Two consequences worth stating because they are easy to get wrong:

- `List(tenant)` filters for convenience. **It is not an authorization boundary.** The
daemon decides what a caller may see; the kernel just answers the query it was given.
- A shared store with a `tenant` column is a deployment choice, and the operator must know
it is one.

### 2.3 Session identity is `(tenant, session)`

Two tenants may each own a session called `abc`. The kernel's key is the pair; the id is
what the caller writes, the tenant is what the daemon authenticated.

Session ids are caller-chosen so `Start(resume)` can be idempotent. If the id had to be
globally unique, every caller would have to invent uniqueness across tenants it cannot
see — and the first collision would be a cross-tenant read.

`tenant` travels on the principal, never on a request field. A request that names its own
tenant is a request that can name somebody else's.

### 2.2 Isolation

Isolation is a **declared property of a tool source**, enforced by whoever launches it:

| Descriptor | Who enforces it |
| --- | --- |
| `none` | Trusted in-process code; nothing to enforce |
| `process` | The daemon spawns a child (stdio) or connects to one (http); the sandbox is the OS's |
| `vm` | The daemon launches a VM or a wasm instance |

The kernel records the descriptor, refuses to migrate a session into a generation where it
changed, and otherwise stays out of it. There is no isolator trait in the kernel, because
the kernel never performs the isolation — it only refuses to pretend the descriptor is
free to change.

What the daemon owns, and the kernel does not:

| Concern | What must be true |
| --- | --- |
| spawning | one child per `process` source, with a scrubbed environment — no inherited descriptors, no shared `HOME`, no shared temp directory |
| separation | the agent's runtime and the tool's runtime are physically distinct environments; "the tool inherits the agent's credentials" is the classic incident |
| deadlines | the deadline is enforced on the **call**, not on the process: graceful terminate, then kill |
| egress | whatever the operator configured. The kernel has no opinion and no code path for it |

What the kernel does guarantee, because these are correctness properties and not policy:

- **Every tool call resolves** — with a result, a failure, or a timeout. Never with silence.
- **It resolves against the generation that issued it**, even if a reload landed simultaneously.
- **A dead tool is an unhealthy source, not a crashed kernel.** The call fails; the session
  lives and decides what to do with a failure, which is business logic the kernel does not own.

## 3. Crates

```text
crates/
  nucleos/    contract/ · adapter/ · engine/ · wire/ (envelopes + schema)
  nucleosd/   router · error mapping · listener
schema/       the published JSON Schema, committed
```

The module graph inside `nucleos` is one-way: `contract → engine → adapter`, with `wire` beside
`contract` because it is the same types plus the HTTP envelopes.
A single structural test pins it, so a later edit that reaches upward fails CI instead of
quietly coupling the layers.

## 4. The five contracts

Every extension point is a trait. There are five; a sixth needs an argument.

```rust
pub trait Model: Send + Sync {
    fn capability(&self) -> Capability;                    // tools? required? schema? window?
    fn count_tokens(&self, request: &ModelRequest) -> Result<u64>;
    async fn invoke(&self, request: ModelRequest) -> Result<ModelStream>;
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn isolation(&self) -> Isolation;                      // default: none
    async fn invoke(&self, args_json: &str, cx: &ToolCtx<'_>) -> ToolOutput;
}

pub trait Store: Send + Sync {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<()>;
    async fn snapshot(&self, session: &SessionId) -> Result<Option<Snapshot>>;
    async fn events_since(&self, session: &SessionId, seq: u64) -> Result<Vec<Event>>;
}

pub trait Policy: Send + Sync {
    async fn check(&self, request: PolicyRequest<'_>) -> Verdict;   // Allow | Deny | AskHuman
}

pub trait Clock: Send + Sync { fn now_unix_millis(&self) -> u64; }
```

Four of the five are `async`: the kernel is an I/O orchestrator, and a synchronous signature
would push every implementor toward blocking a runtime worker.

Three shapes are deliberate and easy to get wrong:

- **`Tool::invoke` returns no `Result`.** A failed call is content with `is_error` set, because
  the error text is what the model must read in order to recover. `Err` would leave the model
  with a failure it cannot see.
- **`Model::invoke` does return `Result`.** An error there means the call never started; an
  error *inside* the stream means it started and then failed — which still costs money and
  still has to be recorded.
- **`count_tokens` is required, with no default.** A context budget the kernel cannot compute
  is a budget it cannot enforce, and a built-in "characters divided by four" would be the
  kernel guessing in exactly the place accuracy matters.
- **`Clock` reports unix milliseconds, not `Instant`.** Events carry unix milliseconds and
  `Instant` can neither be serialised nor reconstructed, so a replay could not reproduce one.
  Time is an input, not an ambient fact.

`Tool`'s only production implementation wraps an MCP tool source (§8). The trait stays
separate so the engine can be tested with a two-line fake, and `adapter::fake` ships those
fakes because a fake only the kernel's tests can reach is a fake every host writes again.

## 5. Session state machine

```text
Created → Running ─┬→ AwaitingTool ──→ AwaitingHuman   (a batch that needs a human)
                   ├→ AwaitingHuman                    (HITL is a state, not a callback)
                   ├→ Cancelling ───→ Done | Failed | Cancelled
                   └→ (turn boundary) Checkpoint
```

A tool turn asks policy about the whole batch *before* running any of it, so a batch that needs
a human parks with nothing executed — which is why the edge from `AwaitingTool` to
`AwaitingHuman` exists.

Three hard constraints:

1. **Checkpoints are written only at a turn boundary.** Nothing half-finished is ever
   persisted, which is what makes recovery a small problem instead of a research problem.
2. **Exactly one writer per session.** A session is driven by one task; state is never
   handed out as `&mut`. No locks are needed on the hot path.
3. **Stopping is four layers deep.** `finish_reason` → step cap → wall clock → cost
   ledger. Each one alone can be defeated; together they cannot.

## 6. Turn loop

```text
Observe  build the prompt: system + history + tool results (+ retrieval)
Think    model.invoke → stream deltas
Act      policy.check(tool_call) → tool.invoke (possibly concurrent, fan-out)
Check    fold results back, validate, extend the ledger, test the stop conditions
Repeat   advance state, or leave the loop and checkpoint
```

Within one turn, tool calls may run concurrently; their results are folded back in
`ToolCallId` order. State transitions are serial, tool execution is parallel — those are
different axes and must not be collapsed.

A tool's answer is a **`Content` plus an `is_error` flag**, not a string and not a
success/failure branch. Content folds into history without an adapter, so a tool that
returns text and an image uses one code path; and a *failed* call still carries content,
because the error text is what the model must read to recover. The only restriction is that
a tool result's content may not contain another tool call — `Content` and `Part` are
mutually recursive, so that rule is what keeps the nesting one level deep.

### 6.1 Selection, retries and the ledger

A session carries a **route**, not a model name: an ordered chain plus a retry count. The
kernel walks it — first target, retries against it, then the next — and records every attempt.

```text
Attempt { index, model, phase, trigger, outcome, usage, duration, degraded[] }
Ledger  { attempts[], total }
```

Three properties make the ledger usable:

- **A failed attempt that streamed still pays.** Its usage is folded into the total, because
  dropping it under-reports the bill.
- **An unreported counter stays unreported.** Adding an unknown as zero is the same lie told
  more quietly.
- **The first attempt defines the total, later ones add to it.** A fresh total is
  zeros-and-unknown, and "nothing recorded yet" is not the same as "a report was missing".

Routing lives in the kernel because the alternative is a runtime whose availability is its
provider's, and because the ledger has to be produced by whoever chose the target. Model
lookup is a plain registry, not a sixth trait: routing needs lookup, and lookup does not need
polymorphism.

`ToolChoice::Required` belongs to the same story. "The model explained the problem instead of
calling the tool" is the most common way an agent fails, and forcing a call is the only fix
that does not involve asking the prompt more nicely.

## 7. Why there is no gateway

`nucleos` is an agent OS, not a model gateway. There is no stateless "invoke a model" endpoint, and
adding one was a mistake that this section exists to prevent from coming back.

The argument for it was that normalization, routing, retries and cost accounting are one package,
so a caller who wants one wants all of them. That argument is real — and it is an argument about
*inside a session*, which is where all four already live. A host that wants a raw model call does
not need an ABI for it: it links the library.

What a gateway endpoint would cost is not code, it is coherence. Once `/generate` exists, the
question "why can't it embed?" has no answer except "yet" — and the honest answer, that embeddings
are a tool, becomes impossible to give.

Three things follow, and they are decisions rather than omissions:

- **Embeddings, speech recognition and speech synthesis are tools.** An embedding model belongs
  with the index and the ranking function that use it (§11), and speech in or out is a media
  pipeline, not a decision the kernel makes. A tool source is exactly where a provider-specific
  pipeline belongs.
- **A tool may report what it spent.** [`ToolOutput::usage`] is optional: a tool that calls a paid
  API can say so, and the kernel folds it into the session's usage and the ledger. A tool that
  reports nothing stays unreported — which is the same rule model usage follows, and the reason
  the ledger never quietly counts an unknown as zero.
- **The kernel's own calls are attributed.** A compaction summary is a model call the kernel makes
  for itself, and `Purpose::Summary` is how its spend is told apart from the answer's.

Cost attribution is the one thing a gateway endpoint would have bought that a tool cannot. Giving
tools a way to report is the smaller, more honest fix: the kernel attributes what it is told and
says so when it is told nothing.

## 8. Tool sources and generations## 8. Tool sources and generations

Tools are declared as **sources**, not as definitions pushed through the ABI. A source is
an MCP endpoint:

| Field | Meaning |
| --- | --- |
| `name` | source identity |
| transport | `stdio` (command) or `http` (URL + auth) |
| `allow` | which advertised tools are exposed to sessions |
| `isolation` | `none` / `process` / `vm` |

The daemon connects, calls `tools/list`, and snapshots the resulting specs into a
**generation**. A generation is an immutable set of tool sources:

```text
Start               → session pinned to the current generation G
Register(new source) → build G+1, latest := G+1
G.refcount == 0      → retire, close connections
```

Because the set is immutable, compatibility is a pure function of two generations:

- **Compatible** — same tool names, structurally equal input schemas (new *optional*
  properties allowed), identical isolation descriptor. The session migrates to the latest
  generation **at its next turn boundary**, recorded as a `generation_migrated` event.
- **Incompatible** — the session stays pinned to its generation until it ends.

Compatibility is decided by schema structure, not by a version number somebody typed.

A mid-turn swap is never allowed. A tool call that has been issued must resolve against
the generation that issued it.

## 9. Budget is credit

Budget is enforced as flow control, not as a post-hoc check: the caller grants credit,
each `usage` event spends it, exhaustion drains the session and writes a checkpoint.

| Layer | Enforced by |
| --- | --- |
| tokens / cost | credit from the caller, debited per model call |
| steps | engine, per turn |
| wall clock | engine, via `Clock` |
| policy | `Verdict::Deny` before the tool runs |

The ledger records every attempt — including attempts that failed after streaming a
partial answer. Under-reporting cost is the one direction nobody forgives.

## 10. Persistence

```text
Store:  events[sid] = [(seq, at, Event)]        append-only
        snapshot[sid] = latest materialised state
recover: load(snapshot) → replay(events_since(snapshot.seq))
```

That single choice buys four things: **recovery** after a crash, **replay** for tests and
evaluation (recorded model deltas are fed back, no new calls), **audit** (a `denied` event
is evidence), and **resumption** (`Run(from_seq)` needs no extra machinery).

The cost is event-schema evolution, paid up front: every event carries `schema_version`
and there is an upcaster table from day one.

## 11. Context management

A session will outlive its model's context window. Three mechanisms, in order of who owns
them:

| Mechanism | Owner | What it does |
| --- | --- | --- |
| **budget** | kernel | counts tokens against the window; refuses a turn that cannot fit |
| **compaction** | kernel | at a turn boundary, replaces a span of history with a summary event |
| **page-in** | a tool | retrieves text the compaction dropped, when the model asks |

The split is the point. The kernel only does what must be exact — counting and replacing
history in a way that replays. Retrieval is a tool, because a tool is where an embedding
model, an index and a ranking function belong.

### 10.1 What must fit

```text
prompt_tokens + max_output_tokens <= context_window
```

The window comes from the model's capability declaration and `max_output_tokens` from the
session's budget. Both are inputs, neither is discovered by trial and error: a provider
that silently truncates is a provider that silently changes the answer.

A session whose *first* turn cannot fit even with empty history fails at `Start`. Failing
on every turn forever is not a policy, it is a bug with a schedule.

### 10.2 Compaction

Trigger: the next prompt would not fit, and there is history to drop.

Policy, written down so it can be tested rather than inferred from behaviour:

- the system prompt is never compacted;
- the first user message is never compacted — it is the intent;
- the most recent N turns stay verbatim (config, default 4);
- everything between them is replaced by one summary, produced by one model call.

The summary is a model call: it consumes budget, it is charged, and it can fail. A failed
compaction stops the session with `StopReason::Tokens`. Pretending a smaller prompt is
equivalent is how a session starts being confidently wrong about its own past.

### 10.3 Page-in

The summary carries **artifact handles**, not text: markers naming the spans it replaced. A
`recall` tool resolves a handle back to the original text.

That is the whole mechanism. The result arrives as an ordinary tool result, so it enters
history on the normal path — no kernel special case, no hidden state, and a model that is
wrong about needing it has simply made a recoverable mistake.

### 10.4 Why the log is never edited

Compaction **adds** an event; it never rewrites or deletes one. The event says, in effect:
"when constructing a prompt, treat the events in `[a, b)` as this summary".

Three properties follow, and none of them survive in-place editing of history:

- the event log stays append-only, so it stays replayable;
- the `seq` space stays dense, so a resume cursor still works;
- an auditor can see what the model was actually told, and what was dropped to make room.

The event kind lands with P3 as a **purely additive** change to `nucleos.v1` — a new member
of the `Event.kind` oneof, no version bump, no field renumbered.

## 12. Telemetry

Names are not ours to invent. Attribute names follow the OpenTelemetry GenAI conventions
(`gen_ai.usage.input_tokens`, `gen_ai.request.model`, `gen_ai.operation.name`), and trace
context follows W3C Trace Context: a `traceparent` rides in session metadata so one span
continues across SDK → daemon → provider.

The reason is not tidiness. A bespoke naming scheme means a bespoke pipeline, which means the
traces exist and nobody looks at them.

The kernel emits rather than exports: it produces events, and the daemon turns them into
spans, metrics and logs. The kernel has no OTLP dependency, and adding one would put network
I/O on the hot path of every turn.

## 13. Non-goals

| Not building | Why |
| --- | --- |
| A scheduler or memory allocator | `tokio` and the OS already provide them; we schedule turns |
| A wire protocol to our own tools | MCP exists; we are a client |
| Identity, signing, certificates | The daemon's job; the kernel takes `Principal` as data |
| Cluster placement, supervision, self-healing | A platform concern; the kernel does not know how many nodes exist |
| A provider-specific parameter bag | One extension hatch is enough; the five parameters every provider has are carried explicitly |
| Multiple ABI bearers (in-process / child / remote as one abstraction) | The most expensive kind of debt: every interface is forced to the most conservative shape |
| Live tool swapping inside a turn | Replaced only at turn boundaries |
| Auto-triggered reload (file watch) | Deferred; reload is explicitly requested |

## 14. Stages

| Stage | Content | Passes when |
| --- | --- | --- |
| P1 | contract + engine + in-memory store + one model adapter + one MCP source | a multi-turn session with a tool call runs end to end |
| P2 | SQLite store, checkpoint/restore, credit enforcement | `kill -9` then resume continues the session |
| P3 | context management: budget, compaction, retrieval references | a session stays coherent past the model's context window |
