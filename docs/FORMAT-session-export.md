# FORMAT — Canonical session export (WL-040)

The interchange contract for `weave session export` / `weave session import`. This
is a **logical, portable, schema-versioned JSON document** for resuming a session
across distinct weave instances (`cross_agent_session_resumer` / casr parity).

It is one of **three** distinct weave "export" surfaces — do not confuse them:

| Surface | Command | Form | Scope | Portable? |
|---|---|---|---|---|
| **WL-034/WL-074** | `weave export` | HTML | presentation (offline mailbox view; per-identity by default, explicit `--all` for whole local store) | viewer-only |
| **WL-035** | `weave backup` / `restore` | USTAR tar (binary DB snapshot) | byte-exact, **host-local** | no (ids/host identical) |
| **WL-040** | `weave session export` / `import` | **canonical JSON** | logical session (messages + asks/groups + memory) | **yes** (ids re-minted on import) |

The one-line rule: **WL-040 is logical + portable + versioned; WL-035 is byte-exact
+ host-local.**

---

## The envelope

A single JSON object. Field order is the stable key order of the emitted document
(serde struct-field order), so serializing the same envelope is byte-identical.
Separate export commands normally have different `exported_at` values.

```json
{
  "weave_session_export": 1,
  "schema_version": 2,
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
| `schema_version` | u32 | **Envelope schema version.** Import accepts `<=` the build's max (currently `2`) and **ignores unknown fields** (additive `#[serde(default)]` tolerance); a higher version is rejected (forward-compat guard — Weave will not silently drop semantics it cannot model). Schema v2 adds message successor state plus the replay-critical configured-send tuple. |
| `identity` | string | The validated source identity. Import remaps it via `--as` and hashes the exact value into synthesized message/ask/group ids; changing it changes replay identity. |
| `exported_at` | i64 | UNIX-seconds wall clock at export time (advisory). |
| `messages` | array | The portable message set (the core payload). Imported through the keyed plain/configured send seams, then linked in a second pass. |
| `asks` | array | Tracked-ask threads, **replayed faithfully** on import via `Store::import_ask` (WL-040b) — each ask is re-materialized in its exported `AskState` with its message links remapped (see *Asks — replayed* below). |
| `ask_groups` | array | Ask-many PARENT anchor rows (broadcast-ask groups). Replayed via `Store::import_ask_group` **before** the child asks that reference them, so `parent_id` linkage survives (WL-040b). |
| `memory` | array | Mesh-memory entries (filesystem-backed scoped memory). Full round-trip. |

### File-size and publication boundary

The complete UTF-8 JSON document is capped at **256 MiB (268,435,456 bytes)** in
both directions. Import rejects an oversized file before UTF-8 decoding or JSON
parsing, using both a metadata precheck and an authoritative bounded read so a file
that grows during the read cannot escape the cap. Export validates the serialized
byte count before creating or replacing the destination; an oversized snapshot
fails with guidance to reduce `--limit` and leaves the destination untouched.

Each top-level repeated block is also capped at **10,000 entries** (`messages`,
`asks`, `ask_groups`, and `memory`), and each memory entry is capped at 16 tags.
Import enforces those limits while serde is walking each untrusted array, before it
can allocate an unbounded collection; export applies the same limits so every
produced document remains importable.

Export publishes through an exclusively created sibling temporary file rather than
writing the destination in place. On Unix that temporary inode is explicitly mode
`0600`; its bytes are flushed with `sync_all`, then published atomically. A normal
export uses an atomic no-clobber hard-link publication so a destination created by a
concurrent process is never overwritten. `--force` uses an atomic rename. The
parent directory is synced on Unix, and every failure path removes the temporary
name. Existing files or symlinks at guessed temporary names are never followed.

### `messages[]`

```json
{
  "id": 1,
  "ts": 1781410734,
  "sender": "alice",
  "recipient": "bob",
  "subject": null,
  "body": "hello",
  "in_reply_to": null,
  "reply_ttl": 0,
  "idempotency_key": null,
  "trace_id": "trace_…",
  "priority": "urgent",
  "superseded_by": 2,
  "configured_send": {
    "priority": "normal",
    "ttl": 600,
    "supersedes": null,
    "dedup_idle": true
  }
}
```

