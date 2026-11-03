# nucleos

> An agent OS kernel, delivered as a service with a stable ABI.

`nucleos` is a runtime for long-lived agent sessions. The kernel owns the session
state machine, the turn loop, budget enforcement, tool-source generations, and the
event log; everything else — models, tools, storage backends, policy, transport
security — plugs in behind an interface.

It is a **service**: a daemon (`nucleosd`) exposes the kernel over JSON/HTTP — with
server-sent events for streams — and callers drive sessions through that ABI. The kernel
itself is a library and never depends on the wire contract.

## Status

`0.0.1-preview`. The kernel, the daemon, the CLI and the reference loop are built and
tested; the ABI is `/v1`. This is not yet published as a stable release.

- [`docs/architecture.md`](docs/architecture.md) — the kernel: layers, contracts, session model
- [`docs/abi.md`](docs/abi.md) — the service ABI and its versioning rules
- [`docs/wire.md`](docs/wire.md) — encoding, framing and what the wire gives up
- [`docs/pitfalls.md`](docs/pitfalls.md) — decisions and the failure modes they prevent
- [`docs/config.md`](docs/config.md) — configuration and administration, and which half is built
- [`docs/reviews/`](docs/reviews) — the architecture and kernel audits, kept as snapshots of a revision
- [`openapi/v1/openapi.json`](openapi/v1/openapi.json) — the committed ABI document

## What it is not

Not an agent framework, not a prompt library, not a RAG pipeline, not a cluster
orchestrator. See the non-goals in the architecture document — they are as load-bearing
as the goals.

## Layout

```text
crates/
  nucleos/         the kernel library — contracts, kernel (syscalls), adapters, wire
  nucleos-agent/   the reference loop, built on those syscalls
  nucleos-openai/  one provider adapter (the whole OpenAI-compatible family)
  nucleos-mcp/     MCP tool sources
  nucleos-policy/  deny-by-default policy, layered system + tenant
  nucleos-sqlite/  the event log, checkpoints and the system store
  nucleos-auth/    sealed credentials, Ed25519 admin signatures, tokens, recovery
  nucleos-testkit/ test doubles — no binary depends on it
  nucleos-cli/     `nucleos init | config | models | tokens | sources | admin | seal`
  nucleosd/        the daemon — authentication, wire translation, listener
openapi/     the published OpenAPI document, committed
docs/        design documents
```

One ABI: JSON over HTTP, with server-sent events for streams. The document is generated from the
route table and the kernel's own message types, and committed, so an SDK generates from it and
never needs a compiler toolchain to read the contract.

## License

[Apache-2.0](LICENSE).
