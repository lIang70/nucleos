# Architecture

The kernel's job is to be **correct**: a session's state never goes backwards, a budget
is never exceeded, an event is never lost. Everything about _policy_ — which model,
which tools, who may call what, how much anything costs — is injected.

## 1. Three principles

| Principle                                      | Means                                                                                                      | Consequence                                                                      |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| **Mechanism in the kernel, policy outside it** | The kernel guarantees invariants; it never decides what is allowed                                         | The same kernel serves products with different rules without a fork              |
| **The session is the process**                 | The unit of scheduling, quota, isolation and snapshot is the session — not the agent, not the conversation | Resource boundaries are explicit, and crash recovery has an obvious landing spot |
| **Every state change is an event**             | No "mutate memory, persist occasionally". Append-only                                                      | Recovery, replay, audit and resume are one mechanism instead of four             |

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

That is deliberate. Isolation between tenants is a property of the _deployment_ — separate
databases, separate accounts, separate processes — and a kernel that pretended to provide
it through a field would be offering a guarantee it cannot keep.

Two consequences worth stating because they are easy to get wrong:

- `List(tenant)` filters for convenience. **It is not an authorization boundary.** The
  daemon decides what a caller may see; the kernel just answers the query it was given.
- A shared store with a `tenant` column is a deployment choice, and the operator must know
  it is one.

### 2.2 Isolation

Two layers, and the split is the whole design:

| Layer                                              | Who sets it  | Where it lives                                   |
| -------------------------------------------------- | ------------ | ------------------------------------------------ |
| **Policy** — what a session's commands may touch   | the operator | `SessionSpec.sandbox`, carried in the checkpoint |
| **Permissions** — what one command asks for on top | the caller   | a field of that one `exec`                       |

`SandboxPolicy` is `danger_full_access`, `read_only`, `workspace_write` or `external_sandbox`,
each with a network flag. `SandboxPermissions` is `use_default`, `require_escalated`, or
`with_additional_permissions`.

The policy is **not** a request field. A caller that can name its own policy can name
`danger_full_access`, and a permission slip the untrusted side writes is not a permission model.

Nothing widens without a gate. An escalation becomes `Gated::Exec` — the policy sees the argv, the
session's policy and what was asked for — and the usual answer applies: allow, deny, or ask a
human. `with_additional_permissions` only ever adds; there is no field for removing a restriction,
because narrowing by request would let one command change what a later command may assume.

The mechanism sits behind the `exec` syscall and is never named in the contract. macOS has one:
**seatbelt**, via `sandbox-exec` and a profile generated from the policy.

The profile denies by default and allows exactly what the policy names: reading everywhere (no
policy in this ABI restricts reading, and a sandbox that breaks library loading is one nobody
keeps), spawning processes, signalling itself, writing to the standard devices, writing under the
roots the policy names, and reaching the network only when the policy says so.

Three decisions worth stating, because each is a place where a friendlier implementation would be
lying:

- **No implicit temporary directory.** `workspace_write` means the workspace. A tool that needs
  `TMPDIR` names it in `extra_writable_roots`. Quietly widening so one program is comfortable makes
  the edge of the sandbox unpredictable, which is the one property it must have.
- **Roots are named twice when they are symlinks.** Seatbelt matches resolved paths, and on macOS
  `/var` is a symlink to `/private/var` — so a profile naming only what the caller typed denies
  writes to the very directory it meant to allow. This was found by the first end-to-end test.
- **Paths are escaped, not trusted.** A workspace under a directory with a quote in its name would
  otherwise close the string literal and turn the rest of the path into profile syntax.

`require_escalated` that the gate has approved runs **outside** the sandbox, because that is what
an escalation means; the gate is then the only protection, which is why the contract says the gate
turns it into a question rather than a formality.

**How it is configured.** Two places, and neither is a request field that a command can rewrite:

```jsonc
POST /v1/processes
{
  "argv": ["sh", "-c", "make test"],
  "sandbox": {                            // session-scoped, decided here, carried in the checkpoint
    "workspace": "/srv/ws",
    "policy": { "kind": "workspace_write", "network_access": false }
  },
  "permissions": {                        // this command only, additive
    "kind": "with_additional_permissions",
    "additional": { "writable_roots": ["/srv/cache"], "network_access": true }
  }
}
```