| Field | Type | Cap / note |
|---|---|---|
| `id` | i64 | **Source** row id. Must be positive and unique in the document. It seeds a deterministic key for a keyless message and resolves predecessor/successor/message links; the importer mints a fresh local id, so this value is never carried to the target store. |
| `ts` | i64 | Source timestamp (advisory; `send` re-stamps `now()` on insert). |
| `sender` | string | `check_ident`: ≤ 128 chars, non-empty, no control chars. |
| `recipient` | string | `check_ident` (same caps). |
| `subject` | string? | ≤ 256 Unicode scalars and no control characters (`check_subject`). |
| `body` | string | ≤ `MAX_BODY` (65536 bytes). |
| `in_reply_to` | i64? | Source id of this message's direct parent. The parent must be present, earlier, and imply this reply's exact recipient and `Re: ` subject. Import remaps the parent id and creates the reply atomically. A top-level message uses `null`. |
| `reply_ttl` | i64 | Relative TTL requested for a reply (`0` means permanent, otherwise `1..=86400`). It must be `0` when `in_reply_to` is `null`; a reply cannot also carry `configured_send`. |
| `idempotency_key` | string? | Validated to the idempotency-key shape if present. |
| `trace_id` | string? | Validated to the trace-id shape if present. |
| `priority` | string? | Effective stored priority: exactly `low`, `normal`, `high`, or `urgent`. It is restored on a newly inserted target row; absent schema-v1 values default to `normal`. An existing-key replay must already have this effective priority. |
| `superseded_by` | i64? | Effective successor link in the exported snapshot. The target link is restored only after all message ids are remapped. The successor must be present, have a later source id, and have the same sender **and recipient**. |
| `configured_send` | object? | Schema-v2 replay tuple for a configured top-level send. It is absent for schema-v1 documents and for tracked ask/answer rows and replies, which are reconstructed by their own operation. See below. |

### `messages[].configured_send`

`configured_send` records the caller-visible request tuple that must remain stable
across an exact idempotent retry:

| Field | Type | Meaning |
|---|---|---|
| `priority` | string | Canonical requested priority (`low` / `normal` / `high` / `urgent`). This is the request value; `messages[].priority` remains the effective stored value. |
| `ttl` | i64 | Relative requested TTL. `0` means permanent; otherwise it must be within the normal message TTL range (`1..=86400`). |
| `supersedes` | i64? | Source id of the explicitly requested predecessor. It must be present in the document, earlier than this message, and on the same sender+recipient route. |
| `dedup_idle` | bool | Whether the original configured send requested idle-ping collapse. It cannot be combined with `supersedes`. |

The distinction between the configured request tuple and effective snapshot state is
intentional. Import calls the configured-send seam with both the original tuple and
the exported effective priority in one transaction, but with broad dedup side effects
disabled; it then restores only the exported `superseded_by` links. Replies likewise
persist their parent, effective priority, key, and relative TTL atomically. This
preserves future exact-retry behavior without a crash window or sweeping unrelated
idle rows already present in the target store.

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
| `id` | string | **Source** correlation id (`ask_<rowid>_<nonce>`). The importer normally derives a stable `ask_imp_<32-hex>` target id from the exact `(source identity, source ask id)` pair; if the remapped question is already attached to an ask from an earlier partial import, that existing target id is adopted instead. The source id is still shape-validated (`ask_id_valid`) so hostile input fails loudly. |
| `question_msg_id` | i64 | **Source** message id; **remapped** on import to the freshly re-minted local question message. An ask whose question is absent from `messages[]` is **dangling** → skipped (counted, never a broken link). |
| `answer_msg_id` | i64? | **Source** message id; remapped on import. An ask claiming an answer whose message is missing is treated as dangling and skipped. |
| `asker` / `askee` | string | `check_ident` (≤ 128 chars, no control chars). `--as`-remapped consistently with messages. |
| `subject` | string? | ≤ 256 Unicode scalars and no control characters (`check_subject`). |
| `state` | string | `open` / `answered` / `acked`. Must parse to the `AskState` enum — an unknown state is **rejected** before any write. The ask is materialized **directly** in this state (out-of-order, bypassing the create→answer→ack lifecycle). |
| `kind` | string | `free_text` / `choice` / `tool_permission`. A missing additive field defaults to `free_text` for older envelopes; any present noncanonical value is rejected before a write. *Additive* WL-040b field (`#[serde(default)]`). |
| `options` | string? | Kind payload (newline-separated choices, or `tool_name\ntool_args`). ≤ `MAX_BODY`. *Additive* WL-040b field. |
| `reply_to` | string? | Source ask-chain link. The referenced ask must be present in the document. Import precomputes stable target ask ids and remaps the link, so chain order is irrelevant and the pointer survives. *Additive* WL-040b field. |
| `close_note` | string? | `ack` closing note. ≤ `MAX_BODY`. *Additive* WL-040b field. |
| `opened_ts` / `updated_ts` / `closed_ts?` | i64 | Carried verbatim (source timestamps are authoritative for a replayed thread). |
| `parent_id` | string? | Source ask-many group id (`askm_<...>`). **Remapped** on import to the replayed `ask_groups` anchor. Every reconstructable child must reference a group present in `ask_groups[]`; a missing group or incoherent child/group payload is rejected before any write. *Additive* WL-040b field. |

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
replayed via `Store::import_ask_group` with a deterministic local `parent_id`
**before** its child asks, and the children's `parent_id` is rewired to it. The
local id is `askm_imp_<32-hex>`, derived from two independently seeded FNV-1a lanes
over the exact `(source identity, source parent_id)` pair. It is therefore stable
across processes, clocks, and delayed re-imports.
`target_count` is preserved within the normal fanout bound (`1..=64`) — totality
(`answered+acked+pending+failed == target_count`) still holds, and any
dangling-skipped child simply counts as `failed` on the target (faithful: that child
genuinely could not be reconstructed).

