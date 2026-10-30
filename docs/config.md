# Configuration and administration

**Status: proposal, under discussion.** Nothing in this document is built. The parts of the
architecture it would replace are named, so the cost of adopting it is visible.

Today every deployment decision is an environment variable — fifteen of them, read in four places,
validated at whatever line happens to need them. There is no file, no single check that says "this
deployment is valid", and no way to change anything without a restart except the tool source. This
document proposes replacing that with: a **first-run command**, **configuration in the store**, and
**signed administration**.

---

## 1. The problem

1. **Fifteen flat environment variables** with no grouping, no schema and no validation. A missing
   `NUCLEOS_MODEL_URL` fails somewhere in the middle of a builder; an empty `NUCLEOS_TOKEN` fails in
   a different place; a malformed `NUCLEOS_MCP` in a third.
2. **Nothing can change without a restart** except tools — including the policy, which is the thing
   an operator most needs to tighten *during* an incident.
3. **Secrets are environment variables**, visible in `ps -e`, `/proc/*/environ` and container
   manifests.
4. **`NUCLEOS_STRICT_TOOLS=0` and `NUCLEOS_STRICT_TOOLS=1` both mean "on"**, because presence is the
   flag. A configuration value that lies about itself.

## 2. What replaces it

| Today | Proposed |
| --- | --- |
| 15 env vars | `nucleos init` once, then `nucleos …` commands |
| no validation | one validation at init, and one per change |
| restart to change | a request that publishes a new version |
| secrets in env | keychain for the administrator's key; `0600` files for the daemon's |
| no record of who changed what | a signed event per change |

**One command, one event.** Every mutation is a request that the kernel records — the same rule as
`generation_migrated` and `session_configured`. A configuration that changes without a line in the log
is indistinguishable, afterwards, from a bug.

## 3. Layers, and what may change

The existing five layers do not disappear; two of them become writable.

| Layer | Owner | Lifetime | Changeable |
| --- | --- | --- | --- |
| **Bootstrap** | the operator, once | restart | never (it is the address of the process) |
| **System config** | an administrator | until changed | yes, signed |
| **Tenant config** | a tenant administrator | until changed | yes, signed, **narrowing only** |
| **Session wiring** | the application | the session | no — `configure` cannot reach it (unchanged) |
| **Per call** | the application | one request | n/a (unchanged) |

The rule for what belongs where stays one sentence: **a change is allowed at runtime when it can be
expressed as a new immutable version that a session either pins to or reads next call.** A policy, a
token, a model, a tool source: yes. The store path and the socket: no — those are the identity of the
process, not its policy.

### 3.1 Bootstrap

The minimum a daemon needs before it can be told anything:

```jsonc
// /etc/nucleos/bootstrap.json — the only file, and it is small on purpose
{
  "store": "/var/lib/nucleos",
  "socket": "/run/nucleos/nucleosd.sock",
  "log": { "format": "json", "level": "info" }
}
```

Three keys, and each is here for a reason that cannot be deferred:

- **store** — without a database there is nowhere to write the configuration that arrives later.
- **socket** — the address has to be known before anyone can connect to change anything.
- **log format** — a daemon that cannot report why it failed is one you debug blind. The *level* is
  mutable afterwards; the *format* is not, because half a log in JSON and half in text is not a log.

Everything else is transient by design. If this file were the whole configuration we would have
rebuilt what we are removing, only in a different syntax.

`NUCLEOS_DEPLOY` may name a different path, for a deployment that cannot use `/etc`.

## 4. Storage: two databases, two lifecycles

| | **system store** | **tenant store** |
| --- | --- | --- |
| Holds | models, tool sources, policy versions, tokens (hashed), admin public keys, schema version | sessions, events, snapshots |
| Writes | a few per day | one per token |
| Backup | every change, kept | periodic, pruned |
| Prune | never | by age |
| Upgrade | rarely | with the kernel |

Not "cleaner" — the lifecycles differ. Session data is pruned and restored; configuration is版本ed and
kept. One file would either never be pruned or prune the configuration with it. Two files also keep
configuration writes out of the session write-ahead log, and a corrupt sessions database does not take
the model registry with it.

