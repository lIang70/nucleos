# The ABI

The ABI is a permanent commitment. Once a caller depends on a field, that field exists
forever. Every decision below is made to keep the surface small rather than to keep it
convenient.

## 1. Shape

JSON over HTTP, with server-sent events for the two streams. The contract is
[`openapi/v1/openapi.json`](../openapi/v1/openapi.json), generated from the kernel's own types —
see [`wire.md`](wire.md) for the encoding rules and why the schema is not a separate type tree.

The routes fall into the same two groups the design always had: **session control**, which is
per-session and hot, and **node administration**, which is node-wide and rare. Merging them would
make every session call carry node-wide authority.

| Group              | Routes                                                                                                                                                                                                                                                                                                           | Answers                                                                                                                 |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Describe           | `GET /v1/describe`                                                                                                                                                                                                                                                                                               | `DescribeResponse` — version, models, ABI version                                                                       |
| Agent loop         | `POST /v1/agents`                                                                                                                                                                                                                                                                                                | `StartResponse` — runs the reference loop to completion                                                                 |
| Sessions           | `POST /v1/sessions`, `GET /v1/sessions`, `GET /v1/sessions/{id}`                                                                                                                                                                                                                                                 | `StartResponse` — opens a session and drives nothing; `SessionListResponse`; `SessionView`                              |
| Turn               | `POST /v1/sessions/{id}/inference`, `POST /v1/sessions/{id}/tools`, `POST /v1/sessions/{id}/state`                                                                                                                                                                                                               | `InferenceResponse` (JSON or SSE), `ToolBatchResponse`, `204`                                                           |
| Session control    | `POST /v1/sessions/{id}/cancel`, `POST /v1/sessions/{id}/credit`, `POST /v1/sessions/{id}/configure`, `POST /v1/sessions/{id}/answer`, `POST /v1/sessions/{id}/close`, `DELETE /v1/sessions/{id}`, `POST /v1/sessions/prune`                                                                                                                                                | `CancelResponse`, `ConfigureResponse`, `StartResponse`, `DeletedResponse`                                               |
| Events             | `GET /v1/sessions/{id}/events?from_seq=`                                                                                                                                                                                                                                                                         | `text/event-stream` of `Event`                                                                                          |
| Processes          | `POST /v1/processes`, `GET /v1/processes`, `POST /v1/sessions/{id}/stdin`, `POST /v1/sessions/{id}/signal`, `POST /v1/sessions/{id}/wait`                                                                                                                                                                        | `ProcessStartResponse`, `ProcessListResponse`, `204`, `ExecOutput`                                                      |
| Models             | `GET /v1/models`, `POST /v1/models`, `DELETE /v1/models/{name}`                                                                                                                                                                                                                                                  | `ModelListResponse`, `ModelView`                                                                                        |
| Sources and tools  | `GET /v1/sources`, `POST /v1/sources`, `DELETE /v1/sources/{name}`, `POST /v1/tools/reload`, `POST /v1/tools/retire`                                                                                                                                                                                             | `SourceListResponse`, `SourceView`, `ReloadResponse`, `RetireResponse`                                                  |
| Policy and tenancy | `PUT /v1/policy`, `PUT /v1/tenants/{tenant}/limits`, `PUT /v1/tenants/{tenant}/policy`                                                                                                                                                                                                                           | `PutResponse`, `TenantLimitsView`                                                                                       |
| Administration     | `GET /v1/config`, `PUT /v1/config`, `GET /v1/config/history`, `GET /v1/admin/keys`, `POST /v1/admin/keys`, `DELETE /v1/admin/keys/{id}`, `GET /v1/tokens`, `POST /v1/tokens`, `DELETE /v1/tokens/{id}`                                                                                                           | `ConfigResponse`, `AppliedDeployment`, `ChangeListResponse`, `AdminKeyListResponse`, `TokenIssued`, `TokenListResponse` |
| Metrics            | `GET /v1/metrics`                                                                                                                                                                                                                                                                                                | `MetricsResponse`                                                                                                       |

The machine-readable form of this table is [`openapi/v1/openapi.json`](../openapi/v1/openapi.json):
`components.schemas` is the JSON Schema this repository already generates, and `paths` is every
route above with its status codes and parameters. It is committed and a test fails when it drifts:

```bash
NUCLEOS_OPENAPI=1 cargo test -p nucleosd --test openapi   # regenerate
cargo test -p nucleosd --test openapi                     # fail if stale
```

One layer, deliberately: **the syscalls** (`open`, `inference`, `tools`, `state`, `close`,
`credit`, `verdict`) are one step each, in an order the caller chooses. An application that wants a
loop — plan-execute, verification turns, map-reduce over items, its own sub-agents — calls these and
owns the loop. The daemon does not embed one.

`inference` takes its messages **in the request** and appends nothing: what you send is what you
get, and whether the answer enters history is decided by the next `state` call. That is what makes
a verification turn possible without polluting the transcript.

```text
Session control   describe · open · inference/tools/state · cancel · configure · close · list
Source management sources add/remove · tools reload · tools retire
```

The names describe the operations; the table above is the wire. `/v1/processes` and the
process routes are part of session control: a process session is a session whose turn is a
child process, and it is supervised by the kernel rather than by the caller.

### 1.1 The published artefact

One document: [`openapi/v1/openapi.json`](../openapi/v1/openapi.json). Its `components.schemas`
is every message the ABI carries, generated from the kernel's own types; its `paths` is every
route in the table above. An SDK generates from it and never copies it — a copy is a version that
drifts, and the drift is discovered by a client failing in production.

```bash
NUCLEOS_OPENAPI=1 cargo test -p nucleosd --test openapi   # regenerate
cargo test -p nucleosd --test openapi                     # fail if stale
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
  session:        caller-chosen id; a retried request with the same id resumes
  resume:         continue an existing session instead of creating one
  initial:        messages to seed the conversation with
  budget:         tokens / cost / steps / wall clock — carried in the checkpoint
  credit_tokens:  tokens granted up front; zero is not "no ceiling"
  sandbox:        workspace, sandbox policy, permissions
  config:         the mutable configuration to start from (same shape `configure` takes)
}
```

`Start` returns `{session, state, stop, next_seq}`. The generation is not in the response:
`GET /v1/describe` reports the one in force, and a session that migrates records
`generation_migrated`.

Sessions are addressed by a caller-chosen id. That makes client retries idempotent: a `Start` that
was retried after a timeout resumes the existing session instead of creating a second one.

Session identity is the pair **(tenant, session)**. The tenant comes from the authenticated
principal and never from a request field; two tenants may each own a session called `abc`
and neither can address the other's.

## 3. Two views of a session, and neither owns it

A stream is a view of the log, never the owner of the work:

- **Follow one turn as it runs** — `POST /v1/sessions/{id}/inference` with
  `Accept: text/event-stream` (§4.0.0). Frames are whole `Event`s and the last one is
  `message_completed`, so a client that reads only the last frame gets exactly what the
  non-streaming call returns.
- **Replay from a cursor** — `GET /v1/sessions/{id}/events?from_seq=N`. Reads a page and closes;
  an `EventSource` reconnects with `Last-Event-ID`, which is the same number as `seq`.

A client that disconnects and reconnects loses nothing: the work lives in the event log.
This is the single most valuable property of the ABI — a long-running session is not owned
by a request, so network distance, proxy timeouts and client restarts cannot lose work.

The replay is **paged**, not "everything since the beginning": a reconnect costs one page, and
the first frame does not wait for the last piece of history.

## 4. Events

```text
Event {
  seq:              uint64   // monotonic within a session, starts at 0
  session:          string
  at_unix_millis:   uint64
  schema_version:   uint32
  kind:             oneof { … }   // flattened into the event, tag once
  meta:             map<string,string>
}
```

The union below is the part the rules turn on, not the whole of it — the schema is the complete list,
and a kind this build does not recognise is carried rather than refused.
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

The page is chosen by the store with the subject already applied, so a short page means there is nothing
left — never that somebody else's rows were removed after the cursor was decided.

`kind` is a filter, and an unknown value is a `400` rather than an empty page: answering "nothing" to a
misspelled filter is how a caller concludes the feature is broken. Another subject's sessions are
absent rather than refused — a listing that names what the caller may not open is a directory of other
people's work.