`config.tools`-style narrowing **does** apply, and the first draft of this section said it did not:
the policy was declared immutable, which was one rule stricter than the design needed and denied the
one operation an operator actually wants mid-incident.

So `ConfigPatch.sandbox` exists and `narrows_to` decides. Narrowing is a configuration change:
`workspace_write` → `read_only`, dropping an extra root, turning the network off. Widening is
refused — `read_only` → `workspace_write`, adding a root, `danger_full_access` — because more room
for a whole session is an escalation, and an escalation is decided about _one command_ by the gate,
with a human in the loop. The two are different acts and they keep different doors.

The order is by what a process may touch, not by name: `read_only` is narrower than
`workspace_write` because it grants no writes at all. `external_sandbox` is not a rung on that ladder
— it means the kernel enforces nothing — so entering it from an enforced policy is widening, and
leaving it is not.

**What the log records.** `exec_started` carries `mechanism` and, when the mechanism takes one,
`profile` — the text handed to `sandbox-exec`. A sandboxed refusal is "Operation not permitted" and
nothing else, so without the profile the log cannot say which rule was missing, and answering means
rebuilding a profile from a policy that may since have changed. `mechanism: null` is a decision too:
it says nothing confined this command, which is the difference between `danger_full_access` and a
sandbox that failed to start.

**Linux is bubblewrap.** `bwrap --unshare-all` gives the command its own user, pid, network, ipc and
uts namespaces; `/` is bound read-only and each writable root is bound on top; `--die-with-parent`
means a killed kernel leaves no unreapable process, and `--new-session` keeps a command off the host's
terminal. `--share-net` appears only when the policy allows the network — the one place the blanket
`--unshare-all` is bent, said in the same breath as the rule it bends.

Landlock is not wired up. It would add path rules _inside_ those mounts, and the bind mounts already
answer "may this command write here"; a second mechanism that can disagree with the first is a second
thing to get wrong. It becomes interesting the day a policy needs to restrict _reading_, which none
in this ABI do.

What is verified where is stated rather than implied: the command line is built by a pure function
tested on every platform, the end-to-end cases run on Linux (CI installs bubblewrap), and they **skip
loudly** where the kernel forbids unprivileged user namespaces instead of passing by accident. The
`bwrap` binary is checked for before the command starts, so a missing dependency is a sentence naming
the dependency rather than a spawn error naming nothing.

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
  error _inside_ the stream means it started and then failed — which still costs money and
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
`nucleos::adapter::fake`, in the production crate, and the daemon ended up _running_ it: an
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

A tool turn asks policy about the whole batch _before_ running any of it, so a batch that needs
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
returns text and an image uses one code path; and a _failed_ call still carries content,
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
_inside a session_, which is where all four already live. A host that wants a raw model call does
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

| Field     | Meaning                                        |
| --------- | ---------------------------------------------- |
| `name`    | source identity                                |
| transport | `stdio` (command) or `http` (URL + auth)       |
| `allow`   | which advertised tools are exposed to sessions |

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

- **Compatible** — same tool names, structurally equal input schemas (new _optional_
  properties allowed), and the same derived isolation descriptor. The session migrates to the
  latest generation **at its next turn boundary**, recorded as a `generation_migrated` event.
  The isolation term is the one that cannot be dropped: a tool whose name and schema are unchanged
  but which now runs in-process where it used to run in a sandboxed child is a silent downgrade
  that every other check passes.
- **Incompatible** — the session stays pinned to its generation until it ends.

Compatibility is decided by schema structure, not by a version number somebody typed.

**What is built.** `Kernel::publish(tools)` adds a generation and pins new sessions to it; a session
resolves tools through the generation it is pinned to, not the newest one, which is the whole point:
a call is answered by the set that built the prompt. `inference` is the turn boundary, and there
`compatible` decides — same names, and input schemas where nothing new is required and nothing old
changed. Compatible sessions move and record `generation_migrated`; incompatible ones stay pinned for
the rest of their lives, and keeping working, because the set they reference is still held.

**What is not built.** Retirement. Every generation this kernel published stays in memory, because
freeing one requires a durable answer to "which sessions still reference it", and the store can only
give that by reading every snapshot. It is a small leak on purpose, and the alternative — freeing a
set a pinned session might reach for — is a correctness bug.