**One tenant database, not one per tenant.** Per-tenant files mean scanning a directory to enumerate
tenants and migrating N databases on upgrade. Tenancy stays a `scope` column, exactly like the
`(tenant, session)` key that already made isolation structural rather than checked.

**What this is not: atomic across both.** "Refuse to delete a model that a live session routes to"
reads the tenant store and writes the system store with no transaction between them. A session created
in that window keeps a dangling reference and falls back on its next call. That is the honest
behaviour — a cross-store transaction here would cost more than the window it closes.

## 5. Tenant configuration narrows, never widens

A tenant administrator may restrict their own users, never lighten what the system decided:

| Setting | Composition |
| --- | --- |
| Policy | both documents evaluated; the stricter verdict wins (`deny` > `ask` > `allow`); if either defaults to deny, so does the result |
| Tool ceiling | intersection |
| Model allow-list | intersection |

This reuses the rule the kernel already applies to `config.tools` and `config.sandbox`: a narrower
scope can take room away and cannot add it. Anything else would make a tenant the authority on its own
permissions.

## 6. Secrets: who holds what

The table is the design. Everything else in this section follows from it.

| Secret | Holder | Where | If it leaks |
| --- | --- | --- | --- |
| Administrator **private key** | the administrator | **OS keychain** | that administrator is revoked; delete the public key |
| Administrator **public key** | the daemon | system store | nothing |
| Recovery password | the administrator's head | **argon2id hash** in the system store | needs the store file as well, and local access |
| Tenant token | that tenant's client | **hash** in the system store | that tenant only; revocable immediately |
| Model API key | the daemon | file `0600`, or env | the provider bill; rotate the key |

**The daemon holds nothing that can mint an administrative credential.** That property is worth more
than the choice of token format, and it is the reason for the two decisions below.

### 6.1 Why not a generated secret and JWTs

"Generate a secret at init, then issue JWTs" is HS256: the signing key lives **in the daemon's
store**. A backup of that store then grants permanent, forgeable, unattributable administration — the
exact failure the previous section exists to avoid.

| Scheme | Signing material lives | What a leaked store grants |
| --- | --- | --- |
| Shared bearer token (today) | the store | acting as administrator |
| HS256 JWT | the store | acting as administrator, with any claims, with no trace |
| **Ed25519 request signature** | **the administrator's keychain** | **nothing — only public keys are stored** |

If JWT is wanted anyway, it must be **EdDSA**, with the private key on the administrator's machine.
At that point JWT's only remaining advantage over a signature over a canonical string is `exp`/`nbf` —
and it brings `alg` confusion, `none`, clock skew and a `jti` denylist to solve. For a local socket and
a ±60 s window, the canonical string is smaller and has fewer ways to be wrong.

JWT *is* a reasonable fit for **short-lived tenant tokens**, where the blast radius is one tenant and
the risk class is different. That is a separate decision, not this one.

### 6.2 Why keychain for the client, and files for the daemon

A keychain is available where a human is logged in, and not where a service runs:

| | Administrator CLI | Daemon |
| --- | --- | --- |
| Runs in | a login session | `launchd` (LaunchDaemon) / `systemd` (system unit) |
| Keychain available | yes, unlocked by the login | **no**: macOS would need the System keychain; Linux needs a Secret Service **D-Bus session**, which a system unit does not have |
| Must start unattended after reboot | irrelevant | **required** |

So: the administrator's private key goes to the keychain — that is exactly its purpose. The daemon's
own secrets stay in `0600` files, and the design keeps that list as short as possible: ideally the only
one is the model API key, which has to reach a third party.

## 7. Bootstrap: `nucleos init`

```text
$ nucleos init
store [/var/lib/nucleos]:
recovery password: ****
repeat: ****

✓ system store      /var/lib/nucleos/system.db
✓ tenant store      /var/lib/nucleos/sessions.db
✓ admin key         kid=7f3a… (private key in the keychain, service "nucleos-admin")
✓ recovery password argon2id, 64 MiB
✓ bootstrap policy  default: deny; admins may change models and sources

next: start nucleosd, then `nucleos models add …`
```

