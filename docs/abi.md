# The ABI

The ABI is a permanent commitment. Once a caller depends on a field, that field exists
forever. Every decision below is made to keep the surface small rather than to keep it
convenient.

## 1. Shape

JSON over HTTP, with server-sent events for the two streams. The contract is
`schema/v1/schema.json`, generated from the kernel's own types — see [`wire.md`](wire.md) for the
encoding rules and why the schema is not a separate type tree.

| Route                                    | Body               | Returns                                                 |
| ---------------------------------------- | ------------------ | ------------------------------------------------------- |
| `GET /v1/describe`                       | —                  | `DescribeResponse` — version, models, ABI version       |
| `POST /v1/agents`                        | `StartRequest`     | `StartResponse` — runs the reference loop to completion |
| `POST /v1/sessions`                      | `StartRequest`     | `StartResponse` — opens a session, drives nothing       |
| `GET /v1/sessions/{id}`                  | —                  | `SessionView`                                           |
| `POST /v1/sessions/{id}/inference`       | `InferenceRequest` | `InferenceResponse`                                     |
| `POST /v1/sessions/{id}/tools`           | `ToolBatchRequest` | `ToolBatchResponse`                                     |
| `POST /v1/sessions/{id}/state`           | `StateRequest`     | `204`                                                   |
| `POST /v1/sessions/{id}/close`           | —                  | `StartResponse`                                         |
| `POST /v1/sessions/{id}/verdict`         | `VerdictRequest`   | `StartResponse` — runs the loop on                      |
| `GET /v1/sessions/{id}/events?from_seq=` | —                  | `text/event-stream` of `Event`                          |

The machine-readable form of this table is [`openapi/v1/openapi.json`](../openapi/v1/openapi.json):
`components.schemas` is the JSON Schema this repository already generates, and `paths` is the same
ten routes with their status codes. It is committed and a test fails when it drifts:

```bash
NUCLEOS_OPENAPI=1 cargo test -p nucleos --test openapi   # regenerate
cargo test -p nucleos --test openapi                     # fail if stale
```

Two layers, deliberately:

- **The driver** (`/v1/agents`, `/v1/sessions/{id}/verdict`) runs the reference loop from
  `nucleos-agent`. One call, an agent happens.
- **The syscalls** (`open`, `inference`, `tools`, `state`, `close`) are one turn each, in an
  order the caller chooses. An application that wants a different loop — plan-execute,
  verification turns, map-reduce over items, its own sub-agents — calls these and owns the loop.

`inference` takes its messages **in the request** and appends nothing: what you send is what you
get, and whether the answer enters history is decided by the next `state` call. That is what makes
a verification turn possible without polluting the transcript.

```text
Describe   capabilities and the current generation
Start      create a session, pin it to a generation
Run        stream events, resumable from any seq
Send       user input, steering, and human-in-the-loop verdicts
Credit     extend the session's budget
Cancel     request an orderly stop
Close      terminate and release resources
List       sessions on this node

Register   add or update a tool source, producing a new generation
Deregister remove a tool source
Sources    list sources, generations, and health
Reload     apply pending source changes as one generation swap

```

Session control and source management are separate groups on purpose: one is per-session
and hot, the other is node-wide and rare. Merging them would make every session call carry
node-wide authority.

### 1.1 The published artefact

One document: [`openapi/v1/openapi.json`](../openapi/v1/openapi.json). Its `components.schemas`
is every message the ABI carries, generated from the kernel's own types; its `paths` is the ten
routes above. An SDK generates from it and never copies it — a copy is a version that drifts, and
the drift is discovered by a client failing in production.

```bash
NUCLEOS_OPENAPI=1 cargo test -p nucleos --test openapi   # regenerate
cargo test -p nucleos --test openapi                     # fail if stale
```

The default run only reads, so CI fails instead of quietly rewriting the file.

Two things it cannot express, both stated rather than hidden:

- **The event stream.** Each `data:` line is an `Event` — the response schema says so — but a
  generator will not produce a working SSE client. Every other route is generated; this one is
  hand-written, and the document says how the framing works.
- **The unix socket.** A socket has no URL, so the servers entry is `/` with the transport
  described in prose. A `http://localhost` base URL would be a guess that is wrong by default.