**Reload is over the ABI.** `POST /v1/tools/reload` reconnects the configured source, publishes what
it advertises now, and answers with the new generation plus what was added and removed. `GET
/v1/describe` reports the generation, so a caller can tell which set it is talking to. A reload is not
a restart: sessions already running keep the set they are pinned to, and the `added`/`removed` lists
describe what changed for sessions that start afterwards.

#### A source dies, and a tool outlives it

A source is a child process, so it stops: it crashes, someone deploys over it, the host reclaims
memory. Every tool it advertised stays registered in a generation, and a session may be pinned to
that generation for days. So the connection lives behind a shared cell that a call can rebuild, and
the tools keep their identity across the rebuild — otherwise every crash would be a new generation
and every session in flight would need to migrate.

Three rules make the rebuild safe:

- **The failed call is not retried.** A tool call is not assumed idempotent, and the server may have
  run it before the pipe broke. The failure is reported as content with the reason, the connection
  is restored, and the _next_ call is the retry — a deliberate one, by whoever can read what
  happened.
- **A reconnect may not change the answer.** The tools are pinned at connect time. A server that
  comes back advertising a different set is refused, because adopting it would leave a generation
  describing declarations that no longer exist — and the session calling names it was never shown.
  Changing the set is a new generation, which is a different mechanism.
- **A source that cannot restart backs off.** Attempts double from 250ms to 30s and are serialised
  through one mutex, so a source whose binary was moved is not spawned once per call, and two
  concurrent calls never produce two children. Callers inside the window are told when the next
  attempt is, rather than being handed a spawn failure they can do nothing with.

The cell also decides what a _failure_ is: only a transport-level error means the child is gone. A
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

|                                     | Who sets it                               | Rule                      |
| ----------------------------------- | ----------------------------------------- | ------------------------- |
| **ceiling** (`config.tools`)        | the application, at `open` or `configure` | may only narrow           |
| **exposure** (`config.tool_policy`) | the application, or a search tool asking  | always within the ceiling |

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
hit, name and summary — and the _display_ consequence arrives in the next prompt, because the turn
being answered was built before the result existed. There is no way around that ordering, and
pretending otherwise would mean mutating a prompt mid-flight.

The next prompt is then built in four steps, in this order:

1. **ceiling** — drop anything `config.tools` does not allow;
2. **policy** — `always` is in, `hidden` is out, `direct`/`deferred` are in only while the pool
   holds them;
3. **order** — by exposure rank, then most recently reached, then name, so two sessions with the
   same history build the same prompt;
4. **budget** — by count, then by bytes.

Two details in step 4 are load-bearing. The baseline is admitted _whatever the budget says_: an
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

Exposure is a _display_ concern. A tool that is not exposed can still be called by name, and
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

The check is the kernel's, and it runs before the spend: `Session::exhausted` used to have exactly
one caller — the reference loop — so a driver that forgot to ask could spend past every limit, and
the event recording the stop was written by that driver. A budget enforced by one cooperative caller
is not a budget.

So `inference` and `invoke_tools` both begin by asking. A session that is out of room is closed with
the reason, `budget_exhausted` is written once, and the call returns `Budget` without spending
anything. `Session::stop_reason` carries the decision, so a driver translates it instead of
re-deriving it.

There is no generic "record an event" syscall either. It existed, and it was how the loop outsourced
this enforcement; with the enforcement moved, its only remaining use was forging events the kernel
would have to accept as fact. A caller writes _state_; the kernel writes the record.

Budget is enforced as flow control, not as a post-hoc check: the caller grants credit,
each `usage` event spends it, exhaustion drains the session and writes a checkpoint.

| Layer         | Enforced by                                    |
| ------------- | ---------------------------------------------- |
| tokens / cost | credit from the caller, debited per model call |
| steps         | the driver, per turn                           |
| wall clock    | the kernel, via `Clock`                        |
| policy        | `Verdict::Deny` before the tool runs           |

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

## 10.1 Reloading a session that died mid-turn

A checkpoint is a cache of the log, not a replacement for it. `load` used to read only the snapshot,
so every event written after the last checkpoint was invisible: the tokens a half-finished turn had
already been billed for, the message it completed just before the crash, a configuration change made
after the last boundary. Reloading therefore **lost paid work**, silently.

