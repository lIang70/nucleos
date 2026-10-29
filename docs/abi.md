# The ABI

The ABI is a permanent commitment. Once a caller depends on a field, that field exists
forever. Every decision below is made to keep the surface small rather than to keep it
convenient.

## 1. Shape

JSON over HTTP, with server-sent events for the two streams. The contract is
`schema/v1/schema.json`, generated from the kernel's own types — see [`wire.md`](wire.md) for the
encoding rules and why the schema is not a separate type tree.

| Route | Body | Returns |
| --- | --- | --- |
| `GET /v1/describe` | — | `DescribeResponse` — version, models, ABI version |
| `POST /v1/sessions` | `StartRequest` | `StartResponse`, or the error body |
| `POST /v1/sessions/{id}/verdict` | `VerdictRequest` | `StartResponse` |
| `GET /v1/sessions/{id}/events?from_seq=` | — | `text/event-stream` of `Event` |

Four routes, and the surface is deliberately the whole of v1's served ABI. The endpoints the
kernel can already support but that the daemon does not yet expose — credit, cancel, close,
list, tool-source registration, reload, the stateless gateway — are listed in §12 rather than
declared and left returning 501: an endpoint that exists but refuses is a contract nobody can
rely on.

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

### 1.1 The schema

One document, N generators. `schema/v1/schema.json` holds every type once, generated from the
kernel's own definitions; an SDK generates its types from it and never copies it — a copy is a
version that drifts, and the drift is discovered by a client failing in production.

```bash
NUCLEOS_SCHEMA=1 cargo test -p nucleos --test schema   # regenerate
cargo test -p nucleos --test schema                    # fail if stale
```

The default run only reads, so CI fails instead of quietly rewriting the file. What an SDK does
with the document is that ecosystem's decision; what this repository guarantees is that the
document matches the code.

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
- Events carry no back-references; a consumer can render from the stream alone. A client
  that needs the prompt must reconstruct it from the events, not ask the server for
  hidden state.

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
  isolation: none | process | vm
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

- `retry_after_millis`, because "retryable" does not say *when*. Zero means no hint, not
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

| nucleos | MCP |
| --- | --- |
| tool source | server (stdio or streamable HTTP) |
| `Tools` (allow-list) | `tools/list` snapshot |
| `ToolCallStarted` | `tools/call` |
| `ToolFinished` | `tools/call` result: `content` blocks → `Content.parts`, `isError` → `is_error` |
| cancel a tool call | `notifications/cancelled` |
| isolation `process` | server runs as a child process |

Deliberately unsupported in v1: MCP **sampling** and **elicitation**. The daemon does not
advertise those client capabilities, so a server that needs them must fall back. Adding
them later is additive — they become events and a policy verdict respectively, not new
methods.

## 10. Edge cases, defined rather than discovered

Every one of these has a defensible alternative. The point is to pick one now, because the
alternative gets chosen accidentally by the first implementation.

| Case | Behaviour |
| --- | --- |
| `Run` on a terminal session | Replays from `from_seq` to the last event, then ends the stream. A client never has to know the session already finished. |
| Two concurrent `Run` streams on one session | Allowed. Each cursor is independent; a stream is a *view*, and no stream can advance or alter state. |
| `Run` with `from_seq` beyond the last event | `INVALID`, not clamped. Silently skipping events hides a client bug until it costs somebody an incident. |
| `Start` with `resume` on an unknown id | `NOT_FOUND`. It does not create a session with that id — otherwise a typo becomes a new session and the real one is orphaned. |
| `Register` with a name that already exists | Replaces it in the pending generation. Idempotent by name, so a retried `Register` cannot create a duplicate source. |
| `Reload` with no pending changes | No-op. Returns the current generation with no failure and no new generation number. |
| `Send` while `AWAITING_TOOL` | Accepted and ordered; applied at the next turn boundary. Rejecting it makes callers retry into a race. |
| `Close` on a running session | Cancels, waits for the turn to resolve, checkpoints, then closes. A `Close` never discards a turn in flight. |
| `Credit` on a terminal session | `INVALID`. A finished session cannot be revived by paying more. |
| `Cancel` twice | Idempotent; the second call reports the state the first one produced. |
| An event kind the client does not know | Ignore it and keep the cursor. `type` is additive; a client that fails on an unknown kind breaks on every future feature. |
| A `PermissionOption` id the kernel did not offer | `INVALID`. The verdict names an option that was presented, or it is a client bug. |
| Nobody answers an `AwaitingHuman` | Apply `default_if_unanswered`, which is a refusal. Silence is never approval. |
| `List` with a `page_token` from an older page set | The token stays valid; it is a key, not an index. `INVALID` only if it cannot be decoded at all. |
| `Run` before `Start` | `NOT_FOUND`. A stream is a view of a session, and there is nothing to view yet. |
| `Describe` on an empty daemon | Succeeds with `generation = 0` and no models. A health check must not need a session. |
| `Deregister` a source a pinned session still uses | Allowed. The source stays alive until the last session on its generation ends; new sessions simply never see it. |
| `Register` while a `Reload` is in flight | Serialized. Reloads run one at a time and a registration lands in the pending set, so a caller never has to guess which generation it joined. |
| `Run` open across a `Reload` | Allowed. The migration happens at the next turn boundary and is announced as `generation_migrated` in the same stream. |

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

| Not an endpoint | Why |
| --- | --- |
| Stateless model invocation | This is an agent OS, not a gateway. A host that wants a raw model call links the library; see [`architecture.md`](architecture.md) §7. |
| Embeddings | A tool: an embedding model belongs with the index and the ranking that use it. |
| Speech to text, text to speech | A tool. Voice output also needs an artifact store the kernel does not have. |
| Tool-source registration, reload | Tool sources are declared in the daemon's configuration; discovery lands with the MCP client. |
| Credit, cancel, close, list | They need a session table and live handles the daemon does not hold yet. |

An endpoint that exists and returns 501 is a contract nobody can rely on, so these are absent
rather than declared.

## 12. Deferred

| Deferred | Why it is safe to defer |
| --- | --- |
| Auto-triggered reload (file watch, SIGHUP) | It is a trigger, not a mechanism: it will call `Reload` |
| Multi-node placement | Sessions are recoverable from the store, so placement is a routing decision above the ABI |
| Streaming tool arguments | `tools/call` is request/response; partial arguments are a model concern, not a wire concern |
| Per-session auth scopes | `Principal` is passed through already; scoping it is policy |