## 2. Sessions

```text
StartRequest {
  spec:      route (model chain + retries), generation params, system prompt,
             initial input, tool allow-list
  budget:    tokens / cost / steps / wall clock
  metadata:  caller-defined labels, opaque to the kernel
  resume:    optional session id — restore and continue instead of creating
}
```

`Start` returns `{session, generation, state}`. The `generation` is the tool-source
generation the session is pinned to; it is not negotiable later, only migratable.

`Start` also carries the caller's **capabilities** — `render_images`, `max_part_bytes`,
`supports_progress`. The daemon downgrades rather than fails: a client that cannot render an
image receives a `FilePart` reference instead of inline bytes. Both sides declare and
neither assumes, because a daemon that guesses the client's abilities is wrong exactly for
the client that needed the guess.

Sessions are addressed by a caller-chosen id when `resume` is set. That makes client
retries idempotent: a `Start` that was retried after a timeout resumes the existing
session instead of creating a second one.

Session identity is the pair **(tenant, session)**. The tenant comes from the authenticated
principal and never from a request field; two tenants may each own a session called `abc`
and neither can address the other's.

## 3. `Run` is resumable, not a subscription

```text
RunRequest { session, from_seq }
→ stream Event
```

The server replays history from `from_seq` and then continues live. A client that
disconnects and reconnects loses nothing.

This is the single most valuable property of the ABI: a long-running session is not owned
by a request. Network distance, proxy timeouts and client restarts cannot lose work,
because the work lives in the event log and the stream is only a view of it.

## 4. Events

```text
Event {
  seq:     uint64   // monotonic within a session, starts at 0
  session: string
  at:      Timestamp
  kind:    oneof {
    state               StateChanged        // created/running/awaiting_*/…
    text_delta          TextDelta           // part_index + text
    reasoning_delta     ReasoningDelta      // part_index + text and/or signature
    tool_call           ToolCallStarted     // index + id + name + deadline
    tool_call_args      ToolCallArgs        // index + argument fragment
    degraded            CapabilityDegraded  // a field the provider could not honour
    attempt_failed      AttemptFailed       // one attempt, with what it cost
    tool_result         ToolFinished
    usage               UsageReport         // tokens + cost + purpose + model + attempt
    awaiting_human      AwaitingHuman       // typed PermissionOptions
    budget_exhausted    BudgetExhausted
    failed              Failure
    generation_migrated GenerationMigrated
    message_completed   MessageCompleted    // the assembled record of a model call
    progress            Progress            // token + done/total
  }
}
```

Rules:

- `seq` is dense and gap-free. A client may treat `seq` as the resume cursor without
  negotiating.
- `usage` is emitted per model call, **including calls that failed after streaming**, and
  the cost is debited whether or not the answer was usable.
- `generation_migrated` is recorded so replay reproduces which tool set a session was on
  at each turn.
- Events carry the **content model**, not copies of its fields: `ToolCallStarted` holds the
  same `ToolCall` a `ToolCallPart` does, and `ToolFinished` holds the same `ToolResult` that
  enters history. A stream and a stored turn cannot describe the same call differently.
- `type` is a routable string (`nucleos.v1.tool_finished`) carried beside the `kind`
  `oneof`. It is redundant on purpose: a proxy filters on it without compiling the schema.
  `kind` stays authoritative.
- `meta` is an opaque map the kernel never reads. It is the **only** extension hatch.
- Deltas carry a **`part_index`**. Without it, a turn that emits text, then a tool call, then
  more text cannot be rendered — a second block is indistinguishable from a continuation.
- `message_completed` carries the **assembled** message. Deltas are a rendering protocol;
  this is the record, and some content (a reasoning signature) exists only after assembly.
- `tool_call` is followed by zero or more `tool_call_args`, addressed by `index`. Arguments
  stream as text because that is what providers send and because a partial object cannot be
  handed to a tool; the kernel assembles and validates before anything runs. Malformed
  arguments become a tool result with `is_error`, which the model can read and repair — not a
  failed turn.
- `degraded` is emitted when a provider cannot honour a request field. A `tool_choice =
REQUIRED` that quietly became `AUTO` is a session behaving differently than it was
  configured to, and the difference is undiscoverable afterwards unless it is recorded.