`load` now folds the tail — `events_since(snapshot.seq + 1)` — and every event kind is classified
explicitly:

| Class | Meaning | Examples |
| --- | --- | --- |
| **State** | it changed something the session carries | `usage`, `message_completed`, `tool_finished`, `state_changed`, `session_configured`, `generation_migrated`, `tools_exposed`, `budget_exhausted` |
| **Narrative** | it describes what a consumer saw | deltas, `capability_degraded`, `progress`, `exec_*` |
| **Abandoned** | evidence of a turn that never finished | `tool_call_started`, `tool_call_args` |

The catch-all is **absent on purpose**: a new event kind that nothing classified is a compile error,
not a silent no-op.

Three details carry the design:

- **The ledger is keyed by attempt.** A stream reports usage several times for one attempt; folding
  each report into its own entry would turn one bill into eight.
- **An abandoned turn is recorded, not resumed.** No provider can resume a stream, so the session does
  not pretend the message completed — the model does not get a fragment it never finished writing —
  and the log gains one `attempt_failed` naming the reason. The spend is kept, and the partial text
  stays in the stream.
- **The fragment is reported, not left to be reassembled.** `GET /v1/sessions/{id}` carries
  `partial` — the text the assistant had produced, the step it reached, and any tool call whose
  arguments were still arriving. Reading it out of the event stream by hand is work every caller would
  repeat, and doing it slightly differently each time is how two clients end up disagreeing about what
  the model said. The record of the abandonment does **not** terminate the fragment: the annotation
  must not erase what it annotates.

The fragment is not resumable and the API says so by shape rather than by prose: there is no "resume"
field, because there is nothing to resume. What a caller can do with it is continue the
*conversation* — send the text back as context for a new turn, which is a new generation and billed as
one.
- **Recovery is idempotent.** A completed unit of work — a message, a tool result, a failed attempt, a
  state change — clears any evidence before it. That makes a second `load` a no-op, and makes a real
  crash *after* a successful turn still visible.

## 11. Context management

A session outlives its model's context window. Three mechanisms, and the split between them is the
design:

| Mechanism              | Owner      | Why there                                                                                                      |
| ---------------------- | ---------- | -------------------------------------------------------------------------------------------------------------- |
| **measure and refuse** | kernel     | it is the only place holding the window, the reserve and the prompt at once, and a silent truncation is banned |
| **compaction**         | the caller | it is editing the caller's message list, and there are many strategies                                         |
| **page-in**            | a tool     | an index, an embedding model and a ranking function belong in a tool                                           |

The reasoning behind the middle row is worth stating, because the obvious design is wrong. The
kernel does not assemble prompts: `inference(session, messages)` takes the conversation from the
caller. Compaction _is_ editing that list, so a kernel that compacted would be rewriting data it does
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
the prompt did not fit _that_ model, which is a fact about one target, not about the call.

### 11.2 Compaction is the caller's, record included

The kernel offers no summariser, no window policy, no "recent N turns", and **no way to report one
either**. What it offers is the measurement above and a refusal that says what is wrong.

The tempting addition is a `compact(from, to, summary)` syscall that appends a `history_compacted`
event, so an auditor could see what the model was told. It was written, and then removed, because it
fails the test every other field in this design has to pass: **the kernel cannot verify it.** The
span addresses the caller's message list, which the kernel has never seen; the summary is the
caller's word for what it sent; and nothing stops a caller from reporting a compaction it did not
perform, or from compacting and not reporting. That is a claim, not a fact — and it would sit in the
one place that claims to be the record.

Two things follow, and neither is a gap:

- **Replay does not need it.** A replay reconstructs the session from `messages`, which the kernel
  owns and the caller's compaction never touches. So the log is complete without it.
- **Audit is the caller's too.** Whatever assembles a prompt is the only thing that can say truthfully
  what went into it. A caller that needs that record keeps it where it made the decision.

There is also no generic "emit my own event" syscall, and that is deliberate: a caller able to emit
arbitrary events could forge `budget_exhausted` and poison every replay that followed. `compact` would
have been a back door around that prohibition, addressed to the one event kind it wanted.