Non-interactive, because containers and CI cannot answer a prompt:

```text
nucleos init --store /srv/nucleos --password-stdin --key-out /run/secrets/admin.key
```

Properties it must have:

- **Idempotent or refusing.** A second `init` refuses rather than rewriting a live installation.
- **The password is bootstrap and recovery only.** It enrols keys and re-enrols them when all keys are
  lost. It never authenticates an ordinary request.
- **Recovery needs local access.** It reads the store file; it is not reachable over the socket, or a
  remote attacker would have an unlimited oracle to grind against.
- **Rate-limited**, in memory: five attempts a minute, regardless of source.

## 8. The administrative surface

Every mutation is a request, signed, recorded, and gated.

```text
GET    /v1/config                 what this daemon is actually running
POST   /v1/models                 add or replace a model
DELETE /v1/models/{name}          remove one
POST   /v1/sources                add a tool source   → publishes a generation
DELETE /v1/sources/{name}         remove one          → publishes a generation
PUT    /v1/policy                 replace the system policy (versioned)
POST   /v1/tokens                 issue a tenant token
DELETE /v1/tokens/{id}            revoke one, immediately
POST   /v1/admin/keys             enrol another administrator's public key
DELETE /v1/admin/keys/{id}        revoke one
```

Two gates, and they are different questions:

| Question | Answered by | Where |
| --- | --- | --- |
| Is this request from an administrator? | signature | transport (`nucleosd`) |
| May *this* administrator change *this*? | **policy** (`Gated::Config`) | kernel |

The second cannot be dropped. A signature proves *who*; it does not prove *what*. And since the policy
is itself configuration, the bootstrap version must be inline in `init` — the shipped default is
`deny`, with an example that lets administrators change models and sources only.

Deletion is the hard direction, and each kind has its own answer:

| Deleting | Behaviour |
| --- | --- |
| A tool source | publish a generation without it; sessions pinned to the old one keep working |
| A model | refuse while a live session's routing chain names it; the check reads the tenant store and is best-effort (see §4) |
| A token | immediate, because revocation that waits is not revocation |
| An admin key | immediate; warn loudly if it was the last one — recovery is still possible locally, and locking yourself out is worse than a weaker fallback |

## 9. What is *not* signable or changeable

- **store path, socket, log format** — the identity of the process (§3.1).
- **the sandbox mechanism** — a property of the platform, not a setting.
- **kernel build-time wiring** — the clock, the store implementation, the policy engine. Those are
  host code, not deployment data.

## 10. Order of work

Each step is useful on its own, and none of them requires the next.

1. **Split the store** — system and tenant, `scope` column, two files. This decides the shape of every
   table that follows.
2. **`Gated::Config`** — the gate before the interface. Deny by default.
3. **`nucleos init`** + bootstrap file + recovery password. Until this exists, step 4 has no bootstrap
   credential.
4. **Configuration ABI** — `/v1/config`, models, sources, tokens, policy; one event per change.
5. **CLI** — a thin client: send the request, print the event that was recorded.
6. **Ed25519 signatures** — last, because it protects what steps 1–5 built. Until then, a single
   bootstrap administrator token (§7) stands in, and the token is understood to be temporary.

## 11. Open questions

1. **Where does the bootstrap file live when `/etc` is not writable** (a container with a read-only
   root)? `NUCLEOS_DEPLOY` is a stopgap; a "configuration from the environment once, then never"
   mode may be needed.
2. **Tenant tokens**: keep bearer tokens, or move to short-lived EdDSA tokens signed by the daemon?
   The second needs the daemon to hold a signing key, which is a smaller risk than an admin one but
   not zero.
3. **Do tenant administrators exist as a concept**, or is tenant config always written by a system
   administrator on a tenant's behalf? The former needs an identity layer this ABI does not have.
4. **Versioning and rollback of policy**: keep the last N versions and allow `nucleos policy rollback`?
   The events already record every version; the question is whether the store keeps the documents.
5. **What `GET /v1/config` exposes to a tenant caller** — nothing, probably; this is an admin surface.
