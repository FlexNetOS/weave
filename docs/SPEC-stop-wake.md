# v0.2 spec — No-poll stop-boundary wake

> Roadmap differentiator #1. Spec produced by a design workflow + critiqued against
> the live Claude hooks docs. Read alongside [ROADMAP-v0.2.md](ROADMAP-v0.2.md).

## Goal
A blocking `Stop`/`SubagentStop` hook that, when a peer has an **unread** message for
this session, emits Claude hook JSON that (a) blocks the stop and (b) feeds the
message to the model — so a peer's message **drives the next turn** instead of the
agent idling. When nothing is unread, stay silent and let Claude stop.

## Invariants (all preserved — additive)
- New hook event `weave hook wake`. The existing `stop` branch is **untouched** (still
  peeks, never marks read). No-daemon local path unchanged. No new crates / tokio.
- Trait stays **sync**: one new sync `Store` method `peek_oldest_unread(me)` with a
  **default impl** in terms of existing `inbox`, so no backend is forced to change.
- **No infinite loop**: a `wake_acks(reader, last_acked_id)` watermark guarantees each
  message wakes at most once; the wake decision is gated on `oldest_unread.id > last_acked`.

## The Claude hook contract (verified against current docs)
On `Stop`/`SubagentStop`, emit a JSON object on stdout (exit 0):
- `"decision": "block"` — the only way a Stop hook prevents idle. Required to wake.
- `"reason": "<text>"` — **shown to Claude as a system reminder so it reacts and
  continues** (verified verbatim in the Stop decision-control docs). weave packs the
  unread message(s) here, wrapped as untrusted data (mirror the broker's banner).
- Never set `"continue": false` (that hard-stops Claude regardless of `decision`).
- `"suppressOutput": true` to keep weave's bookkeeping out of the transcript.
- Note: `additionalContext`/`hookSpecificOutput` is the **UserPromptSubmit/SessionStart**
  shape, NOT the Stop shape — do not use it here.

## ⚠️ Correction the critique caught (must fix before building)
The draft keyed its primary loop-guard on `payload.stop_hook_active == true`. **That
field is NOT in the current documented Stop input** (`session_id, transcript_path,
cwd, permission_mode, …`). Do **not** rely on it. Use the `wake_acks` watermark as the
sole loop guard: after a wake blocks with message #N's content, record `last_acked = N`
for this reader; the next `wake` only blocks if a newer unread (`id > last_acked`)
exists. This makes re-entry safe without the missing field.

## Implementation checklist
1. `store`: add `wake_acks(reader TEXT PRIMARY KEY, last_acked_id INTEGER)` to both
   schemas (additive migration like `in_reply_to`); add `peek_oldest_unread(me)` (default
   impl via `inbox(me, false, false, 1)`) and `set_wake_ack(me, id)`.
2. `main.rs`: `weave hook wake` → resolve identity (explicit only; never wake under a
   guessed identity); if `peek_oldest_unread` returns a row with `id > last_acked`, print
   the `decision:block` JSON with the untrusted-wrapped body and set the ack; else silent.
3. `setup.rs`: register `Stop`/`SubagentStop` → `weave hook wake` (merge, idempotent,
   alongside the existing drain hooks).
4. Tests: a `wake` integration test (unread → block JSON once; second wake silent;
   new message → blocks again).