| Field | Type | Cap / note |
|---|---|---|
| `parent_id` | string | Source `askm_<...>` id; shape-validated (`ask_many_id_valid`). Source parent ids must be unique in the document. The target id is deterministically derived, not copied. |
| `asker` | string | `check_ident`; `--as`-remapped. |
| `subject` | string? | ≤ 256 Unicode scalars and no control characters (`check_subject`). |
| `body` | string | ≤ `MAX_BODY`. |
| `opened_ts` | i64 | Carried verbatim. |
| `target_count` | i64 | The post-dedup requested fanout count (`1..=64`); preserved for totality. |

If a deterministic target group id already exists, the Store compares the complete
group payload (`asker`, subject shape/value, body, `opened_ts`, and `target_count`).
An exact match is an idempotent replay; any mismatch is a collision error. A second
import can therefore neither create an orphan duplicate group nor silently attach
children to a different group.

Export also closes each represented group over its live children. If `--limit`
would include a group while omitting a child whose required messages still exist,
export fails instead of producing a partial group. Metadata for a child whose
required message was genuinely removed by retention does not block export; that
unreconstructable child is skipped and remains part of the preserved failed count.

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
| `scope_name` | Valid named sub-scope for project/persona/orchestrator; must be empty for `global`. Values that would be sanitized or truncated are rejected. |
| `key` / `title` / `tags` / `body` | Strictly preflighted before any DB write: no lossy key/title/tag normalization, bounded tag count/length, body ≤ 64 KiB. `memory_write` re-validates at the filesystem seam. |

The format embeds **no filesystem path fields** — only the `(scope_kind, scope_name,
key)` triple, from which the importer reconstructs the scoped path under its own
config dir. A crafted document therefore cannot direct a write outside the memory
store. That triple must also be unique within the document; duplicates are rejected
instead of making array order decide which value wins.

---

## Import semantics

- **The source identity is part of the contract.** `identity` is validated with
  `check_ident` before any write. It participates in identity remapping and in every
  synthesized message/ask/group identifier; changing it describes a different source
  document, not a retry of the old one.
- **Id remap is free.** Messages are ordered by positive, unique source id and
  inserted through the keyed plain/configured send seams, which mint fresh local
  autoincrement ids. Source ids never collide with the target's. The source→target
  id map then resolves asks, configured predecessors, and effective successors.
- **Idempotent re-import.** A message that carried a source `idempotency_key` reuses
  it. A **keyless** message gets
  `wl040:<32-hex-identity-hash>:<source_id>`, where the complete source identity is
  hashed through two independently seeded FNV-1a lanes. The identity is neither
  sanitized nor truncated, so punctuation or a long shared prefix cannot alias.
  Effective keys must be unique within the document. An existing key is accepted
  only for the same route/content and effective priority; a semantic mismatch is a
  collision error. This synthesized namespace identifies one source store only by
  `(identity, source row id)`: importing two independently created stores that use
  the same identity and overlapping keyless row ids into one target is deliberately
  unsupported. Weave detects a semantic collision and fails with an actionable
  `source namespace conflict`; import those sources into separate targets or use
  stable source idempotency keys.
