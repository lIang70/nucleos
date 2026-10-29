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

### 2.2 Isolation

Two layers, and the split is the whole design:

| Layer | Who sets it | Where it lives |
| --- | --- | --- |
| **Policy** — what a session's commands may touch | the operator | `SessionSpec.sandbox`, carried in the checkpoint |
| **Permissions** — what one command asks for on top | the caller | a field of that one `exec` |

`SandboxPolicy` is `danger_full_access`, `read_only`, `workspace_write` or `external_sandbox`,
each with a network flag. `SandboxPermissions` is `use_default`, `require_escalated`, or
`with_additional_permissions`.

The policy is **not** a request field. A caller that can name its own policy can name
`danger_full_access`, and a permission slip the untrusted side writes is not a permission model.

Nothing widens without a gate. An escalation becomes `Gated::Exec` — the policy sees the argv, the
session's policy and what was asked for — and the usual answer applies: allow, deny, or ask a
human. `with_additional_permissions` only ever adds; there is no field for removing a restriction,
because narrowing by request would let one command change what a later command may assume.

The mechanism — seatbelt, bwrap with landlock, a Windows restricted token — sits behind the
`exec` syscall and is never named in the contract. A deployment that runs on none of them still
has a working `exec`; it just runs with the isolation the platform can give.

### 2.3 Session identity is `(tenant, session)`

Two tenants may each own a session called `abc`. The kernel's key is the pair; the id is
what the caller writes, the tenant is what the daemon authenticated.

Session ids are caller-chosen so `Start(resume)` can be idempotent. If the id had to be
globally unique, every caller would have to invent uniqueness across tenants it cannot
see — and the first collision would be a cross-tenant read.

`tenant` travels on the principal, never on a request field. A request that names its own
tenant is a request that can name somebody else's.

### 2.4 What the kernel still guarantees

Two properties survive every rearrangement above, because they are correctness rather than
policy:

- **Every call resolves** — with a result, a failure, or a timeout. Never with silence.
- **A dead tool is an unhealthy source, not a crashed kernel.** The call fails; the session lives
  and decides what to do with a failure, which is business logic the kernel does not own.

## 3. Crates

```text
crates/
  nucleos/         contract/ · adapter/ · kernel/ (syscalls) · wire/
  nucleos-agent/   the reference loop, built on those syscalls
  nucleos-openai/  one provider adapter: the OpenAI-compatible family
  nucleosd/        router · error mapping · listener
openapi/           the published ABI, committed
```

`nucleos-openai` exists as its own crate because the kernel must not depend on an HTTP client: the
kernel owns the mechanism, and a provider's dialect is an adapter like any other.

`nucleos-agent` depends on `nucleos` and nothing depends on `nucleos-agent` except the daemon:
the loop is the application's, and the kernel must not learn it.

The module graph inside `nucleos` is one-way: `contract → kernel → adapter`, with `wire` beside
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
separate so the kernel can be tested with a two-line double.

That double lives in `nucleos-testkit`, which no binary depends on. It used to live in
`nucleos::adapter::fake`, in the production crate, and the daemon ended up *running* it: an
in-memory store and an allow-everything policy shipped as the deployment. A fake in the crate
everything links is a fake one `use` away from being the product, and nothing in a build says which.

Three of the five adapters now ship with a real implementation instead: `SystemClock`, because
reading the time is not a decision; `nucleos-sqlite`'s store, because a test that runs against a
different store than production is a test of a store nobody deploys; and `nucleos-policy`, because
"no policy configured" was, until recently, what the daemon actually ran.

`AllowAll` and `DenyAll` stay in the kernel crate as adapters rather than stand-ins — they are
answers, not substitutes — but the daemon no longer picks one by default. It reads `NUCLEOS_POLICY`
and refuses to start without it; `"default": "allow"` in that file is how a deployment says "gate
nothing", in a file that is versioned and reviewed.

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

## 8. Tool sources and generations

Tools are declared as **sources**, not as definitions pushed through the ABI. A source is
an MCP endpoint:

| Field | Meaning |
| --- | --- |
| `name` | source identity |
| transport | `stdio` (command) or `http` (URL + auth) |
| `allow` | which advertised tools are exposed to sessions |