`GET /v1/processes` reports a **summary** per process — session, argv, cwd, status — and never the
environment the command was given. A listing is a cross-session view, and `spec.env` is a credential as
often as a setting; the full spec stays behind `GET /v1/sessions/{id}`, which answers only to the
session's owner.

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

## 5. Budget is credit, granted at `Start`

`Start` carries `credit_tokens` and `budget` (tokens / cost / steps / wall clock). Each `usage`
event debits the credit; on exhaustion the session drains, writes a checkpoint, emits
`budget_exhausted`, and stops.

`POST /v1/sessions/{id}/credit` grants more: `{"amount": 10000}` adds tokens, and omitting `amount`
removes the limit. A session that stopped because its credit ran out is revived — open again, stop
reason cleared. The grant is recorded as `credited`, so a replay cannot mistake a revived session for
one that never stopped.

Two refusals, and both are about not lying to the caller. A session that ended any other way —
completed, cancelled, failed, denied — is done, not out of funds. And a session stopped by a **ceiling**
(`max_steps`, `max_tokens`, `max_cost`, `max_wall`) stays stopped: those were fixed at `Start`, a grant
does not move them, and reviving into a state where the next call is refused would report a session
open that cannot run.

The four `Budget` ceilings are unchanged: they answer "how much may this session ever consume",
which is not the question a grant answers.

Backpressure and cost control are the same mechanism, which is why there is no separate
rate-limit surface: the caller decides how much to grant at `Start`.

`credit_tokens: 0` is not "no ceiling": it closes the session on its first turn. Omitting the field
means no limit.

## 5.1 Retention

`POST /v1/sessions/prune` removes **closed** sessions whose last activity is before a deadline.
`dry_run` **defaults to true**: a request that does not say otherwise gets a report and deletes
nothing, with up to fifty of the matching sessions named so the operator can check first. Exactly one
of `before_unix_millis` and `older_than_millis` is required; `limit` bounds one pass (default and cap
1,000); `tenant` narrows it to one tenant, absent means all.

An open session is never pruned, however old it is — a parked session is waiting on a human, not
garbage. A session whose last activity is unknown is kept: retention deletes what it can date, never
what it cannot. Whole sessions only, so every survivor's `seq` stays dense and its resume cursor keeps
working.

The deployment document's `retention` field configures the same thing automatically:
`closed_session_ttl_millis` and `sweep_interval_millis`. Both absent means keep everything and never
sweep, which is the default. An interval with no TTL is refused.

## 6. Generations and reload

Tool sources are declared, never inlined. `POST /v1/sources` takes:

```text
SourceRequest {
  name:     string
  command:  string        // the MCP server to spawn (stdio)
  args:     repeated string
  env:      map<string,string>
  url:      string?       // an HTTP endpoint instead of a command
}
```

`url` wins when both are present: it is the field a newer row adds, and the two cannot both be true.
A row that names neither is refused at load with the source's name. The transport decides the
**isolation** recorded for the tools it advertises: `process` for a spawned child, `http` for an
endpoint.

The daemon connects over stdio, calls MCP `tools/list`, stores the command, and **publishes a
generation**. Removing a source publishes another. There is no pending set and no transactional
swap between several adds: each add or remove is one generation, which is why a failed connection
fails the request (the caller is waiting) while a source that will not start at boot is a warning
and a missing tool (`architecture.md` §8).

Two things a source does **not** carry today, stated rather than implied: a per-source tool
allow-list (the ceiling is per-session `config.tools`), and the derived isolation descriptor on the
wire (`SourceView` reports the command or the url and the tools it currently advertises; the
descriptor itself is on `ToolSpec`). A source asserting its own isolation was removed earlier for
being an unchecked claim.

`Reload` reconnects and publishes what the servers advertise now. Two properties matter:

- **Reload is explicit.** Nothing watches the filesystem. A caller decides when the
  generation changes, so a half-written config never becomes a generation.
- **Reload is best-effort per source.** A source that will not reconnect is logged and skipped; the
  response carries the generation and the tools that came back, not a per-source error list.

