# The wire

The ABI is JSON over HTTP, with server-sent events for streams. The schema is
`schema/v1/schema.json`, generated from the kernel's own types — the kernel's shape *is* the
wire shape, which is the trade this design makes in exchange for not maintaining a second type
tree.

> **Status.** This is the wire. The daemon serves it, `proto/` is gone, and the schema is
> generated from the kernel's own types.

## 1. One wire, two bindings

| Deployment | Listener | Authentication |
| --- | --- | --- |
| Local (default) | unix socket | peer credentials — uid and pid from the kernel |
| Remote (opt-in) | TCP + TLS | token or mTLS |

The difference is the listener and the authentication, never the protocol. A local caller is not
speaking a different dialect: `curl --unix-socket /tmp/nucleosd.sock` is a complete client.

This is why there is no separate "local IPC format". A bespoke frame format would buy
microseconds — measured at under 4% of a local round trip, against a model call six orders of
magnitude larger — and would cost a second implementation in every SDK.

## 2. Encoding

JSON per RFC 8259, UTF-8, `content-type: application/json`.

| Decision | Rule | Why |
| --- | --- | --- |
| 64-bit integers | JSON **numbers**, not strings | protobuf's canonical JSON mapping turns `uint64` into a string, so `event.seq + 1` silently becomes `"1231"`. Every value here is far below 2^53. |
| Enums | `snake_case` strings | `"tool_finished"`, not `20`. A reader without the schema can still tell what it is. |
| Discriminated unions | `{"kind": "…", …fields}` | Tag inline, fields beside it. One level of nesting, not two. |
| Unknown union member | **carry it, do not fail** | The union is additive. A client that errors on an unknown `kind` breaks on every future feature. |
| Absent vs null | absent means "use the default" | `Option` fields are omitted when `None`. `null` is not used and is not equivalent. |
| Empty vs missing | a missing list is an empty list | A caller should be able to send `{}`. |
| Unknown fields | ignored | A newer client talking to an older daemon is normal. |

`meta` — a `map<string,string>` on `Part`, `ToolCall` and `Event` — is the only extension hatch.
The kernel never reads it, never interprets it, and never rejects an unknown key.

## 3. Events are server-sent events

```http
GET /v1/sessions/{id}/events?from_seq=0
accept: text/event-stream

event: nucleos.v1.tool_finished
id: 42
data: {"seq":42,"session":"s1","kind":{"kind":"tool_finished", …}}

```

- `event:` is the event's type — the same string its `type_name()` returns.
- `id:` is its `seq`. An `EventSource` reconnect sends `Last-Event-ID`, and the server replays
  from there: the browser's own resume mechanism and the ABI's cursor are the same number.
- `data:` is the event, one JSON object per line.

`from_seq` is **inclusive**, and `Last-Event-ID` wins when both are present — a client that
reconnected did not mean to re-read what it already saw.

## 4. Errors

Every failure is the same body, whatever the endpoint:

```json
{"code": "denied", "reason": "read_file is not available", "retryable": false}
```

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid` | 400 | The request was malformed |
| `not_found` | 404 | The thing named does not exist |
| `denied` | 403 | Policy refused |
| `budget` | 429 | A budget was exhausted |
| `timeout` | 504 | A deadline passed |
| `unavailable` | 503 | A dependency was not reachable |
| `internal` | 500 | A bug |

`retryable` is a statement about this request, not about the world. `retry_after_millis` is
absent when there is no hint — absent, not zero.

## 5. Versioning

- The version is in the path: `/v1/…`.
- The schema is additive: fields are added, never removed, never renamed, never re-typed. An
  enum value is added, never renumbered.
- Changing a serialized field's name is a breaking change even though the Rust field may still be
  called something else — `#[serde(rename)]` is how a rename stays invisible on the wire.
- A breaking change is `/v2`, served beside `/v1`.

`nucleos.v1` is a promise, and `schema/v1/schema.json` is where it is written down. The drift test
fails when the code and the file disagree, which is the only enforcement mechanism this design
has — protobuf's compiler and `buf breaking` are both gone.

## 6. What this gives up

| Protobuf had | Now |
| --- | --- |
| A neutral source of truth, no language privileged | The Rust types are the source; other SDKs generate from the JSON Schema |
| `protoc` rejecting a circular import | A `$ref`-resolves test (the graph is genuinely recursive — `Content → Part → ToolResult → Content` — so acyclicity was never the real property) |
| Binary encoding | JSON: ~2× the bytes and ~90 ns more per event. Measured, and irrelevant against a model call |
| Generated client stubs | Generated types only; the HTTP client is thin enough to write |
| Compile-time field presence | `Option` plus a schema; a client still has to read the schema |