Isolation is not a field a source declares. It is **derived** by the daemon from how it reached
the source — a spawned child, an HTTP endpoint, an in-process closure — because that is a fact the
daemon owns. A source asserting its own isolation would be an unchecked claim dressed as a
verified one: this design had exactly that field once (`Tool::isolation`), no consumer ever read
it, and it was removed rather than left to look like a guarantee. The descriptor returns with the
generation check below, computed rather than declared.

The daemon connects, calls `tools/list`, and snapshots the resulting specs into a
**generation**. A generation is an immutable set of tool sources:

```text
Start               → session pinned to the current generation G
Register(new source) → build G+1, latest := G+1
G.refcount == 0      → retire, close connections
```

Because the set is immutable, compatibility is a pure function of two generations:

- **Compatible** — same tool names, structurally equal input schemas (new *optional*
  properties allowed), and the same derived isolation descriptor. The session migrates to the
  latest generation **at its next turn boundary**, recorded as a `generation_migrated` event.
  The isolation term is the one that cannot be dropped: a tool whose name and schema are unchanged
  but which now runs in-process where it used to run in a sandboxed child is a silent downgrade
  that every other check passes.
- **Incompatible** — the session stays pinned to its generation until it ends.

Compatibility is decided by schema structure, not by a version number somebody typed.

#### A source dies, and a tool outlives it

A source is a child process, so it stops: it crashes, someone deploys over it, the host reclaims
memory. Every tool it advertised stays registered in a generation, and a session may be pinned to
that generation for days. So the connection lives behind a shared cell that a call can rebuild, and
the tools keep their identity across the rebuild — otherwise every crash would be a new generation
and every session in flight would need to migrate.

Three rules make the rebuild safe:

- **The failed call is not retried.** A tool call is not assumed idempotent, and the server may have
  run it before the pipe broke. The failure is reported as content with the reason, the connection
  is restored, and the *next* call is the retry — a deliberate one, by whoever can read what
  happened.
- **A reconnect may not change the answer.** The tools are pinned at connect time. A server that
  comes back advertising a different set is refused, because adopting it would leave a generation
  describing declarations that no longer exist — and the session calling names it was never shown.
  Changing the set is a new generation, which is a different mechanism.
- **A source that cannot restart backs off.** Attempts double from 250ms to 30s and are serialised
  through one mutex, so a source whose binary was moved is not spawned once per call, and two
  concurrent calls never produce two children. Callers inside the window are told when the next
  attempt is, rather than being handed a spawn failure they can do nothing with.

The cell also decides what a *failure* is: only a transport-level error means the child is gone. A
tool that answered with `isError` answered — the model has to read that, and reconnecting over it
would turn a refusal into a crash report.

A mid-turn swap is never allowed. A tool call that has been issued must resolve against
the generation that issued it.

### 8.1 Discovery: exposure is not permission

A prompt cannot carry every tool's schema. Each one costs 100–300 tokens, so thirty tools is
3–9k tokens of fixed overhead on every call — and a model choosing between sixty tools chooses
worse than one choosing between eight. The failure is silent: nothing errors, the answers just get
slower and duller.

So a session has **two** tool sets, and they mean different things:

| | Who sets it | Rule |
| --- | --- | --- |
| **ceiling** (`config.tools`) | the application, at `open` or `configure` | may only narrow |
| **exposure** (`config.tool_policy`) | the application, or a search tool asking | always within the ceiling |

Everything is `deferred` by default. A session that has configured nothing sees exactly one tool —
`tool_search` — and reaches the rest through it. The alternative, showing everything unless told
otherwise, is the behaviour discovery exists to replace, so it is not the default: it would make
the twenty-ninth tool as visible as the first, which is the failure this whole mechanism is for.
`always` is the opt-in baseline, and it should stay short.

Two budgets, and the distinction is the design:

- **`policy.budget`** (32 definitions / 16 KiB) caps one prompt.
- **`policy.discovery`** (32 tools / 16 KiB / 10 idle turns) caps the session's **pool**: what it
  remembers having found. A round can show eight tools and the session may still remember thirty;
  forgetting one it used two turns ago would make it search twice for the same thing.