- **Identity remap (`--as`).** Messages are inserted under the importing identity.
  Occurrences of the **source** identity in `sender`/`recipient` are rewritten to the
  importing identity; every **third-party** name is preserved verbatim. When `--as`
  equals the source identity it is an identity-preserving import (the common
  cross-machine resume case). Choose the mapping per target: importing the same
  source document later under a different `--as` reuses the source-derived
  keys/group ids but changes route payload, so it is rejected as a collision rather
  than silently duplicating the session. Preflight also rejects a remap target that
  is already used by a distinct sender, recipient, asker, askee, or group asker in
  the source document; two source actors can never be silently collapsed into one
  target identity.
- **Schema-v2 configured semantics are restored.** A `configured_send` message is
  imported with its exact canonical request tuple: requested priority, relative TTL,
  optional predecessor, and `dedup_idle`. The separately exported effective
  `priority` is restored on a newly inserted row. Requested predecessor and effective
  successor links must be closed over the export, ordered (predecessor earlier,
  successor later), and remain on one sender+recipient route. Export fails if its
  `--limit` omits a required predecessor or successor; import rejects a crafted
  broken/cross-route graph before writing.
- **Schema-v2 reply semantics are restored.** A reply's parent is closed over the
  export, remapped before the child, and validated against the parent-derived route
  and subject. Parent link, effective priority, key, and relative TTL are inserted
  atomically. If `--limit` omits an existing parent, export fails; if retention has
  genuinely deleted the parent, export normalizes the surviving child to a portable
  top-level configured send while preserving content, effective priority, and TTL.
- **TTL is relative at the portability boundary.** On the first target insertion, a
  nonzero configured TTL is re-stamped from the target's current time, granting the
  full exported duration rather than copying a stale source deadline. Re-import finds
  the existing keyed row and does **not** extend that deadline.
- **Only exported successor state is applied.** Import disables configured
  `dedup_idle`'s broad target-local sweep, then restores the source snapshot's
  `superseded_by` links after every message has been remapped. Unrelated target rows
  are never collapsed as a side effect of rehydration.
- **Trace is attempt-local.** `trace_id` is carried for a newly created message but is
  excluded from exact-retry identity. On re-import, the first accepted row's trace
  remains authoritative; a later trace neither collides nor overwrites it.
- **Asks + groups replayed (WL-040b).** After messages, the importer replays the
  ask-many `ask_groups` anchors (deterministic local `parent_id`), then each ask via
  `Store::import_ask`: it resolves the ask's **remapped** question/answer message ids
  from the message-import pass, materializes the row **directly** in its exported
  `AskState` (no lifecycle replay — the message rows already exist), and rewires
  `parent_id` to the replayed group. The keyed message seams return the existing
  local id on a dedup hit, so the message remap is correct whether a message was
  newly inserted or already present.
  `reply_to` is remapped through a precomputed source-ask→target-ask table; a target
  already created by an earlier partial import is adopted by its remapped question
  message, so delayed retries cannot create a dangling chain.
  - **Dangling ask → skipped.** An ask whose question (or claimed answer) message is
    absent from `messages[]` cannot be faithfully linked, so it is **skipped** with a
    counted warning — never an inserted broken link.
  - **Idempotent.** Replay dedups on the remapped `(asker, askee, question_msg_id)`
    triple (the source ask id is never copied as a local row id); groups dedup on their
    deterministic target id and require exact payload equality. A second import
    replays 0 new asks/groups. The summary reports
    `N ask(s) replayed, M skipped (already present)[, K dangling skipped]; G ask
    group(s) replayed, …`.
- **`--dry-run`** parses + validates + reports counts (messages, would-replay asks
  excluding danglers, groups, memory) and writes nothing.
- **Memory** entries are written via `memory_write`, which preserves an existing
  entry's `created_ts` (idempotent overwrite). An existing key remains writable when
  its scope is at the file-count cap; only creation of an additional key is refused.

### Schema-v1 fallback

