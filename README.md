# nucleos

> An agent OS kernel, delivered as a service with a stable ABI.

`nucleos` is a runtime for long-lived agent sessions. The kernel owns the session
state machine, the turn loop, budget enforcement, tool-source generations, and the
event log; everything else — models, tools, storage backends, policy, transport
security — plugs in behind an interface.

It is a **service**: a daemon (`nucleosd`) exposes the kernel over gRPC, and callers
drive sessions through that ABI. The kernel itself is a library and never depends on
the wire contract.

## Status

🚧 **Design phase.** This repository holds the architecture, the ABI contract, and a
non-functional skeleton. No working kernel yet.

- [`docs/architecture.md`](docs/architecture.md) — the kernel: layers, contracts, session model
- [`docs/abi.md`](docs/abi.md) — the service ABI and its versioning rules
- [`docs/pitfalls.md`](docs/pitfalls.md) — decisions and the failure modes they prevent

## What it is not

Not an agent framework, not a prompt library, not a RAG pipeline, not a cluster
orchestrator. See the non-goals in the architecture document — they are as load-bearing
as the goals.

## Layout

```text
crates/
  nucleos-proto/   generated Rust bindings (committed)
  nucleos/         the kernel library — no dependency on the wire
  nucleosd/        the daemon — authn, translation, listener
proto/             the ABI contract — one source of truth for every SDK
docs/              design documents
```

Dependency direction is one-way and enforced by a structural test:
`nucleos-proto ← nucleosd → nucleos`.

The `.proto` files live at the root, not inside a crate, because they are the contract for
every language. The Rust crate publishes only the **generated** bindings, so a consumer
never needs `protoc` and never needs the source tree.

## License

[Apache-2.0](LICENSE).