```text
ReloadResponse {
  generation: uint64
  tools:      repeated string   // every tool name in the new generation
  added:      repeated string   // names that were not in the previous one
  removed:    repeated string
}
```

Migration happens at turn boundaries only, and only when the old and new tool sets are
compatible — same tool names, and structurally equal input schemas (new optional properties
allowed). Anything else pins the session to its generation until it ends.

The second check is `schema_narrows`: every request the pinned schema accepted must still be
accepted by the new one. A rename, a removed property and a newly required one all fail it.

The third term is the **isolation descriptor**, carried on `ToolSpec`: `process` for a spawned
child, `http` for an endpoint, `in_process` for a built-in. It is derived by whoever connected the
tool, because it is a fact about the connection and not a claim the tool gets to make. A tool that
keeps its name and schema but now runs somewhere else is incompatible, and the session stays pinned.
`None` is not a wildcard: a generation that knows must not migrate onto one that does not say.

## 7. Errors

```text
invalid · not_found · denied · budget · timeout · conflict · cancelled · unavailable · internal
```

No numeric values: the code is a `snake_case` string, so a reader without the schema can tell what
it is and an enum cannot be renumbered by accident. `conflict` is the compare-and-swap answer —
re-read and retry — and `cancelled` is distinct from `timeout` because nothing went wrong.

Every response that can fail returns `code`, a human-readable `reason`, and a `retryable`
flag — never a bare string, and never an error that has to be parsed. A string error code
is the most expensive kind of debt: the first caller who compares strings freezes it.

`retryable` is a statement about the same request, not about the world. `unavailable` and
`timeout` are retryable; `budget` is not, because the limits are set at `Start` and carried in the
checkpoint — there is no route that raises them, so retrying the same request cannot succeed.

Two details make an error actionable rather than merely typed:

- `retry_after_millis`, because "retryable" does not say _when_. Zero means no hint, not
  "immediately".
- `details`, a machine-readable map — `{"needed_tokens": "40000"}` on `BUDGET` is the
  difference between a caller that can fix it and one that can only report it.

`GET /v1/sessions` pages by **keyset token**, not by offset: sessions are created and retired
constantly, so an offset skips or repeats rows. `next_page_token` empty means the last page; a
client stops on empty, never on a short page.

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

| nucleos              | MCP                                                                                       |
| -------------------- | ----------------------------------------------------------------------------------------- |
| tool source          | server (stdio child, or a streamable-HTTP endpoint)                                       |
| `Tools` (allow-list) | `tools/list` snapshot                                                                     |
| `ToolCallStarted`    | `tools/call`                                                                              |
| `ToolFinished`       | `tools/call` result: `content` blocks → `Content.parts`, `isError` → `is_error`           |
| cancel a tool call   | `notifications/cancelled`                                                                 |
| source isolation     | derived from the transport: `process` for a child, `http` for an endpoint                 |

Deliberately unsupported in v1: MCP **sampling** and **elicitation**. The daemon does not
advertise those client capabilities, so a server that needs them must fall back. Adding
them later is additive — they become events and a policy verdict respectively, not new
methods.

## 10. Edge cases, defined rather than discovered

Every one of these has a defensible alternative. The point is to pick one now, because the
alternative gets chosen accidentally by the first implementation.

