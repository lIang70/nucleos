# Architecture

> The kernel's job is to be **correct**: a session's state never goes backwards, a
> budget is never exceeded, an event is never lost. Everything about _policy_ —
> which model, which tools, who may call what, how much anything costs — is
> injected.

This document is the shape of the system: what the parts are, what each one
promises, and how a single turn moves through them.

---

## 1. Two commitments

| Commitment                                     | Means                                                                                    | Consequence                                                                                     |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| **Mechanism in the kernel, policy outside it** | the kernel guarantees invariants; it never decides what is allowed                       | one kernel serves deployments with different rules without a fork                               |
| **The caller owns the conversation**           | the kernel records what it actually sent; it does not keep a second copy of the dialogue | there is no second truth to disagree with, so no cursor, no history merge, no compaction action |

The second one is the decision this design is built around, and §6 is about it.

---

## 2. What it is not

Not an agent framework: the loop is the caller's. Not a model gateway: there is no
"call a model without a session" endpoint, because budget enforcement happens
before spending and that needs a session. Not a RAG pipeline: retrieval is a tool.
Not a cluster orchestrator: it says who owns a session, it does not schedule.
Not a sandbox: it calls an isolation backend and records what it enforced.

---

## 3. Four invariants

Every one is carried by a type or a single entry point. If it were a convention, it
would be a suggestion.

|        | Invariant                                | Enforced by                                                        | Why it is not negotiable                                                    |
| ------ | ---------------------------------------- | ------------------------------------------------------------------ | --------------------------------------------------------------------------- |
| **I1** | One writer per session                   | a lease the host assembles; `&mut` appears exactly once            | the log's dense, ordered sequence is the basis of replay, cursors and audit |
| **I2** | Every state change is one immutable fact | a single append path; facts are constructed only inside the kernel | replay is a state, not a story                                              |
| **I3** | Budget is checked before spending        | a permit obtained before any model or tool call                    | a check after the spend is a bill                                           |
| **I4** | The kernel records what **it** produced  | fact constructors are private                                      | otherwise the record is the caller's claim                                  |

Three consequences worth stating because they are easy to get wrong:

- **A failed attempt still pays.** A stream that broke after producing text cost
  money, so the ledger records it. An unreported counter stays unreported rather
  than being counted as zero.
- **A side effect that started and never reported an outcome is not guessed.**
  Retrying repeats it; skipping loses it. The session carries the uncertainty and
  refuses to run the next turn until the caller acknowledges it.
- **The kernel does not refuse what it cannot judge.** A tool result it never
  dispatched is _recorded_ as unverified, not rejected: rejecting would only push
  the same work into prose inside `instructions`, where nothing is visible.

---

## 4. Layers

```text
contract   pure types                                   no I/O, no async, no runtime
   ▲
 ports     the seams a host implements                   no implementation
   ▲
 kernel    the state machine, journal, gate, turns       knows only ports
   ▲
 wire      envelopes, routes, error mapping, frames      knows only contract
   ▲
 hosts     the library (embed) · the daemon (service) · the CLI
```

The graph is one-way and it is checked, not remembered: `wire` may not name
`kernel`, `kernel` may not name `wire`, and neither may name a transport. A
structural test reads the manifests rather than trusting review.

`contract` carries no trait and no async, because a trait needs a runtime and a
runtime is a decision the host makes. That is also why the ABI document can be
generated from it: an SDK generator reads one JSON file and never compiles Rust.

---

## 5. The seams

Six, and a new one needs an argument: each is something a host must implement,
and a host that must implement seven things usually implements the seventh badly.

| Seam          | Question                                     | Shape                                             |
| ------------- | -------------------------------------------- | ------------------------------------------------- |
| **Generator** | how do I turn a request into an answer?      | one call, or one stream of events                 |
| **Embedder**  | how do I turn documents into vectors?        | a batch in, vectors out, in order                 |
| **Decider**   | how do I answer a batch of typed questions?  | questions in, typed answers and refusals out      |
| **Tool**      | how do I run one call?                       | `execute(binding, args)`; failures become content |
| **Runner**    | how do I start and manage a process session? | sessions, read by cursor — not one-shot calls     |
| **Clock**     | what time is it, for a test?                 | a deterministic clock behind one method           |

Two shapes are deliberate:

- **A vendor is one struct, not one interface per capability.** `Provider` holds a
  required generator and optional embedder, decider and catalog; an absent seam is
  `None`, not a stub that always answers "unsupported". Its fields are private
  behind a builder, so a provider without a generator is not a value that fails
  validation — it is a value that cannot be written.
- **A tool sees a session through two methods.** An id and a scratch drawer, plus
  the capabilities it _declared_. A capability it did not declare reads as absent,
  which is how "a tool cannot be given authority it did not ask for" is enforced
  without copying or nil-ing anything.

---

## 6. The caller owns the conversation

The common shape is the opposite: the kernel stores the message list and the caller
sends an increment. That shape buys a server-side prompt assembly, and it costs a
machine of its own — a history cursor, a merge rule, a compaction action, a
"commit this turn or not" flag, and a digest chain to detect that the caller and
the server disagreed about what the history was.

Here the caller sends the whole `messages` array, byte for byte, and the kernel:

1. checks it is well formed (every tool call answered, every name callable);
2. checks the model can take it (input kinds, output mode, limits);
3. records `PromptSent { messages, digest, count, bytes }` — content-addressed, so a
   message repeated across ten turns is one blob;
4. forwards it unchanged, adding only what the kernel owns: the tool set and the
   preamble (a skill section, one line per skill).

