# Configuration and administration

**Status: partly built.** Step 1 of §10 exists: the two stores, one root, `--root`/`--socket`, and the
platform paths. Everything else — `init`, the configuration ABI, the admin surface, signatures — is
still a proposal, and this document says so where it matters so that a reader is never guessing which
half is real.

Built, today:

- `system.db` and `sessions.db` under one root, with `system.db` holding a schema version and the file
  existing for a reason rather than as a placeholder;
- `--root`, `--socket`, `--print-paths`, and platform defaults that are absolute (§4.1);
- the socket default is a private directory, never a bare `/tmp/nucleosd.sock`;
- **`Gated::Config`** and `Kernel::check_config` — the gate exists before the interface it protects.

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
  "root":   "/var/lib/nucleos",              // data: both stores, derived names
  "socket": "/run/nucleos/nucleosd.sock",    // runtime, often a different filesystem
  "log":    { "format": "json", "level": "info" }
}
```

Three keys, and each is here for a reason that cannot be deferred:

- **root** — without a database there is nowhere to write the configuration that arrives later, and
  one prefix is enough to derive everything else. Two flags, `--root` and `--socket`, override these;
  a second instance on one host is then one more `--root`.
- **socket** — the address has to be known before anyone can connect to change anything. It is not
  derived from `root` because it usually belongs on a different filesystem (§4.1).
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

### 4.1 Paths

Three rules, and everything below follows from them.

1. **One root, everything else derived.** `--root` names the data directory; the rest is computed.
   Only `--socket` may point elsewhere, because a runtime directory is routinely a different
   filesystem (tmpfs, or a per-user temporary directory).
2. **Three kinds of directory, with different permissions and different backup rules.** Mixing them
   is how a credential ends up in a backup.
3. **The working directory never matters.** A relative default is not a default, it is a bug — today
   `NUCLEOS_STORE=nucleos.db` writes to wherever the daemon was started from.

| Kind | Holds | Copied / backed up | Mode |
| --- | --- | --- | --- |
| **config** | `bootstrap.json`, `seal.key` | the file yes; **the seal key never alongside data** | `0400`/`0644` |
| **data** | both stores | **yes** — backups, copies to a laptop, support bundles | `0600` |
| **runtime** | socket, pid | never; recreated on start | `0600`, directory `0700` |

The seal key is **config, not data**. That is the answer to "where does the key-encryption key go":
the data directory is the thing that gets copied, so the key that makes copies useless lives outside
it.

#### Linux, as a system service

| Path | What | Mode | Backup |
| --- | --- | --- | --- |
| `/etc/nucleos/bootstrap.json` | root, socket, log format | `0644` | yes |
| `/etc/nucleos/seal.key` | the key that unwraps stored credentials | `0400 root` | **no** — a secret manager, if at all |
| `/var/lib/nucleos/system.db` | models, sources, policy versions, token hashes, admin keys | `0600` | yes |
| `/var/lib/nucleos/sessions.db` | sessions, events, snapshots | `0600` | optional — only to resume elsewhere |
| `/run/nucleos/nucleosd.sock` | the listener | `0600`, directory `0700` | never |
| `/run/nucleos/nucleosd.pid` | the pid | `0644` | never |
| stdout | logs, to the journal | — | never |
| `/etc/systemd/system/nucleosd.service` | the unit — the one place these paths are written down | `0644` | yes |

#### macOS, as a LaunchDaemon

Apple's layout, same three kinds:

| Path | What | Mode |
| --- | --- | --- |
| `/Library/Application Support/nucleos/bootstrap.json` | bootstrap | `0644` |
| `/etc/nucleos/seal.key` | seal key | `0400` |
| `/Library/Application Support/nucleos/system.db` | system store | `0600` |
| `/Library/Application Support/nucleos/sessions.db` | tenant store | `0600` |
| `/var/run/nucleos/nucleosd.sock` | listener (`/var/run` → `/private/var/run`) | `0600` |
| `/Library/LaunchDaemons/com.nucleos.daemon.plist` | the launchd job | `0644` |

As a **LaunchAgent** — the development case — macOS gives every user a private `$TMPDIR`, so the
socket needs no special handling:

| Path | What |
| --- | --- |
| `~/Library/Application Support/nucleos/` | both stores |
| `$TMPDIR/nucleos/nucleosd.sock` | listener, private to the user by construction |

#### Client side

| Path | What | Note |
| --- | --- | --- |
| `~/.config/nucleos/socket` (Linux), `~/Library/Application Support/nucleos/socket` (macOS) | one line: the socket path | written by `nucleos init`; this is how a client finds the daemon |
| *(the keychain)* | the administrator's private key | **not a file**; service `nucleos-admin` |

#### Backup classes

| Class | Paths | Why |
| --- | --- | --- |
| **must** | `system.db`, `bootstrap.json` | losing them means re-initialising |
| **optional** | `sessions.db` | only to resume sessions after a machine change |
| **never with data** | `seal.key` | backed up beside the store, the encryption is decorative |
| **disposable** | socket, pid | recreated at start |

One sentence to remember it by: **the seal key is never in the same backup as the store it seals.**

#### Socket permissions: what they mean for the application

Three different questions, answered in three different places, and conflating them is how a service
ends up with a permission model nobody can explain:

| Question | Answered by | Applies to |
| --- | --- | --- |
| May this host user talk to the daemon at all? | socket mode + directory mode | all callers |
| Which tenant is this? | bearer token, or a signed request | tenant callers |
| May this change configuration? | Ed25519 signature + `Gated::Config` | administrators |

| Socket mode | Who can connect | Effect on an application |
| --- | --- | --- |
| `0600` (default) | the daemon's own user, and root | an application under another user gets `EACCES` — it needs `sudo` or group membership |
| `0660` + group `nucleos` | members of that group | the usual shape for "service plus clients on one host", as `docker.sock` is `0660 root:docker` |

Two details that are easy to miss:

- **The directory mode matters as much as the socket's.** A `0660` socket inside a `0700` directory is
  unreachable: a client cannot traverse the path. The pair is `0750 <user>:nucleos` on the directory
  and `0660` on the socket.
- **Nothing in the protocol changes.** A refused connection is `EACCES` from `connect()`, which is a
  different failure from an invalid token or a bad signature, and should stay distinguishable.

`init` prints the two commands needed to open the socket to a group and does not run them: changing
group membership is a system change, not something an installer should do silently.

#### Diagnosis

`nucleosd --print-paths` prints every path it will use with its mode and whether it exists. Path
problems are almost entirely "which directory did it actually choose", and a table in a document rots
faster than a command that reads the same values.

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

✓ root              /var/lib/nucleos
✓ socket            /run/nucleos/nucleosd.sock
✓ seal key          /etc/nucleos/seal.key (0400, not in backups)
✓ system store      /var/lib/nucleos/system.db
✓ tenant store      /var/lib/nucleos/sessions.db
✓ admin key         kid=7f3a… (private key in the keychain, service "nucleos-admin")
✓ recovery password argon2id, 64 MiB
✓ bootstrap policy  default: deny; admins may change models and sources

next: start nucleosd, then `nucleos models add …`
```