Schema v1 remains importable. Its missing `superseded_by`, `configured_send`,
`in_reply_to`, and `reply_ttl` fields default to `null`/`0`, so those messages use
the plain keyed-send fallback:

- their effective exported `priority` is restored (`normal` when omitted);
- the effective priority and a private portable-session provenance marker are
  inserted atomically. The marker is not exported, but it prevents a plain import
  from being mistaken for a locally configured send with the same visible payload
  and idempotency key after the database is reopened;
- no reply parent, reply TTL, configured TTL, explicit predecessor request,
  idle-dedup request, or effective successor graph can be reconstructed because v1
  did not record those semantics;
- the same deterministic hashed key is synthesized for a keyless row; and
- exact re-import still compares route/content/effective priority while excluding
  attempt-local trace metadata.

The older additive ask fields and `ask_groups` block also continue to default
cleanly when absent. A future export with `schema_version > 2` is rejected rather
than imported with unknown semantics.

### Untrusted input

An import file is treated like a network payload. Before *any* Store write, every
store-bound message/ask/group field is bounded and every cross-reference is checked:
the 256 MiB document cap; parser-time 10,000-entry collection caps; `check_ident` on
source and importing identities plus every
sender/recipient/asker/askee; unique positive message ids and unique effective keys;
`check_body`/`MAX_BODY`; `check_subject`; idempotency/trace-id shapes; canonical
priorities and TTL; configured tuple compatibility; reply parent/route/subject
closure; same-sender+recipient predecessor/successor closure; ask `state` / `kind`;
bounded ask options and group target counts; `ask_id_valid` / `ask_many_id_valid`;
unique source group ids; and a remap that cannot collapse distinct actors. Every
memory scope/key/title/tag/body field is strictly preflighted, including tag-count
and unique `(scope_kind, scope_name, key)` checks, so no later sanitization or
overwrite ordering can make the round-trip lossy; `memory_write` then revalidates at
the filesystem seam.
`import_ask` / `import_ask_group` **re-validate** at the store seam
(defense-in-depth), including exact collision checks for a deterministic group id.
All writes go through parameterized SQL (`params!` / `params(vec![...])` — no SQL
is ever interpolated). No external program is spawned (no shell). `--in`/`--out`
are UTF-8- and existence/overwrite/parent-guarded.

---

## Scope boundary

- **Messages — semantically imported.** Payload, reply parent/TTL, effective
  priority, schema-v2 configured request tuple, and effective successor graph
  round-trip. Local message ids and message timestamps are re-minted; ask/group
  timestamps remain source-authoritative, a relative TTL is re-stamped, and the
  first accepted trace remains authoritative.
- **Memory — imported.** Full round-trip across all scopes.
- **Asks — replayed (WL-040b).** Each tracked ask thread is materialized in its
  exported `AskState` (open / answered / acked) with its message links remapped to the
  freshly minted local ids, via the dual-backend `Store::import_ask`. Standalone asks,
  answered+acked threads, and broadcast-ask **groups** (`ask_groups` + child
  `parent_id`) and cross-ask `reply_to` chains all round-trip. Retention-dangling asks
  (whose message was genuinely deleted) are skipped; an existing ask or message
  omitted only by `--limit` is a closure error rather than silent fidelity loss.
- **Peers — excluded by design.** A peer row is host/mux/pane-local liveness state
  (`mux`, `target`, `socket`, `pid`, `host`, `birth_cert`) that is meaningless — and
  a takeover hazard — in another instance.

The WL-040b ask fields + `ask_groups` block remain additive `#[serde(default)]`
fields. Schema v2 deliberately bumps the version for the replay-semantic message
fields (`superseded_by` and `configured_send`), while those fields still default
cleanly when a v1 document is read. Importers ignore unknown JSON fields but reject
an envelope whose declared schema version is newer than they understand.

---

## Worked round-trip

```bash
# instance A
weave session export --out /tmp/alice.json --for alice

# move /tmp/alice.json to instance B, inspect it, then choose ONE target mapping:
weave session import --in /tmp/alice.json --as alice --dry-run # inspect counts only
weave session import --in /tmp/alice.json --as alice          # preserve the name
# OR, on a target where this source document has not already been imported:
weave session import --in /tmp/alice.json --as alice2         # remap to a new name
```

Importing `/tmp/alice.json` twice into instance B inserts each message exactly once,
does not extend an existing TTL, and does not create a second ask-group anchor.