The pool expires. A tool found on turn 3 and never used again should not still be displacing a
baseline on turn 40, so every turn ages every entry and `idle_rounds` retires them. Eviction order
is by exposure rank first, then by staleness: `always` outranks `direct` outranks `deferred`,
because a baseline that can be evicted by its own discoveries is not a baseline.

#### What a search changes

A search changes exactly one thing: names enter the pool. The search result is content — a line per
hit, name and summary — and the *display* consequence arrives in the next prompt, because the turn
being answered was built before the result existed. There is no way around that ordering, and
pretending otherwise would mean mutating a prompt mid-flight.

The next prompt is then built in four steps, in this order:

1. **ceiling** — drop anything `config.tools` does not allow;
2. **policy** — `always` is in, `hidden` is out, `direct`/`deferred` are in only while the pool
   holds them;
3. **order** — by exposure rank, then most recently reached, then name, so two sessions with the
   same history build the same prompt;
4. **budget** — by count, then by bytes.

Two details in step 4 are load-bearing. The baseline is admitted *whatever the budget says*: an
application's own `always` set vanishing because discovery filled the room would be the
configuration overridden by the thing meant to enforce it, and a prompt that loses its door cannot
ask for it back. And a schema that does not fit the byte budget is **skipped, not treated as the
end of the list** — the order is by rank and recency, not by size, so breaking on the first
oversized schema would evict every smaller tool behind it.

`tools_exposed` names what reached the prompt, not what entered the pool. The two differ whenever
the budget is full: a discovered tool that did not fit is still callable by name — the prompt is a
view of the pool, not the whole of it — but an event claiming it entered the prompt would be a
second, wrong truth.

`hidden` exists so that "do not advertise this" and "do not allow this" stay separable. A hidden
tool is still callable by name and still gated; only the prompt forgets it.

Exposure is a *display* concern. A tool that is not exposed can still be called by name, and
execution goes through the same gate either way — narrowing exposure therefore cannot grant a
permission, and widening it cannot bypass one. Conflating the two would make "hide the tool" and
"forbid the tool" the same act, and then the only way to forbid would be to hide, which the model
would experience as a tool that exists one turn and not the next.

A tool cannot widen its own exposure: it has no handle on the session. `ToolOutput.expose` is a
**request**, and the kernel answers it — intersecting with the ceiling, recording a
`tools_exposed` event, and applying the result from the next prompt on. The event is not
bookkeeping: a replay has to know which tools the model could see at turn 42, for the same reason
it has to know which system prompt was in force.

#### Ranking

Discovery ranks with BM25 over each tool's name and description. Four rules, each for a reason:

- **A name hit outscores a description hit** (3 against 1). `webfetch` in a name is a stronger
  signal than the word "fetch" in a paragraph.
- **A query term matches by prefix, at half weight.** `webf` still finds `webfetch`.
- **Per field, a query term contributes its best match, never the sum of its prefix matches.**
  Otherwise a document that merely prefixes four expansions of one term outranks the one that
  contains it — the exact misfire that makes naive prefix search feel broken.
- **CJK is indexed as single characters plus adjacent bigrams.** Han, Kana and Hangul have no
  whitespace boundaries; bigrams match fragments without a segmentation dictionary.

The corpus is a few hundred short documents, so the index is ~300 lines of inverted lists, not a
search engine: `tantivy` here would be a deployment dependency for a table of contents.

The search tool is shown only when something is behind it: if every allowed tool is `always` or
`hidden`, nothing is searchable, the door is hidden too, and the prompt carries no schema for a room
that is empty. That is how a three-tool deployment avoids paying for search — by declaring
`default: always`, not by the kernel noticing the catalog is small.

There is deliberately **no size threshold**. An earlier version skipped the policy entirely below
twelve tools, which meant a `hidden` tool was shown whenever the catalog was small and hidden the
moment a twelfth tool was registered by somebody else. A silent override of an explicit
configuration is worse than a redundant schema, and the configuration is where the intent already
lives.

## 9. Budget is credit