- `attempt_failed` appears inside the stream for the same reason: a retry does not erase the
  attempt that failed, or the money it spent.
- A tool result carries content **and** an `is_error` flag. A failure is not a separate
  branch: the error text is content the model reads, and refusing it a place in the content
  model would leave the model with a message it cannot see.
- `tools_exposed` records that the model can see tools it could not see before. Same reason
  as the `configure` event: a replay must be able to say which tools were in the prompt at
  turn 42, and "whatever the session had at the time" is not answerable from the record
  otherwise.
- Events carry no back-references; a consumer can render from the stream alone. A client
  that needs the prompt must reconstruct it from the events, not ask the server for
  hidden state.

## 4.0 Listing sessions

`GET /v1/sessions?kind=agent|process&limit=&page_token=` returns a page of **summaries** — id, kind,
state, stop reason, cursor — for the sessions the caller owns. Not full views: a page of a hundred
sessions must not load a hundred conversations, and nothing a caller does with a listing needs the
messages.

`kind` is a filter, and an unknown value is a `400` rather than an empty page: answering "nothing" to a
misspelled filter is how a caller concludes the feature is broken. Another subject's sessions are
absent rather than refused — a listing that names what the caller may not open is a directory of other
people's work.

### 4.0.0 Streaming a turn

`POST /v1/sessions/{id}/inference` with `Accept: text/event-stream`.

The response is the session's **events**, one per frame, and nothing else — no second vocabulary, no
summary type, no synthesized terminal frame:

```text
id: 42
event: nucleos.v1.turn_started
data: {"seq":42,"session":"s1","at_unix_millis":0,"schema_version":1,
       "kind":"turn_started","model":"primary","generation":3}

id: 43
event: nucleos.v1.text_delta
data: {"seq":43,…,"kind":"text_delta","part_index":0,"text":"Hel"}
```

The `kind` is **flattened into the event**, the same way a part's kind is flattened into a part, so the
tag appears once and the SSE event name and the JSON tag are the same fact at the same level.

| Rule | Why |
|---|---|
| `id` is the log's sequence, the same cursor `GET /events` uses | a client that reconnects resumes with `Last-Event-ID` on either endpoint, and never has to re-run a turn it already paid for |
| the SSE `event:` name is derived from the JSON `kind` tag | a client that reads only `data` gets the same type; the two cannot disagree because both come from one enum |
| the last frame is `message_completed`, carrying the assembled message | a client that ignores every earlier frame ends up with exactly what the non-streaming call returns |
| deltas carry `part_index` / `index` | without them a stream cannot represent text → tool call → more text, or which call an argument fragment belongs to |
| the turn is announced by `turn_started` before the first model call | a client can tell "no answer yet" from "never started", and a stream that stops silently is distinguishable from one that stalled |
| a turn that produces no message ends with `turn_failed` | a policy that refuses the model, or a prompt that cannot fit, would otherwise leave the stream ending after the announcement with no reason at all |

**Where a stream begins is decided by the log.** A turn that fails before `turn_started` (no credit,
unknown model, policy refusal) answers with an **HTTP status code and a JSON error**, not a `200` and an
error frame — even though the kernel may already have recorded *why* (`budget_exhausted`), because that
record belongs to the session, not to a stream this request never opened. After `turn_started`, failures
arrive in the stream: `attempt_failed` for a failed attempt, `turn_failed` for a failure that never got as
far as an attempt, and `message_completed` for success. Exactly one of those three ends a turn, so the
stream always says how it ended.

**Hanging up cancels the turn.** The turn runs in its own task so the record is still written: the log
gets `attempt_failed` with code `cancelled`, the session stays **open**, and the spend already reported
stays debited. A client that disconnects has not ended a conversation, only stopped watching one.

**Retries are visible, and provisional.** A route that falls back stays inside the same turn: the log
gets `attempt_failed`, then the next attempt's deltas. So a client that renders as it reads must treat a
failed attempt's output as provisional and reset it on `attempt_failed`. A client that only reads the
last frame never has to care — the terminal frame is the whole answer, which is exactly why it is the
kernel's own `message_completed` rather than something the transport made up.

### 4.0.1 Retiring generations

`POST /v1/tools/retire` → `{"current": 3, "retired": [1, 2]}`