The divergence this leaves is the honest one: after a compaction, `load` returns the full history and
therefore **more** than the last prompt carried. The kernel's copy is the record; the prompt is a view.

What the kernel does promise is the part a caller cannot do for itself — measure, and refuse instead of
truncate. A session whose caller never compacts ends at `StopReason::Tokens`, and the kernel does not
pretend otherwise.

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

## 11.5 Authentication

Three claims, and the daemon only makes the ones it can keep:

| Claim                             | How                                                              |
| --------------------------------- | ---------------------------------------------------------------- |
| only the local user can connect   | the socket lives in a `0700` directory and is `0600`             |
| a request carries a credential    | `NUCLEOS_TOKEN`, checked on **every** route including `describe` |
| every session belongs to somebody | one principal from `NUCLEOS_PRINCIPAL`, for now                  |

Peer credentials (`LOCAL_PEERCRED`) would be the better first row, and they need `unsafe`, which this
workspace forbids. So the check is pushed to the one place that can enforce it without it — the
filesystem — rather than left to a comment claiming a check that does not exist.

A token exempted from any route is not a token: `describe` leaks the model and tool names, so it is
behind the same check as everything else.

**Several callers: `NUCLEOS_TOKENS`.** A JSON document maps each token to a tenant and a subject;
the middleware resolves the caller and puts the principal on the request, so a handler reads *its*
caller and not the daemon's. With one token, `NUCLEOS_TOKEN` and `NUCLEOS_PRINCIPAL` still work.

**Tenant isolation is in the schema.** The store's key is `(tenant, session)`, not the id alone:
session identifiers are chosen by callers, and two tenants each choosing `s1` is ordinary rather than
adversarial. So the tables are keyed by the pair, `Store`'s methods take a `SessionKey`, and a lookup
*cannot* reach another tenant's rows — there is no query to forget to scope.

The snapshot records its owner as well (`Principal`), and within one tenant an id still belongs to the
subject that created it: `load` refuses a mismatch with `NotFound`, because a refusal that says "this
exists but is not yours" is a directory of the identifiers other people use.

## 12. Telemetry

Two things, and the absence of the rest is deliberate.

### 12.1 Logs: `tracing`, and who decides what

Libraries emit through **`tracing`**; only the binary installs a subscriber. That split is the whole
decision:

|                                             | Decides                                           |
| ------------------------------------------- | ------------------------------------------------- |
| a library (kernel, adapters, store, policy) | _what_ happened, as structured fields             |
| `nucleosd`                                  | format, sink, level (`NUCLEOS_LOG`, JSON, stdout) |

A library that picked its own output is a library a host cannot embed, and a kernel that wrote files
is a kernel with an opinion about deployments. `tracing` over `log` for one reason that matters here:
fields. `session = %id, tool = %name, attempt = 3` is what makes a record actionable, and rebuilding
that from a formatted string is the work this avoids. Spans are available but not required: a
library's `tracing` calls need no instrumentation pass to be useful.

What libraries log is deliberately narrow, because the event log already holds the domain record:

- **refusals and stops** the caller cannot see afterwards: budget, prompt-too-large, policy verdicts;
- **fallbacks, retries and reconnects**: attempt failed, source restarted, tool set changed;
- **degradations**: a parameter the provider dropped;
- **deployment state**: generation published, session migrated, policy loaded.

What they do not log: message content, tool arguments, model output. A log line carrying a user's
prompt is a second copy of the data with weaker retention rules, and the event log already holds it
under the session's lifecycle.

Two rules for the future, learned the expensive way here. A log line must be **assertable** —
`crates/nucleos/tests/logging.rs` installs a real subscriber and checks the record's fields, because
instrumentation nothing observes is dead code with a formatting cost. And a comment claiming a
library misbehaves must be checked against a correct experiment: this file briefly claimed axum did
not poll a graceful-shutdown future, which was a broken test harness killing the wrong process.

**Counters, at `GET /v1/metrics`.** Which route was called how many times, how many requests were
refused, how many sessions opened, how many stopped on budget, how many reloads, and how long the
daemon has been counting. Keyed by route _template_: a thousand sessions would otherwise be a
thousand keys, and a counter with unbounded cardinality is a memory leak wearing a metric's name.