Non-interactive, because containers and CI cannot answer a prompt:

```text
nucleos init --root /srv/nucleos --password-stdin --key-out /run/secrets/admin.key
```

Properties it must have:

- **Idempotent or refusing.** A second `init` refuses rather than rewriting a live installation.
- **The password is bootstrap and recovery only.** It enrols keys and re-enrols them when all keys are
  lost. It never authenticates an ordinary request.
- **Recovery needs local access.** It reads the store file; it is not reachable over the socket, or a
  remote attacker would have an unlimited oracle to grind against.
- **Rate-limited**, in memory: five attempts a minute, regardless of source. This stops a person at a
  keyboard, not an attacker with the database file — that one is answered by the hash cost, and the
  document says so rather than implying the limiter is a boundary (§6, and the module note in
  `nucleos-auth`).

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

The gate is `Gated::Config { scope, subject }`, where the subject is a name (`models`, `sources`,
`policy`, `tokens`, `admin_keys`) and the scope is `system` or `tenant/<id>`. Three properties follow,
and each is a test rather than an intention:

- **A tool rule grants nothing over configuration.** A deployment that allowed `read_file` has said
  nothing about whether anyone may add a model — which is the ability to point every session's
  traffic at a different server.
- **The scope travels with the request**, so a rule written for `tenant` cannot reach the system scope
  even when the subject matches.
- **`ask` becomes a refusal.** There is no session here to park until a human replies, and an
  unattended question that expires into an answer would be a gate that opens by itself.

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

1. **Split the store — done.** System and tenant, two files, one root, platform paths, and the two bad
   defaults replaced (§4.1). The `scope` column arrived earlier, with `(tenant, session)`.
2. **`Gated::Config` — done.** `Gated::Config { scope, subject }`, a `config` field and a `scope`
   field in the policy document, and `Kernel::check_config`. Two properties are pinned by tests: a
   rule that allows `tool: "*"` grants nothing over configuration, and an `ask` answer becomes a
   refusal, because there is no session to park.
3. **`nucleos init`** + bootstrap file + recovery password. Until this exists, step 4 has no bootstrap
   credential.
   *Partly done: the recovery password exists — Argon2id, PHC-encoded so raised costs do not
   invalidate it, stored under `auth/recovery_password` in the system store, verified only where the
   store file is readable. 64 MiB and three passes cost about 1.5 s per hash on a laptop, which is the
   intended trade and the reason nothing on a request path may verify one. Still missing: the `init`
   command that asks for it, and the key enrolment it exists for.*
4. **Configuration ABI** — `/v1/config`, models, sources, tokens, policy; one event per change.
5. **CLI** — a thin client: send the request, print the event that was recorded.
6. **Ed25519 signatures** — last, because it protects what steps 1–5 built. Until then, a single
   bootstrap administrator token (§7) stands in, and the token is understood to be temporary.

## 11. Open questions

1. **Where does the bootstrap file live when `/etc` is not writable** (a container with a read-only
   root)? `NUCLEOS_DEPLOY` is a stopgap; a "configuration from the environment once, then never"
   mode may be needed. The same question applies to `seal.key`, which cannot live in the data
   directory by design (§4.1).
2. **Tenant tokens**: keep bearer tokens, or move to short-lived EdDSA tokens signed by the daemon?
   The second needs the daemon to hold a signing key, which is a smaller risk than an admin one but
   not zero.
3. **Do tenant administrators exist as a concept**, or is tenant config always written by a system
   administrator on a tenant's behalf? The former needs an identity layer this ABI does not have.
4. **Versioning and rollback of policy**: keep the last N versions and allow `nucleos policy rollback`?
   The events already record every version; the question is whether the store keeps the documents.
5. **What `GET /v1/config` exposes to a tenant caller** — nothing, probably; this is an admin surface.
6. **Whether `seal.key` is one file per root or one per host.** Per root keeps instances independent
   and means a copy of one instance's data is useless without its own key; per host is fewer things to
   lose. Currently written as per root, which is also what the paths table implies.