Budget is enforced as flow control, not as a post-hoc check: the caller grants credit,
each `usage` event spends it, exhaustion drains the session and writes a checkpoint.

| Layer | Enforced by |
| --- | --- |
| tokens / cost | credit from the caller, debited per model call |
| steps | the driver, per turn |
| wall clock | the kernel, via `Clock` |
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

A session outlives its model's context window. Three mechanisms, and the split between them is the
design:

| Mechanism | Owner | Why there |
| --- | --- | --- |
| **measure and refuse** | kernel | it is the only place holding the window, the reserve and the prompt at once, and a silent truncation is banned |
| **compaction** | the caller | it is editing the caller's message list, and there are many strategies |
| **page-in** | a tool | an index, an embedding model and a ranking function belong in a tool |

The reasoning behind the middle row is worth stating, because the obvious design is wrong. The
kernel does not assemble prompts: `inference(session, messages)` takes the conversation from the
caller. Compaction *is* editing that list, so a kernel that compacted would be rewriting data it does
not own — and it would have to pick one strategy, in a place where summarising, sliding windows,
tool-result eviction, importance marking and retrieval all exist and fail differently.

So the kernel does the part that must be exact, and only that.

### 11.1 What must fit

```text
prompt_tokens + completion_reserve <= context_window
```

Checked before the call, by `count_tokens` — the one method on the model contract that had never
been called, while this section claimed the check existed. The reserve is the request's
`max_output_tokens` when it sets one, and the model's declared maximum otherwise.

Three refusals, each for a failure that is otherwise silent:

- **Over the window** — the turn fails with `Invalid`. The caller compacts or lowers the reserve.
  Sending it anyway is a provider that truncates, which changes the answer without saying so.
- **No declared window** — refused rather than treated as unlimited. "No stated limit" cannot mean
  "send anything"; that is the same truncation, moved one layer down.
- **A counting failure** — fails the turn. A budget the kernel cannot compute is a budget it cannot
  enforce, and `count_tokens` is required by the contract for exactly this.

The check runs inside the route, so a fallback model with a larger window still gets its chance —
the prompt did not fit *that* model, which is a fact about one target, not about the call.

### 11.2 Compaction is the caller's

The kernel offers no summariser, no window policy, no "recent N turns". What it offers is a way to
say what happened:

```rust
kernel.compact(&mut session, replaced_from, replaced_to, summary)
```

That appends `history_compacted` and does nothing else. The span addresses the messages the caller
passed, and the summary is what the model was given instead. An empty or backwards span is refused:
a caller reporting that it dropped nothing is reporting a bug, and a log entry saying so would be
worse than the error.

What this buys, for one event: a replay can reconstruct the prompt the model actually saw. Without
it, the log says one thing and the request said another, and an auditor cannot tell compaction from
a bug.

What it costs, stated plainly: **the kernel no longer promises that a long conversation keeps
running.** Its promise is "you are told, not truncated" — a session whose caller never compacts ends
at `StopReason::Tokens`. A kernel that answered one question for everyone would be answering it
wrongly for most.

### 11.3 Page-in

The summary may carry **artifact handles** rather than text: markers naming the spans it replaced. A
`recall` tool resolves a handle back to the original text.

That is the whole mechanism, and it belongs to the caller as well. The result arrives as an ordinary
tool result, so it enters history on the normal path — no kernel special case, no hidden state, and a
model that is wrong about needing it has made a recoverable mistake.

### 11.4 Why the log is never edited

Compaction **adds** an event; it never rewrites or deletes one. Three properties follow, and none of
them survive in-place editing:

- the event log stays append-only, so it stays replayable;
- the `seq` space stays dense, so a resume cursor still works;
- an auditor can see what the model was told, and what was dropped to make room.

The kernel's own copy of the conversation is the record; the prompt is a view of it. That is why a
caller's compaction changes the view without touching the log.

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
| P1 | contract + kernel + in-memory store + one model adapter + one MCP source | a multi-turn session with a tool call runs end to end |
| P2 | SQLite store, checkpoint/restore, credit enforcement | `kill -9` then resume continues the session |
| P3 | context management: budget, compaction, retrieval references | a session stays coherent past the model's context window |