What that removes: the history cursor, the merge rule, the compaction action, the
commit flag, the digest chain, the pinned dropped blobs. What it removes _as a
problem_: divergence. Two copies cannot disagree when there is one.

What it costs, said plainly:

- the server cannot assemble your context for you — a skill section is rendered by
  the kernel, anything else is yours;
- an imported conversation that names tools this deployment does not have is
  refused, because the model cannot call a tool it was never offered;
- compaction is your edit of your array. It is still _visible_ — the next
  `PromptSent` has a different `count` and `digest` — it is simply not an action
  the kernel performs.

---

## 7. One turn, end to end

```text
caller                     kernel                          adapters
  │                          │
  ├─ POST /inference ───────▶│
  │                          ├─ lease: one turn at a time (423 + Retry-After)
  │                          ├─ grant: confine, tools, models
  │                          ├─ capability: modes, input kinds, limits
  │                          ├─ budget: reserve before spending
  │                          ├─ expose: which tools the prompt carries
  │                          ├─ append TurnStarted            ← the commit point
  │                          ├─ append PromptSent
  │                          ├─ route: walk the chain ───────▶│ generate_stream
  │                          │◀─ events ──────────────────────┤
  │                          ├─ fold: text, tool calls, usage
  │                          ├─ settle: ledger, usage, stop reason
  │                          ├─ append MessageCompleted | TurnFailed | AttemptFailed
  │◀─ message + outcome ─────┤
  │   + warnings + seq       │
```

**The commit point is `TurnStarted`.** Before it, a failure costs nothing and is a
status code. After it, the log is the record and the caller learns the outcome from
facts. A turn that produced no record must not look like a turn that is still
running, which is why the three endings are exhaustive.

A tool call does not execute here. The kernel records that the model asked, ends the
turn, and the caller dispatches in a second request — so a tool can fail, wait for a
human, or fan out, while the turn that asked for it is already over and already
billed.

---

## 8. Concurrency

A session runs **one turn at a time**. A second concurrent call gets `423 Locked`
with `Retry-After`; `cancel` takes effect immediately, while `narrow` and `close`
wait.

There is no `ETag`, no `If-Match`, no `412`. That header answers "is the server's
history still there", and there is no server history. What remains is that two
turns must not both spend — a lock, not a compare-and-swap.

A retried `inference` is a second turn, and it spends again. "Did the last one
land" is answered by reading the log for that step's `message_completed`, not by a
table keyed by an idempotency key that would have to age with retention.

---

## 9. Isolation

Four orthogonal dimensions, not one ladder: files (read roots, write roots,
read-only), network (default / proxy / allow-list / deny-all, with rules and TLS
interception), environment (allow-list plus injected values), and resources (cpu,
memory, disk, output). A single ordered enum reads well in a table and cannot
express "writes under `/srv`, no network, and only these four environment
variables".

A backend **declares** what it can enforce, and a policy it cannot enforce is
refused at construction rather than downgraded at runtime. A backend also
distinguishes a boundary refusal from an execution failure — the refusal means
_nothing ran_, which is what makes it safe to try a command and ask a human only
after the boundary said no.

---

## 10. The record

```text
facts      (session, seq) → at, kind, body      append-only, dense, ordered
checkpoints session       → version, seq, state  a cache of the log
blobs      id = hash      → content              content-addressed, deduplicated
```

The log is the truth and the checkpoint is a cache of it. Recovery is
`load(checkpoint)` then fold the tail; the store refuses a checkpoint that is not
strictly newer than what it holds, which is how a stale handle is detected.

Every fact is also an ABI: a streamed frame _is_ a fact, so the vocabulary is
closed on purpose and a client that meets an unknown kind ignores it rather than
failing.

Two axes of observability, because one cannot answer the other's question:

|                | Carrier                               | Answers                                |
| -------------- | ------------------------------------- | -------------------------------------- |
| **vertical**   | the fact log                          | why did this session turn out this way |
| **horizontal** | trace context, carried as a parameter | where did the time go across services  |

---

## 11. Status

| Crate                 | State                                     |
| --------------------- | ----------------------------------------- |
| `contract`            | built — 87 tests, every type has a schema |
| `ports`               | built — 23 tests                          |
| `kernel`              | in progress                               |
| `wire`                | in progress                               |
| daemon, CLI, adapters | designed, not built                       |

---

## 12. Rejected designs

Recorded because they will come back with better packaging.

| Rejected                          | What it would buy                                         | Why not                                                                                             |
| --------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| The kernel holds the conversation | server-side prompt assembly, a shared view across clients | it creates a second copy, and the machine needed to reconcile two copies is larger than the feature |
| A stateless model-call endpoint   | one place for routing, retries, accounting                | once it exists, "why can't it be embedded" has no answer except "yet"                               |
| The kernel owns compaction        | callers do not have to think about windows                | it is editing data it does not own, and it must pick one of dozens of strategies                    |
| A caller-chosen sandbox policy    | simpler requests                                          | a caller that can name its policy can name the least confined one                                   |
| `ETag` / `If-Match` for turns     | optimistic concurrency                                    | it answers a question that no longer exists; a lease is the actual requirement                      |
| A `record_event` syscall          | drivers would not construct facts                         | its only real use is forging facts the kernel would have to treat as true                           |

---

## 13. Where the detail lives

In the code, one crate at a time. Each crate opens with a document that states what
it owns, what it refuses, the invariant it keeps, and the mistakes it invites —
`contract` explains why it has no trait and no async, `ports` explains why an absent
seam is `None` rather than a stub.

The shape of a crate document is the same as the shape of this one, one level down:
the invariant first, then the type that carries it, then the cost of the design that
was chosen over the alternative.
