# FORMAT — Canonical session export (WL-040)

The interchange contract for `weave session export` / `weave session import`. This
is a **logical, portable, schema-versioned JSON document** for resuming a session
across distinct weave instances (`cross_agent_session_resumer` / casr parity).

It is one of **three** distinct weave "export" surfaces — do not confuse them:

| Surface | Command | Form | Scope | Portable? |
|---|---|---|---|---|
| **WL-034** | `weave export` | HTML | presentation (offline mailbox view) | viewer-only |
| **WL-035** | `weave backup` / `restore` | USTAR tar (binary DB snapshot) | byte-exact, **host-local** | no (ids/host identical) |
| **WL-040** | `weave session export` / `import` | **canonical JSON** | logical session (messages + memory) | **yes** (ids re-minted on import) |

The one-line rule: **WL-040 is logical + portable + versioned; WL-035 is byte-exact
+ host-local.**

---

## The envelope

A single JSON object. Field order is the stable key order of the emitted document
(serde struct-field order), so two exports of the same logical state are
byte-identical.

```json
{
  "weave_session_export": 1,
  "schema_version": 1,
  "identity": "alice",
  "exported_at": 1781410734,
  "messages": [ ... ],
  "asks": [ ... ],
  "memory": [ ... ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `weave_session_export` | u32 | **Format magic.** Must equal `1` on import; any other value is rejected ("not a weave session export"). |
| `schema_version` | u32 | **Envelope schema version.** Import accepts `<= ` the build's max (currently `1`) and **ignores unknown fields** (additive `#[serde(default)]` tolerance); a *higher* version is rejected (forward-compat guard — we will not silently drop data we cannot model). |
| `identity` | string | The exported identity (advisory provenance). Import remaps it via `--as`. |
| `exported_at` | i64 | UNIX-seconds wall clock at export time (advisory). |
| `messages` | array | The portable message set (the core payload). Imported via `Store::send`. |
| `asks` | array | Tracked-ask threads, **recorded for fidelity** — NOT replayed on import in this version (see *Scope boundary*). |
| `memory` | array | Mesh-memory entries (filesystem-backed scoped memory). Full round-trip. |

### `messages[]`

```json
{
  "id": 1,
  "ts": 1781410734,
  "sender": "alice",
  "recipient": "bob",
  "subject": null,
  "body": "hello",
  "idempotency_key": null,
  "trace_id": "trace_…",
  "priority": "normal"
}
```

| Field | Type | Cap / note |
|---|---|---|
| `id` | i64 | **Source** row id. Used ONLY to synthesize a dedup key for keyless messages — the importer mints a fresh local id; this value is never carried to the target store. |
| `ts` | i64 | Source timestamp (advisory; `send` re-stamps `now()` on insert). |
| `sender` | string | `check_ident`: ≤ 128 chars, non-empty, no control chars. |
| `recipient` | string | `check_ident` (same caps). |
| `subject` | string? | ≤ 4096 bytes on import. |
| `body` | string | ≤ `MAX_BODY` (65536 bytes). |
| `idempotency_key` | string? | Validated to the idempotency-key shape if present. |
| `trace_id` | string? | Validated to the trace-id shape if present. |
| `priority` | string? | Advisory; not re-applied on import in this version. |

### `asks[]`

`id`, `question_msg_id`, `answer_msg_id?`, `asker`, `askee`, `subject?`, `state`
(`open`/`answered`/`acked`), `opened_ts`, `updated_ts`, `closed_ts?`. Recorded for
fidelity/inspection; **not replayed** on import (see below).

### `memory[]`

```json
{
  "scope_kind": "global",
  "scope_name": "",
  "key": "patterns",
  "title": "Patterns",
  "tags": ["rust"],
  "body": "Always use types."
}
```

| Field | Note |
|---|---|
| `scope_kind` | one of `global` / `project` / `persona` / `orchestrator`. An unknown kind is a hard import error. |
| `scope_name` | sub-scope (empty for `global`). |
| `key` / `title` / `tags` / `body` | re-bounded by `memory_write` on import (key sanitized, body ≤ 64 KiB). |

The format embeds **no filesystem path fields** — only the `(scope_kind, scope_name,
key)` triple, from which the importer reconstructs the scoped path under its own
config dir. A crafted document therefore cannot direct a write outside the memory
store.

---

## Import semantics

- **Id remap is free.** Each message is re-inserted via `Store::send`, which mints a
  fresh local autoincrement id. Source ids never collide with the target's.
- **Idempotent re-import.** `send` dedups on `idempotency_key`. A message that carried
  a source key reuses it; a **keyless** legacy message gets a deterministic synthetic
  key `wl040:<source_identity>:<source_id>` (the source identity sanitized to
  `[A-Za-z0-9_]`), so importing the same document twice is a no-op for already-present
  rows.
- **Identity remap (`--as`).** Messages are inserted under the importing identity.
  Occurrences of the **source** identity in `sender`/`recipient` are rewritten to the
  importing identity; every **third-party** name is preserved verbatim. When `--as`
  equals the source identity it is an identity-preserving import (the common
  cross-machine resume case).
- **`--dry-run`** parses + validates + reports counts, and writes nothing.
- **Memory** entries are written via `memory_write`, which preserves an existing
  entry's `created_ts` (idempotent overwrite).

### Untrusted input

An import file is treated like a network payload. Before *any* store write, every
field is bounded: `check_ident` on the importing identity and every per-message
sender/recipient, `check_body`/`MAX_BODY` on bodies, the subject cap, and the
idempotency/trace-id shape checks. All writes go through the parameterized
`Store::send` (no SQL is ever interpolated). No external program is spawned (no
shell). `--in`/`--out` are UTF-8- and existence/overwrite/parent-guarded.

---

## Scope boundary (v1)

- **Messages — imported.** Full round-trip.
- **Memory — imported.** Full round-trip across all scopes.
- **Asks — recorded, not replayed.** The `asks` block is written for fidelity, but
  import does not drive the ask lifecycle (that needs a new dual-backend
  `Store::import_ask` accepting an out-of-order `AskState` — a distinct cohesive
  change tracked as **WL-040b**). Import reports `N asks in archive not imported —
  see WL-040b`.
- **Peers — excluded by design.** A peer row is host/mux/pane-local liveness state
  (`mux`, `target`, `socket`, `pid`, `host`, `birth_cert`) that is meaningless — and
  a takeover hazard — in another instance.

Schema growth is additive: a future `memory`/`asks` extension, or a new block, bumps
`schema_version` without breaking older importers (which ignore unknown fields).

---

## Worked round-trip

```bash
# instance A
weave session export --out /tmp/alice.json --for alice

# move /tmp/alice.json to instance B, then:
weave session import --in /tmp/alice.json --as alice          # resume under same name
weave session import --in /tmp/alice.json --as alice --dry-run # inspect counts only
weave session import --in /tmp/alice.json --as alice2         # resume under a new name
```

Importing `/tmp/alice.json` twice into instance B inserts each message exactly once.
