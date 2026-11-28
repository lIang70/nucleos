# nucleos

<p align="center">
  <a href="README.md">English</a> ·
  <a href="README_CN.md">简体中文</a>
</p>

> An agent OS kernel, delivered as a service with a stable ABI.

`nucleos` is a runtime for long-lived agent sessions. The kernel owns the session
state machine, the turn loop's syscalls, budget enforcement, tool generations and
the durable record; **everything else plugs in behind an interface** — models,
tools, storage, isolation, policy.

Two commitments decide the shape of everything else:

- **Mechanism in the kernel, policy outside it.** The kernel guarantees invariants;
  it never decides what is allowed.
- **The caller owns the conversation.** The kernel records what it actually sent
  and what it actually did. It does not keep a second copy of your dialogue, and
  therefore cannot disagree with you about it.

---

## What it is not

Not an agent framework, not a prompt library, not a RAG pipeline, not a model
gateway, not a cluster orchestrator, not a sandbox. Each of those is either a
seam the kernel calls or the caller's job.

---

## Four invariants

Each one is enforced by a type or a single entry point, not by a convention.

|        | Invariant                               | Enforced by                           | What breaking it costs                    |
| ------ | --------------------------------------- | ------------------------------------- | ----------------------------------------- |
| **I1** | One writer per session                  | a lease the host holds                | the log's sequence stops meaning anything |
| **I2** | Every state change is an immutable fact | one append path, private constructors | replay becomes a story instead of a state |
| **I3** | Budget is checked **before** spending   | a permit obtained before the call     | it is not a budget, it is a bill          |
| **I4** | The kernel records what **it** produced | facts are built inside the kernel     | the record becomes a claim                |

---

## Shape

```text
                    contract          the vocabulary: ids, content, tools,
                       ▲              model facts, confine, grant, facts
        ┌──────────────┼──────────────┐
      ports          wire           (adapters plug into ports)
        ▲              ▲
      kernel ──────────┘
        ▲
   hosts: the library, or the daemon over the ABI
```

| Crate             | What it is                                                                                                                                                                          |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/contract` | Pure types. No I/O, no async, no runtime. Every type derives `Serialize` + `JsonSchema`, so the ABI document is generated from it.                                                  |
| `crates/ports`    | The seams a host implements: generation, embedding, judgment, tools, process sessions, policy, stores, clock. No implementation, except two that are answers rather than stand-ins. |
| `crates/kernel`   | _(in progress)_ The session state machine, the journal, the gate, turns, replay.                                                                                                    |
| `crates/wire`     | _(in progress)_ The ABI: envelopes, the route table, error mapping, SSE frames.                                                                                                     |

The module graph is one-way and is checked: `contract ← ports ← kernel`, with
`wire` beside `contract`. Neither the kernel nor `wire` may name the other.

---

## Using it

Deploy once, then run one loop per session:

```text
deploy   declare models, tool sources, skill roots, policy
open     grant = { confine, tools, models, skills }   frozen, narrowable only
loop     inference → if finish == tool_calls → dispatch → append → again
close    close (reversible) · reopen · delete (tombstone)
```

The loop is the caller's, deliberately: the kernel does not decide that a turn
must be followed by another.

Both exit conditions are the caller's: it may stop because the model finished,
because the budget ran out, or because it already has the answer. Three exit
conditions, and the kernel decides exactly one of them.

---

## Status

`0.0.1`. Early, and honest about it.

|                        |                                                         |
| ---------------------- | ------------------------------------------------------- |
| ✅ Built and tested    | `contract` (90 tests), `ports` (23 tests)               |
| 🚧 In progress         | `kernel`, `wire`                                        |
| 📄 Designed, not built | the daemon, the CLI, the adapters, the OpenAPI document |

```bash
cargo test --workspace      # 113 tests
cargo clippy --workspace --all-targets
make fmt-check              # rustfmt + prettier, one gate for both
```

The ABI version is separate from the crate version: a kernel release may keep
speaking an older ABI, and conflating the two is how "we upgraded the daemon and
the SDK broke" happens.

---

## Documentation

|                                              |                                                         |
| -------------------------------------------- | ------------------------------------------------------- |
| [docs/architecture.md](docs/architecture.md) | the kernel: layers, invariants, one turn end to end     |
| each crate's own document                    | what it owns, what it refuses, the mistakes it invites  |
| `openapi/v1/openapi.json`                    | not yet: the published ABI, generated from the contract |

---

## License

Apache-2.0.