| Case                                                          | Behaviour                                                                                                                                                                                                          |
| ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `inference` on a terminal session                             | `invalid`, naming the stop reason. A closed session is not revived; its log still replays through `GET /v1/sessions/{id}/events`.                                                                                  |
| Two concurrent turns on one session                           | The kernel does not refuse it, and `cancel` addresses the most recently started turn. One writer per session is a rule the caller keeps, not a lock the kernel holds — the log stays dense either way.             |
| `GET /events` with `from_seq` beyond the last event           | An empty stream. A cursor is a position, not an assertion about the log, so there is nothing to correct and nothing to clamp.                                                                                      |
| `Start` with `resume` on an unknown id                        | `not_found`. It does not create a session with that id — otherwise a typo becomes a new session and the real one is orphaned.                                                                                      |
| `POST /v1/sources` with a name that already exists            | Replaces the source under that name and publishes a new generation. Idempotent by name, so a retried add cannot create a duplicate source.                                                                         |
| `POST /v1/tools/reload` when nothing changed                  | Publishes a generation anyway: `added` and `removed` are empty and the number moves. Reload is not conditional.                                                                                                    |
| `state` while a turn is running                               | Applied immediately — messages are appended and checkpointed — so the next turn sees them. There is no queue to be ordered against.                                                                                |
| `close` on a running session                                  | Marks the session closed and checkpoints. It does **not** cancel a turn already running: `cancel` is the route for that, and it leaves the session open.                                                           |
| `cancel` twice                                                | The second answers `cancelled: false`. Not an error, and not a second log entry: there is no turn left to stop.                                                                                                    |
| An event kind the client does not know                        | Ignore it and keep the cursor. The union is additive, and the kernel itself carries an unknown kind rather than refusing the row.                                                                                  |
| A `PermissionOption` id the kernel did not offer              | `invalid`. The verdict names an option that was presented, or it is a client bug.                                                                                                                                  |
| Nobody answers an `AwaitingHuman`                             | The event carries `default_if_unanswered` (a refusal), and there is no sweeper that applies it: a parked session stays parked until a verdict or a close. Silence is never approval.                               |
| `GET /v1/sessions` with a `page_token` from an older page set | The token stays valid; it is a key, not an index. `invalid` only if it cannot be decoded at all.                                                                                                                   |
| `GET /events` before the session exists                       | `not_found`. A stream is a view of a session, and there is nothing to view yet.                                                                                                                                    |
| `describe` on an empty daemon                                 | Succeeds with `generation = 0` and no models. A health check must not need a session.                                                                                                                              |
| Removing a source a pinned session still uses                 | Allowed. The session holds its tool set, so the connection outlives the row; new sessions simply never see it.                                                                                                     |
| Adding a source while a reload is in flight                   | Serialized by one lock on the source set, so the generation a caller gets back is the one it published.                                                                                                            |
| A stream open across a reload                                 | Handled at the next turn boundary, which is the only moment the set may change, and recorded as `generation_migrated`.                                                                                             |

One case is deliberately left undefined: a request that races a `close`. The session drains and
ends; whether the request was consumed is observable in the log, and the caller must read it rather
than assume.

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

## 10.2 Parked questions

A policy that answers `ask` parks the session instead of failing it, and the question is one of two
kinds: a **tool call** (`pending_id` is the call's id) or a **command** (`pending_id` is the session's).
`POST /v1/sessions/{id}/answer` answers either:

```json
{ "pending_id": "call-1", "option_id": "allow_once" }
```

The answer is consumed before the work runs — tools and commands are not idempotent, and a client that
never saw the first response retries the same answer. Allowing a command runs it; refusing it records the
refusal and closes the session with `denied`. Nothing is driven afterwards: whether another turn follows
is the caller's decision, which is why this is one route and not a loop.

## 11. Not served, and why

| Not an endpoint                | Why                                                                                                                                                       |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Stateless model invocation     | This is an agent OS, not a gateway. A host that wants a raw model call links the library; see [`architecture.md`](architecture.md) §7.                    |
| Embeddings                     | A tool: an embedding model belongs with the index and the ranking that use it.                                                                            |
| Speech to text, text to speech | A tool. Voice output also needs an artifact store the kernel does not have.                                                                               |
| Raising the `Budget` ceilings  | Those are fixed at `Start` and carried in the checkpoint; a grant moves the balance, not the ceiling.                                                     |

An endpoint that exists and returns 501 is a contract nobody can rely on, so these are absent
rather than declared. Session cancel and list, and source registration and reload, are served —
see the table in §1.

## 12. Deferred

| Deferred                                   | Why it is safe to defer                                                                     |
| ------------------------------------------ | ------------------------------------------------------------------------------------------- |
| Auto-triggered reload (file watch, SIGHUP) | It is a trigger, not a mechanism: it will call `Reload`                                     |
| Multi-node placement                       | Sessions are recoverable from the store, so placement is a routing decision above the ABI   |
| Streaming tool arguments                   | `tools/call` is request/response; partial arguments are a model concern, not a wire concern |
| Per-session auth scopes                    | `Principal` is passed through already; scoping it is policy                                 |