**Progress, from the kernel's side.** `ToolCtx::progress` is a sink: a tool reports steps while it
runs, the kernel writes one `progress` event per report with the call id as the token.

Nothing reported used to mean nothing recorded, which made a slow silent tool indistinguishable from a
hang. So the kernel adds the half it knows: a call that has not reported after `heartbeat`
(default 15s, host-configurable, zero disables) gets `progress { done: elapsed_seconds, total: 0,
message: "still running" }`. `total: 0` is this ABI's "unknown", which is exactly right — nobody told
the kernel how much work is left. A tool that reports is never interrupted by one: once a call has
spoken for itself the kernel stops speaking for it, because interleaving "still running" with real
numbers is noise, and a tool that reports once and then hangs is a bug the deadline bounds. The events are written by a task sharing the session's sequence
counter, so a batch of eight concurrent calls still leaves one ordered log.

MCP servers' own `notifications/progress` are **not** forwarded: the client library assigns the
progress token per request and does not expose it to the caller. See `docs/pitfalls.md` §56 — the gap
is documented at the line that would need it rather than implied by silence.

**One record per request.** Method, route, status, duration, through the same subscriber as
everything else — so a deployment has one stream, one format and one filter, and a collector builds
its own histogram from the durations.

What is absent, and why each absence is a decision rather than an omission:

- **No histograms in the daemon.** A p99 needs a sink that can aggregate, and a daemon exporting to
  nowhere is a promise about dashboards nobody has. The duration is in the line; aggregation belongs
  where the data lands.
- **No trace context.** The event log is already a better trace: it has sequence numbers, it is
  durable, and it replays. A second trace would be a second ordering of the same facts, and the two
  would disagree eventually.
- **No exporter.** Files, sockets and scrapers are deployment decisions, and the deployment file does
  not exist yet. Adding one now means choosing for every deployment at once.

The counters are the questions an operator asks first. Everything else can be computed from the event
log, which is the record rather than a statistic.

## 12.9 Configuration, today and proposed

Today every deployment decision is an environment variable: fifteen of them, read in `nucleosd`,
validated wherever they happen to be needed. Nothing can change without a restart except the tool
source, and that includes the policy — the one thing an operator most needs to tighten during an
incident.

[`config.md`](config.md) is the proposal that would replace it: `nucleos init` once, configuration in
the store, a signed request per change, and two databases with two lifecycles.

Its first step is built: the daemon now opens **two** databases under one root — `system.db` for
configuration and `sessions.db` for sessions — and takes `--root` and `--socket`, with platform
defaults that are absolute and a socket in a private directory rather than a predictable name in
`/tmp`. The rest is still a proposal, and the document says which half is which.

What it does **not** propose to change, because the architecture already decided it: session wiring
(kind, sandbox, workspace, budget) stays startup-only and lives in the checkpoint; everything a request
carries stays in the request; and a change to exposure or sandbox narrows and never widens.

## 13. Non-goals

| Not building                                                          | Why                                                                                          |
| --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| A scheduler or memory allocator                                       | `tokio` and the OS already provide them; we schedule turns                                   |
| A wire protocol to our own tools                                      | MCP exists; we are a client                                                                  |
| Identity, signing, certificates                                       | The daemon's job; the kernel takes `Principal` as data                                       |
| Cluster placement, supervision, self-healing                          | A platform concern; the kernel does not know how many nodes exist                            |
| A provider-specific parameter bag                                     | One extension hatch is enough; the five parameters every provider has are carried explicitly |
| Multiple ABI bearers (in-process / child / remote as one abstraction) | The most expensive kind of debt: every interface is forced to the most conservative shape    |
| Live tool swapping inside a turn                                      | Replaced only at turn boundaries                                                             |
| Auto-triggered reload (file watch)                                    | Deferred; reload is explicitly requested                                                     |

## 14. Stages

| Stage | Content                                                                  | Passes when                                              |
| ----- | ------------------------------------------------------------------------ | -------------------------------------------------------- |
| P1    | contract + kernel + in-memory store + one model adapter + one MCP source | a multi-turn session with a tool call runs end to end    |
| P2    | SQLite store, checkpoint/restore, credit enforcement                     | `kill -9` then resume continues the session              |
| P3    | context management: budget, compaction, retrieval references             | a session stays coherent past the model's context window |