Forgets every tool set but the current one. Safe at any time, and safe to ignore: a session pinned to a
retired set **holds** it, so dropping the number frees what nobody holds and nothing else.

- `retired: []` is the honest answer when there was nothing to forget — a kernel with one generation has
  nothing to retire and has done nothing wrong.
- The current generation is never retired: it is the target of every migration.
- A session on a retired generation keeps answering calls from its own set, and its compatibility check
  still compares against that set rather than against a number that no longer resolves.

### 4.0.2 Cancelling a turn

`POST /v1/sessions/{id}/cancel` → `{"cancelled": true|false}`

Stops the turn the session is running *now*. A budget bounds a turn and a deadline bounds a session;
neither ends one on request. Cancelling stops a call and leaves the session **open** — closing it would
throw away the conversation to stop one call.

- `cancelled: false` is not an error. A caller racing the end of a turn is a normal race; `404` would say
  the session is gone and `500` would say something broke.
- What the attempt had already produced stays in the event log. The attempt is recorded as
  `attempt_failed` with code `cancelled`, which is how the next reader knows nothing went wrong —
  somebody decided. The spend already reported stays debited.
- It does not close, does not retry, and does not remove the partial answer from the log.
- A session that is not running a turn, or not yours, answers the same way: `404`.

## 4.1 The ceiling and the exposure

A session has two tool sets. `config.tools` is the **ceiling**: what the session may ever use,
settable only narrower. `config.tool_policy` is the **exposure policy**: how visible each tool is,
always within the ceiling.

`Exposure` has four levels, and all tools are `deferred` unless the policy says otherwise:

| level      | in the prompt                                       |
| ---------- | --------------------------------------------------- |
| `always`   | every turn — the configured baseline                |
| `direct`   | once found or used, while it stays in the pool      |
| `deferred` | only after a search surfaces it; the default        |
| `hidden`   | never, not even via search — still callable by name |

Two budgets, and they are deliberately different: `policy.budget` (default 32 definitions or
16 KiB) caps what one prompt carries; `policy.discovery` (default 32 tools, 16 KiB, 10 idle turns)
caps what a session remembers having found. A session that can show eight tools may have
discovered thirty, and re-searching for one it used two turns ago is the waste the second cap
prevents.

`tool_search` is registered by the kernel itself whenever there is a catalog, and seeded to
`always`: a catalog whose only door is hidden is a catalog the model can never enter, so configuring
it away is refused rather than silently obeyed. It is _shown_ only when the policy leaves something
reachable by no other route — a policy of `always` everywhere hides it, because there is then
nothing to find. It ranks the catalog and returns names and one-line summaries — never full
schemas, which would spend the tokens the search exists to save.

Exposure is a display concern, not a permission: a tool that is not exposed is still callable by
name, and execution is gated identically either way. `configure` refuses an `always` entry outside
the ceiling; a tool whose result asks for a name outside it has that name dropped, because the tool
has no session and the kernel is the only place that decides.

## 4.2 Reload and generations

`POST /v1/tools/reload` reconnects the deployment's tool source and publishes what it advertises now.
It answers with the new `generation`, the tool names it holds, and `added`/`removed` — which describe
what changed for sessions that start from now on, not what happened to sessions already running.

A session resolves tools through the generation it was pinned to when it opened, so a reload cannot
change the meaning of a prompt already sent. A compatible set (same names, nothing newly required) is
adopted at the next turn boundary and recorded as `generation_migrated`; an incompatible one leaves
the session pinned until it ends. `GET /v1/describe` reports the generation in force.

## 4.3 Metrics and reload

`GET /v1/metrics` returns counters keyed by route template. The request that produced a snapshot is
counted after the body is built, so it cannot appear in its own response.

`POST /v1/tools/reload` reconnects the configured source and publishes a new generation; see 4.2.

## 5. Credit

`Credit` extends budget; `Run` carries an initial credit grant. Each `usage` event debits
it. On exhaustion the session transitions to `Cancelling`, writes a checkpoint, emits
`budget_exhausted`, and ends.

Backpressure and cost control are the same mechanism, which is why there is no separate
rate-limit surface: a caller that cannot keep up stops granting credit.

## 6. Generations and reload

Tool sources are declared, never inlined. `Register` takes a source descriptor:

```text
ToolSource {
  name:      string
  transport: oneof { stdio {command, args, env} | http {url, auth} }
  allow:     repeated string     // tool name filters
  isolation: none | process | vm        # derived by the daemon, not declared by the source
}
```

The daemon connects, calls MCP `tools/list`, and snapshots the advertised specs into a new
generation. `Reload` commits the pending set as one atomic swap. Two properties matter:

- **Reload is explicit.** Nothing watches the filesystem. A caller decides when the
  generation changes, so a half-written config never becomes a generation.
- **Reload reports partial success.** Individual sources may fail to connect or advertise
  nothing; the response names them rather than failing the whole swap.

```text
ReloadResponse {
  generation:      uint64
  failed:          repeated SourceFailure   // source, reason, code
  pinned_sessions: uint32                   // sessions that cannot migrate
}
```

Migration happens at turn boundaries only, and only when the old and new tool sets are
compatible — same tool names, structurally equal input schemas (new optional properties
allowed), identical isolation descriptor. Anything else pins the session to its
generation until it ends.

## 7. Errors

```text
enum Code {
  UNSPECIFIED = 0; INVALID = 1; NOT_FOUND = 2; DENIED = 3;
  BUDGET = 4; TIMEOUT = 5; UNAVAILABLE = 6; INTERNAL = 7;
}
```

Every response that can fail returns `code`, a human-readable `reason`, and a `retryable`
flag — never a bare string, and never an error that has to be parsed. A string error code
is the most expensive kind of debt: the first caller who compares strings freezes it.

`retryable` is a statement about the same request, not about the world. `UNAVAILABLE` is
retryable; `BUDGET` is retryable only after `Credit`.

Two details make an error actionable rather than merely typed:

- `retry_after_millis`, because "retryable" does not say _when_. Zero means no hint, not
  "immediately".
- `details`, a machine-readable map — `{"needed_tokens": "40000"}` on `BUDGET` is the
  difference between a caller that can fix it and one that can only report it.

`List` pages by **keyset token**, not by offset: sessions are created and retired constantly,
so an offset skips or repeats rows. `next_page_token` empty means the last page; a client
stops on empty, never on a short page.

## 8. Versioning

- Package `nucleos.v1`. A breaking change is `nucleos.v2`, served side by side.
- Fields are added, never removed, never renamed, never re-typed. Changing meaning means a
  new field.
- Enum values are added, never renumbered. Unknown values must be tolerated by clients —
  which is why `DESCRIBE` reports the daemon's supported version range.
- The schema is committed and a test fails when the code and the file disagree. Renaming a
  serialized field is a breaking change even when the Rust field keeps its name: `#[serde(rename)]`
  is how a rename stays invisible on the wire.

## 9. Mapping to MCP

| nucleos              | MCP                                                                             |
| -------------------- | ------------------------------------------------------------------------------- |
| tool source          | server (stdio or streamable HTTP)                                               |
| `Tools` (allow-list) | `tools/list` snapshot                                                           |
| `ToolCallStarted`    | `tools/call`                                                                    |
| `ToolFinished`       | `tools/call` result: `content` blocks → `Content.parts`, `isError` → `is_error` |
| cancel a tool call   | `notifications/cancelled`                                                       |
| isolation `process`  | the server runs as a child process, so the daemon derives `process`             |

Deliberately unsupported in v1: MCP **sampling** and **elicitation**. The daemon does not
advertise those client capabilities, so a server that needs them must fall back. Adding
them later is additive — they become events and a policy verdict respectively, not new
methods.

## 10. Edge cases, defined rather than discovered

Every one of these has a defensible alternative. The point is to pick one now, because the
alternative gets chosen accidentally by the first implementation.

