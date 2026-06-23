# FORMAT — Canonical session export (WL-040)

The interchange contract for `weave session export` / `weave session import`. This
is a **logical, portable, schema-versioned JSON document** for resuming a session
across distinct weave instances (`cross_agent_session_resumer` / casr parity).

It is one of **three** distinct weave "export" surfaces — do not confuse them:

| Surface | Command | Form | Scope | Portable? |
|---|---|---|---|---|
| **WL-034/WL-074** | `weave export` | HTML | presentation (offline mailbox view; per-identity by default, explicit `--all` for whole local store) | viewer-only |
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
  "ask_groups": [ ... ],
  "memory": [ ... ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `weave_session_export` | u32 | **Format magic.** Must equal `1` on import; any other value is rejected ("not a weave session export"). |
| `schema_version` | u32 | **Envelope schema version.** Import accepts `<= ` the build's max (currently `1`) and **ignores unknown fields** (additive `#[serde(default)]` tolerance); a *higher* version is rejected (forward-compat guard — we will not silently drop data we cannot model). The WL-040b ask fields + `ask_groups` block are **additive** (each `#[serde(default)]`), so they did **not** bump `schema_version` — an older weave ignores them. |
| `identity` | string | The exported identity (advisory provenance). Import remaps it via `--as`. |
| `exported_at` | i64 | UNIX-seconds wall clock at export time (advisory). |
| `messages` | array | The portable message set (the core payload). Imported via `Store::send`. |
| `asks` | array | Tracked-ask threads, **replayed faithfully** on import via `Store::import_ask` (WL-040b) — each ask is re-materialized in its exported `AskState` with its message links remapped (see *Asks — replayed* below). |
| `ask_groups` | array | Ask-many PARENT anchor rows (broadcast-ask groups). Replayed via `Store::import_ask_group` **before** the child asks that reference them, so `parent_id` linkage survives (WL-040b). |
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

```json
{
  "id": "ask_12_3",
  "question_msg_id": 12,
  "answer_msg_id": 14,
  "asker": "alice",
  "askee": "bob",
  "subject": "deploy?",
  "state": "acked",
  "kind": "free_text",
  "options": null,
  "reply_to": null,
  "close_note": "done",
  "opened_ts": 1781410700,
  "updated_ts": 1781410800,
  "closed_ts": 1781410800,
  "parent_id": null
}
```

| Field | Type | Cap / note |
|---|---|---|
| `id` | string | **Source** correlation id (`ask_<rowid>_<nonce>`). Advisory only — the importer **regenerates** a fresh local ask id from the remapped question id (the source id is meaningless across instances). Still shape-validated (`ask_id_valid`) so a hostile value fails loudly. |
| `question_msg_id` | i64 | **Source** message id; **remapped** on import to the freshly re-minted local question message. An ask whose question is absent from `messages[]` is **dangling** → skipped (counted, never a broken link). |
| `answer_msg_id` | i64? | **Source** message id; remapped on import. An ask claiming an answer whose message is missing is treated as dangling and skipped. |
| `asker` / `askee` | string | `check_ident` (≤ 128 chars, no control chars). `--as`-remapped consistently with messages. |
| `subject` | string? | ≤ 4096 bytes on import. |
| `state` | string | `open` / `answered` / `acked`. Must parse to the `AskState` enum — an unknown state is **rejected** before any write. The ask is materialized **directly** in this state (out-of-order, bypassing the create→answer→ack lifecycle). |
| `kind` | string | `free_text` (default) / `choice` / `tool_permission`. *Additive* WL-040b field (`#[serde(default)]`). |
| `options` | string? | Kind payload (newline-separated choices, or `tool_name\ntool_args`). ≤ `MAX_BODY`. *Additive* WL-040b field. |
| `reply_to` | string? | Source chain link. **Dropped to NULL on import** — the chain references a *source* ask id that is regenerated, so rewriting it would dangle; the thread itself replays faithfully, only the cross-ask chain pointer is lost. *Additive* WL-040b field. |
| `close_note` | string? | `ack` closing note. ≤ `MAX_BODY`. *Additive* WL-040b field. |
| `opened_ts` / `updated_ts` / `closed_ts?` | i64 | Carried verbatim (source timestamps are authoritative for a replayed thread). |
| `parent_id` | string? | Source ask-many group id (`askm_<...>`). **Remapped** on import to the replayed `ask_groups` anchor; a parent that was not in the export drops to NULL so the ask still replays standalone. *Additive* WL-040b field. |

### `ask_groups[]`

```json
{
  "parent_id": "askm_500_91",
  "asker": "alice",
  "subject": "poll",
  "body": "yes or no?",
  "opened_ts": 1781410500,
  "target_count": 2
}
```

The PARENT anchor of a broadcast-ask (ask-many) group. Export carries only the
groups referenced by the exported child asks' `parent_id`s. On import each group is
replayed via `Store::import_ask_group` with a **freshly minted** local `parent_id`
**before** its child asks, and the children's `parent_id` is rewired to it.
`target_count` is preserved verbatim — totality (`answered+acked+pending+failed ==
target_count`) still holds, and any dangling-skipped child simply counts as `failed`
on the target (faithful: that child genuinely could not be reconstructed).

| Field | Type | Cap / note |
|---|---|---|
| `parent_id` | string | Source `askm_<...>` id; shape-validated (`ask_many_id_valid`), regenerated on import. |
| `asker` | string | `check_ident`; `--as`-remapped. |
| `subject` | string? | ≤ 4096 bytes on import. |
| `body` | string | ≤ `MAX_BODY`. |
| `opened_ts` | i64 | Carried verbatim. |
| `target_count` | i64 | The post-dedup requested fanout count; preserved for totality. |

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
- **Asks + groups replayed (WL-040b).** After messages, the importer replays the
  ask-many `ask_groups` anchors (fresh local `parent_id`), then each ask via
  `Store::import_ask`: it resolves the ask's **remapped** question/answer message ids
  from the message-import pass, materializes the row **directly** in its exported
  `AskState` (no lifecycle replay — the message rows already exist), and rewires
  `parent_id` to the replayed group. `Store::send` returns the existing local id on a
  dedup hit, so the message remap is correct whether a message was newly inserted or
  already present.
  - **Dangling ask → skipped.** An ask whose question (or claimed answer) message is
    absent from `messages[]` cannot be faithfully linked, so it is **skipped** with a
    counted warning — never an inserted broken link.
  - **Idempotent.** Replay dedups on the remapped `(asker, askee, question_msg_id)`
    triple (the source ask id is meaningless across instances); groups dedup on the
    minted `parent_id`. A second import replays 0 new asks/groups. The summary reports
    `N ask(s) replayed, M skipped (already present)[, K dangling skipped]; G ask
    group(s) replayed, …`.
- **`--dry-run`** parses + validates + reports counts (messages, would-replay asks
  excluding danglers, groups, memory) and writes nothing.
- **Memory** entries are written via `memory_write`, which preserves an existing
  entry's `created_ts` (idempotent overwrite).

### Untrusted input

An import file is treated like a network payload. Before *any* store write, every
field is bounded: `check_ident` on the importing identity and every per-message
sender/recipient and per-ask asker/askee, `check_body`/`MAX_BODY` on bodies, the
subject cap, the idempotency/trace-id shape checks, ask `state`/`kind` parsed to
their enums (unknown `state` rejected), and `ask_id_valid`/`ask_many_id_valid` on
ask/parent ids. `import_ask` / `import_ask_group` **re-validate** at the store seam
(defense-in-depth). All writes go through parameterized SQL (`params!` /
`params(vec![...])` — no SQL is ever interpolated). No external program is spawned
(no shell). `--in`/`--out` are UTF-8- and existence/overwrite/parent-guarded.

---

## Scope boundary

- **Messages — imported.** Full round-trip.
- **Memory — imported.** Full round-trip across all scopes.
- **Asks — replayed (WL-040b).** Each tracked ask thread is materialized in its
  exported `AskState` (open / answered / acked) with its message links remapped to the
  freshly minted local ids, via the dual-backend `Store::import_ask`. Standalone asks,
  answered+acked threads, and broadcast-ask **groups** (`ask_groups` + child
  `parent_id`) all round-trip. The only intentional fidelity loss is the `reply_to`
  chain pointer (NULLed — it references a regenerated source ask id), and dangling asks
  (message absent) are skipped+counted.
- **Peers — excluded by design.** A peer row is host/mux/pane-local liveness state
  (`mux`, `target`, `socket`, `pid`, `host`, `birth_cert`) that is meaningless — and
  a takeover hazard — in another instance.

Schema growth is additive: the WL-040b ask fields + `ask_groups` block were added with
`#[serde(default)]` and **did not** bump `schema_version` (an older weave ignores
them; an older export missing them defaults cleanly). A future block that changes the
envelope shape would bump `schema_version` without breaking older importers.

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