| Case                                              | Behaviour                                                                                                                                     |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `Run` on a terminal session                       | Replays from `from_seq` to the last event, then ends the stream. A client never has to know the session already finished.                     |
| Two concurrent `Run` streams on one session       | Allowed. Each cursor is independent; a stream is a _view_, and no stream can advance or alter state.                                          |
| `Run` with `from_seq` beyond the last event       | `INVALID`, not clamped. Silently skipping events hides a client bug until it costs somebody an incident.                                      |
| `Start` with `resume` on an unknown id            | `NOT_FOUND`. It does not create a session with that id — otherwise a typo becomes a new session and the real one is orphaned.                 |
| `Register` with a name that already exists        | Replaces it in the pending generation. Idempotent by name, so a retried `Register` cannot create a duplicate source.                          |
| `Reload` with no pending changes                  | No-op. Returns the current generation with no failure and no new generation number.                                                           |
| `Send` while `AWAITING_TOOL`                      | Accepted and ordered; applied at the next turn boundary. Rejecting it makes callers retry into a race.                                        |
| `Close` on a running session                      | Cancels, waits for the turn to resolve, checkpoints, then closes. A `Close` never discards a turn in flight.                                  |
| `Credit` on a terminal session                    | `INVALID`. A finished session cannot be revived by paying more.                                                                               |
| `Cancel` twice                                    | Idempotent; the second call reports the state the first one produced.                                                                         |
| An event kind the client does not know            | Ignore it and keep the cursor. `type` is additive; a client that fails on an unknown kind breaks on every future feature.                     |
| A `PermissionOption` id the kernel did not offer  | `INVALID`. The verdict names an option that was presented, or it is a client bug.                                                             |
| Nobody answers an `AwaitingHuman`                 | Apply `default_if_unanswered`, which is a refusal. Silence is never approval.                                                                 |
| `List` with a `page_token` from an older page set | The token stays valid; it is a key, not an index. `INVALID` only if it cannot be decoded at all.                                              |
| `Run` before `Start`                              | `NOT_FOUND`. A stream is a view of a session, and there is nothing to view yet.                                                               |
| `Describe` on an empty daemon                     | Succeeds with `generation = 0` and no models. A health check must not need a session.                                                         |
| `Deregister` a source a pinned session still uses | Allowed. The source stays alive until the last session on its generation ends; new sessions simply never see it.                              |
| `Register` while a `Reload` is in flight          | Serialized. Reloads run one at a time and a registration lands in the pending set, so a caller never has to guess which generation it joined. |
| `Run` open across a `Reload`                      | Allowed. The migration happens at the next turn boundary and is announced as `generation_migrated` in the same stream.                        |

One case is deliberately left undefined: a `Send` that arrives after `Cancelling` began.
The session drains to the boundary and ends; whether the input was consumed is observable
in the event log, and the caller must read it rather than assume.

### 10.1 Ordering

A stream that can be wrong about order is a stream nobody can build a UI on, so:

- `seq` is assigned by the session's single writer, which makes it a total order within a
  session.
- Every delta of a model call precedes that call's `usage`. `usage` precedes any
  `tool_call` produced from the same turn.
- `tool_call` always precedes its `tool_result`. A result never arrives for a call the
  client has not seen.
- The terminal `state` event is the last event of a session, and `Run` ends immediately
  after it. No event ever follows a terminal state.
- Two concurrent `Run` streams never disagree about the order of events they both see.

## 11. Not served, and why

| Not an endpoint                  | Why                                                                                                                                    |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Stateless model invocation       | This is an agent OS, not a gateway. A host that wants a raw model call links the library; see [`architecture.md`](architecture.md) §7. |
| Embeddings                       | A tool: an embedding model belongs with the index and the ranking that use it.                                                         |
| Speech to text, text to speech   | A tool. Voice output also needs an artifact store the kernel does not have.                                                            |
| Tool-source registration, reload | Tool sources are declared in the daemon's configuration; discovery lands with the MCP client.                                          |
| Credit, cancel, list             | They need an operator surface; `close` exists because a syscall can do it statelessly.                                                 |

An endpoint that exists and returns 501 is a contract nobody can rely on, so these are absent
rather than declared.

## 12. Deferred

| Deferred                                   | Why it is safe to defer                                                                     |
| ------------------------------------------ | ------------------------------------------------------------------------------------------- |
| Auto-triggered reload (file watch, SIGHUP) | It is a trigger, not a mechanism: it will call `Reload`                                     |
| Multi-node placement                       | Sessions are recoverable from the store, so placement is a routing decision above the ABI   |
| Streaming tool arguments                   | `tools/call` is request/response; partial arguments are a model concern, not a wire concern |
| Per-session auth scopes                    | `Principal` is passed through already; scoping it is policy                                 |
