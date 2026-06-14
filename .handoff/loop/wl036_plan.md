# WL-036 — Post-send hooks (atm-core parity)

## Goal
Let an operator configure external commands that weave runs *after* a message is sent
(and optionally after an ack), with the hook's `recipient` pattern matched against the
message recipient (supporting a `*` wildcard plus the existing `BROADCAST` aliases). The
hook is **config-driven** (`[[post_send_hook]]` in `config.toml`), runs an **argv-only,
no-shell** external program, receives message metadata via `WEAVE_HOOK_*` **env vars**
(never argv concatenation), is **fault-isolated** (a slow/failing/missing hook never
breaks `send`, logs only to **stderr**), and fires from a **single shared helper** used by
both CLI `weave send` and MCP `weave_send`. No `Store`/schema change, no glob crate, no
new standing MCP tool.

## Touched files
| file | layer/crate | what changes | why |
|---|---|---|---|
| `weave-core/src/config.rs` | `weave-core::config` | Add `PostSendHook` struct + `post_send_hook: Option<Vec<PostSendHook>>` field on `Config` (`#[serde(default)]`); add the pure `hook_recipient_matches(pattern, recipient)` matcher + `HookEvent` enum + the per-hook input caps; add `Config::hooks_for(event, recipient) -> Vec<&PostSendHook>`. No env overlay needed (hooks are file-only; note the rationale). | Config schema + pure matcher live in the lowest layer (no I/O); reusable by both send paths. |
| `weave-inject/src/inject.rs` | `weave-inject` | Add `pub fn run_post_send_hook(argv: &[String], env: &[(String,String)], timeout) -> Result<()>` — the argv-only, trusted-program, bounded-wait **spawn helper**, reusing `resolve_trusted_program` + the `run_bounded_env` pattern. | The spawn primitive must sit in `weave-inject` so it is reachable from BOTH `weave` (bin) and `weave-mcp` without an upward dep (mcp already depends on inject; main depends on both). |
| `weave-mcp/src/mcp.rs` OR a new shared seam | see "Shared helper" | Add the **fire_post_send_hooks** orchestration helper (build env, select matching hooks, spawn each best-effort, log failures to stderr). | One source of truth invoked by both send call-sites. |
| `weave/src/main.rs` | `weave` (bin) | Call `fire_post_send_hooks(Send, ...)` at the CLI send/notify post-persist seam (inside / right after `inject_and_trace`). | CLI `weave send` must fire hooks. |
| `weave-mcp/src/mcp.rs` | `weave-mcp` | Call `fire_post_send_hooks(Send, ...)` at the end of `tool_send` (and `tool_notify`). | MCP `weave_send` must fire hooks. |
| `config.toml` example block in `weave-core/src/config.rs` doc-comment + README/OPERATIONS | docs | Add `[[post_send_hook]]` example. | Operator discoverability. |

## Shared helper — where it lives (layer DAG)
The pure matcher + config structs live in **`weave-core::config`** (no I/O). The **spawn
primitive** (`run_post_send_hook`) lives in **`weave-inject`** (it is the layer already
trusted to spawn external programs — `spawn`, `kill`, `run_bounded_env`,
`resolve_trusted_program` are all here). The **orchestration helper**
`fire_post_send_hooks(cfg, event, sender, recipient, subject, message_id)` must be callable
from both `weave/src/main.rs` and `weave-mcp/src/mcp.rs`.

DAG today: `weave-core` ← `weave-inject` ← `weave-mcp` ← `weave`(bin).
- Option A (RECOMMENDED): put `fire_post_send_hooks` in **`weave-inject`** as a free fn
  taking `&Config` + the message fields. `weave-inject` already depends on `weave-core`
  (Config) and owns the spawn primitive; both `weave-mcp` and `weave` depend on
  `weave-inject`, so it is reachable from both send paths with **no upward dep**. This is
  the single source of truth. Prefer this.
- Option B (reject): duplicating the orchestration in both `main.rs` and `mcp.rs` — two
  sources of truth, drift hazard. Do not.

Implementer note: the matcher + caps are pure in `weave-core`; the spawn + orchestration
are in `weave-inject`. `weave-mcp`/`weave` only *call* `inject::fire_post_send_hooks`.

## Dual-backend?
**No.** Hooks are pure config + runtime spawn; they touch **no** `Store` trait method, no
SQL, no schema. `weave-core/src/store_libsql.rs` is **not** touched. (Confirm in the plan
so the verifier doesn't go looking.)

## Config schema (weave-core/src/config.rs)
```rust
/// One post-send hook rule (WL-036, atm-core parity). Runs an external program
/// (argv-only, NO shell) after a matching send/ack. Message fields reach the child
/// ONLY as WEAVE_HOOK_* env vars — never concatenated into argv.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct PostSendHook {
    /// Recipient glob: `*` matches any recipient; a BROADCAST alias (all/*/everyone/
    /// broadcast) matches a broadcast send; otherwise an exact (case-sensitive) match.
    /// `None`/empty ⇒ treat as `*` (match all) per atm-core, OR require it — see Q1.
    pub recipient: Option<String>,
    /// The command to run as an explicit argv vector. argv[0] is the program (resolved
    /// to a TRUSTED absolute path); the rest are passed as DISCRETE argv elements.
    /// NEVER a shell line.
    pub argv: Vec<String>,
    /// "send" (default) or "ack". Parsed via HookEvent::parse; unknown ⇒ Send.
    #[serde(default)]
    pub event: Option<String>,
    /// Per-hook wall-clock bound (ms); clamped to [MIN, MAX]; None ⇒ default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
```
Add on `Config`:
```rust
#[serde(default)]
pub post_send_hook: Option<Vec<PostSendHook>>,
```
Caps/consts (mirror existing `MAX_*` discipline):
- `MAX_POST_SEND_HOOKS: usize` (e.g. 16) — bound the rule count (drop excess w/ stderr note).
- `MAX_HOOK_ARGV: usize` (e.g. 16) — bound argv length (reuse `inject::MAX_SPAWN_ARGS` value).
- reuse `inject::spawn_arg_ok` for per-arg validation (len + control/NUL reject).
- `HOOK_TIMEOUT_MS_DEFAULT` (e.g. 5000) + clamp via existing `[MIN_TIMEOUT_MS, MAX_TIMEOUT_MS]`.

`HookEvent` enum (`Send` | `Ack`) with `as_str`/`parse` (the `MessagePriority`/`AskRole`
precedent — TEXT, total parse, default `Send`).

**No env overlay** for hooks (a hook is an argv to spawn — exposing it through an env var
would let an ambient env inject a program to run; file-only is the safe posture; document
this explicitly as a deliberate non-parity-with-other-keys choice).

## Wildcard matcher (PURE, unit-testable — weave-core)
```rust
/// Does a hook `pattern` match a message `recipient`? PURE; total on any input.
/// Rules (single source of truth, mirrors model::BROADCAST):
///   - "*" (the universal wildcard) matches ANY recipient.
///   - a BROADCAST alias pattern (model::is_broadcast(pattern)) matches a recipient
///     that is itself a broadcast send (model::is_broadcast(recipient)).
///   - otherwise exact, case-sensitive equality.
/// NOTE: "*" is BOTH the universal wildcard AND a BROADCAST alias; we treat a literal
/// "*" pattern as the universal wildcard (superset) so it also fires on broadcasts.
pub fn hook_recipient_matches(pattern: &str, recipient: &str) -> bool
```
Keep it tiny: support exactly `*` (whole-string universal) — NOT general glob substrings
(`a*b`) unless atm-core requires it; do NOT add a glob crate. If partial globs are needed,
implement a 10-line two-pointer `*`-matcher (state the decision under Q2). The `BROADCAST`
alias is handled by reusing `model::is_broadcast` so the alias set never drifts.

## The fire points (exact call-sites)
1. **CLI `weave send`** — `weave/src/main.rs` `Cmd::Send` arm, the **local (`None`) branch**,
   after `store.send(...)` (line **2888**) and the `inject_and_trace(...)` call (line
   **2900**). Fire `inject::fire_post_send_hooks(&cfg, HookEvent::Send, &from, &to, subject, mid)`.
   (The cross-store `Some(store_path)` intent branch at 2860 may ALSO fire a send hook — Q3;
   recommend YES with recipient=`to`, since a queued intent IS a send.)
2. **CLI `weave notify`** — `weave/src/main.rs` `Cmd::Notify` arm after `store.send` (line
   **2933**) / `inject_and_trace` (2947). Notify is a point-to-point send ⇒ fire `Send` hooks.
3. **MCP `weave_send`** — `weave-mcp/src/mcp.rs` `tool_send`, after the local `store.send`
   (line **641**) and the inject block, before `Ok(out)` (line **726**). Fire `Send` hooks.
   The cross-store intent early-return (line 636) — see Q3.
4. **MCP `weave_notify`** — `tool_notify` (line **777**), same as notify.
5. **ack event** — find the `ack` handlers (CLI `Cmd::Ack` + MCP `tool_ack` / answer/ack
   path) and fire `HookEvent::Ack` there. (Implementer: locate via `grep -nF 'fn tool_ack'`
   / `Cmd::Ack` — the ack path mutates ask lifecycle; the hook fires post-state-change.)

All five go through the ONE `inject::fire_post_send_hooks` helper (no forked logic).

## How message fields reach the child (env vars — SAFEST)
The orchestration helper builds an env vector and passes it to the spawn helper via
`Command::envs` (exactly how `spawn` threads identity in `run_bounded_env`). Message-derived
values therefore reach the child as **opaque env values**, never parsed by a shell, never
concatenated into a command line:
- `WEAVE_HOOK_EVENT`   = "send" | "ack"
- `WEAVE_HOOK_SENDER`  = message sender
- `WEAVE_HOOK_RECIPIENT` = message recipient (the raw `to`, incl. a broadcast alias)
- `WEAVE_HOOK_SUBJECT` = message subject (empty string when none)
- `WEAVE_HOOK_MESSAGE_ID` = stored message id (decimal)
- (optional, atm-parity) `WEAVE_HOOK_PAYLOAD` = a small JSON object of the above, mirroring
  atm-core's `ATM_POST_SEND` JSON. Body is NOT exported by default (avoid leaking message
  bodies into child env / process listings) — gate behind an explicit hook opt-in if needed (Q4).

**Injection-safety argument.** `argv` is a FIXED operator-authored template from
`config.toml`; weave never substitutes message text INTO an argv element. The program is
`std::process::Command::new(resolve_trusted_program(argv[0]))` with `.args(&argv[1..])` —
each remaining element passed whole. Message-derived strings travel ONLY as `Command::envs`
values, which the OS delivers as the child's `environ` array with no shell evaluation. There
is no `sh -c`, no string concatenation, no interpolation. A hostile subject `; rm -rf /` or
`$(reboot)` is therefore an inert env value (or an inert whole argv element if a future
template substitutes it as a single element) — the shell metacharacters are never seen by a
shell because no shell exists on this path. argv[0] is constrained to a trusted dir
(`resolve_trusted_program`), so a hook cannot launch an arbitrary `$PATH` binary. Env values
are length/control-char bounded (reuse the ident/`spawn_arg_ok`-class caps) before being set.

## Fault isolation / non-blocking
`fire_post_send_hooks`:
- selects matching hooks via the pure matcher; for each, calls the bounded
  `inject::run_post_send_hook(argv, env, timeout)`;
- the spawn uses the **bounded-wait** pattern of `run_bounded_env` (try_wait loop + kill on
  timeout) so a slow hook cannot hang `send` past `timeout_ms`;
- EVERY error (missing trusted binary, non-zero exit, timeout, spawn failure) is caught and
  logged to **stderr** (`eprintln!` in CLI; the mcp `log()` helper which writes stderr —
  STDOUT DISCIPLINE), and **never** propagated to the send result. The send already
  succeeded (message persisted) before any hook runs.
- Decision: **bounded synchronous** (spawn + bounded wait) rather than fire-and-detach,
  matching the existing `run_bounded_env` discipline and keeping the test deterministic (a
  sentinel-file integration test needs the hook to have completed before assertion). Note a
  long `timeout_ms` blocks send for up to that long — that is the operator's tradeoff; the
  clamp bounds the worst case. (Detach is a possible future opt-in — Q5.)

## Invariants in scope
- **No-shell / argv-only** — `weave-inject/src/inject.rs::run_post_send_hook` (the single
  most dangerous edit): `Command::new` + explicit argv, `resolve_trusted_program(argv[0])`,
  message fields via `.envs` only. NEVER a command string. (ARCHITECTURE §7)
- **Input caps** — `MAX_POST_SEND_HOOKS`, `MAX_HOOK_ARGV`, `spawn_arg_ok` per element,
  env-value length/control bounds, `timeout_ms` clamp. `weave-core/src/config.rs`.
- **stdout discipline** — all hook failure logging to stderr; in `mcp.rs` use `log()` not
  `println!`. (MCP only emits JSON-RPC on stdout.)
- **token-light MCP** — NO new standing tool; hooks are invisible to `tools/list`. No change
  to `tool_catalog()`. `weave-mcp/src/mcp.rs`.
- **BROADCAST single-source-of-truth** — matcher reuses `model::is_broadcast`; no new alias
  list. `weave-core/src/model.rs` (read-only).

## Test layers required (docs/TESTING.md §8)
1. **Unit — pure matcher** (`weave-core/src/config.rs` `#[cfg(test)]`): `hook_recipient_matches`
   for: `*` matches any (`"agent-a"`, `"all"`); exact match hit + miss; a BROADCAST-alias
   pattern (`"all"`) matches a broadcast recipient (`"*"`/`"everyone"`) and does NOT match a
   named recipient; case sensitivity; empty pattern behavior (per Q1). 
2. **Unit — config parse** (`weave-core/src/config.rs`): a `[[post_send_hook]]` TOML block
   deserializes into `Config.post_send_hook`; the over-`MAX_POST_SEND_HOOKS`/over-`MAX_HOOK_ARGV`
   caps drop excess; `HookEvent::parse` totality (unknown ⇒ Send).
3. **Unit — argv has no shell** (`weave-inject` or `weave-core`): assert the constructed hook
   invocation is `[bin, arg1, ...]` with no `sh`/`-c`/concatenation; a hook whose argv[0] is
   not trusted is rejected (mirrors `spawn` rejecting an untrusted program).
4. **Integration** (`weave/tests/integration.rs`): configure (via a temp `config.toml` under a
   scrubbed `XDG_CONFIG_HOME`, or a `WEAVE_*` path) a hook whose `argv` is a trusted tiny
   program (e.g. a shell-free helper, or `/usr/bin/env`-class writer placed under
   `WEAVE_MUX_DIR`) that writes `$WEAVE_HOOK_SENDER/$WEAVE_HOOK_RECIPIENT/...` to a sentinel
   file; run the compiled `weave send`; assert the sentinel file appears with the
   env-derived content (and that a non-matching recipient does NOT fire it). Follow the
   existing integration pattern: `CARGO_BIN_EXE_weave`, scrubbed env, unique temp `WEAVE_DB`.
   (Implementer: the test "program" must be a trusted-dir binary; reuse the test harness's
   fake-mux-dir mechanism (`WEAVE_MUX_DIR`) to host a sentinel-writer script, OR invoke a
   known no-shell coreutil like `/usr/bin/touch`/`tee` and read the file — pick per the
   existing `tests/common` helpers.)
5. **Security** (`weave/tests/security.rs`): configure a hook and `weave send` with a hostile
   subject (`"; rm -rf /"`, `"$(reboot)"`, backticks) and a hostile recipient/sender; assert
   the metacharacters reach the child INERT (the sentinel file contains the literal string,
   nothing was executed — e.g. no canary file the injected command would have created
   appears), proving no shell evaluation. Also: a hook with an UNTRUSTED argv[0] is refused
   (no spawn) and send still succeeds; a hook that times out does NOT hang/sink send.
6. **Prop** (optional, `weave/tests/prop.rs`): property that `hook_recipient_matches("*", r)`
   is true for all `r`, and that for any non-`*` pattern `p != r` (and not a broadcast-alias
   pairing) the matcher is false — the wildcard/exact invariant.

## Docs to sync
- **CHANGELOG.md** `[Unreleased]` — user-facing: "post-send hooks (`[[post_send_hook]]`)".
- **ARCHITECTURE.md** — note the post-send hook seam under the send path + the no-shell/env-only
  injection-safety statement (it is a security invariant surface).
- **README.md** / **docs/OPERATIONS.md** — `[[post_send_hook]]` config example + the
  `WEAVE_HOOK_*` env var contract.
- **docs/REPOWIRE-PARITY.md** / **docs/MULTI-SURFACE-PARITY.md** — tick the atm-core
  post-send-hook parity row.
- **docs/SECURITY.md** — document the argv-only/env-only hook execution model + trusted-program
  constraint.
- **docs/TESTING.md** — if a new test category seam is added.

## Edit order (dependency-respecting)
1. `weave-core/src/config.rs`: `PostSendHook` struct, `HookEvent`, caps, `post_send_hook`
   field (+ Debug field), the pure `hook_recipient_matches`, `Config::hooks_for`, + unit
   tests (matcher, parse, caps). (lowest layer; compiles standalone)
2. `weave-inject/src/inject.rs`: `run_post_send_hook` spawn primitive (+ argv-no-shell /
   untrusted-program unit test) AND `fire_post_send_hooks` orchestration free fn (builds
   `WEAVE_HOOK_*` env, selects hooks, bounded-spawns each, stderr-logs failures).
3. `weave/src/main.rs`: invoke `fire_post_send_hooks` at the Send/Notify (and Ack) seams.
4. `weave-mcp/src/mcp.rs`: invoke `fire_post_send_hooks` at `tool_send`/`tool_notify` (and
   ack) seams; ensure failures use `log()` (stderr).
5. `weave/tests/integration.rs` (sentinel-file hook) + `weave/tests/security.rs` (inert
   metachars / untrusted prog / timeout) + optional `weave/tests/prop.rs`.
6. Docs sync (CHANGELOG, ARCHITECTURE, README/OPERATIONS, PARITY, SECURITY).
7. Full gate incl. dual backend (hooks are backend-agnostic, but BOTH builds must stay green
   since `config.rs` is in `weave-core`): `cargo test --all-targets` and
   `cargo test --no-default-features --features libsql`; clippy + fmt for both.

## Risks / open questions
- **Q1 — empty/missing `recipient`**: atm-core treats a hook rule as fire-on-match. Recommend
  `None`/empty ⇒ `*` (match all), the most useful default; OR require `recipient` and reject
  an empty one at load. **Recommend default-to-`*`** (document it).
- **Q2 — glob richness**: support ONLY whole-string `*` (universal) + exact + BROADCAST alias,
  OR a tiny two-pointer `a*b` matcher. **Recommend whole-string `*` first** (atm-core's
  documented use is wildcard-recipient, not substring); add the two-pointer matcher only if
  parity testing shows atm uses substrings. No glob crate either way.
- **Q3 — cross-store intents**: should a queued Tier-2 intent (the `to_store` branch) fire a
  `send` hook? **Recommend YES** (a queued intent is a send; recipient=`to`), but the message
  was NOT delivered locally — document the semantic. Easy to scope to local-only if preferred.
- **Q4 — body in env**: do NOT export `WEAVE_HOOK_BODY` by default (message bodies in child
  env / `ps e` is a leak); gate behind an explicit per-hook `pass_body=true` if a use case needs it.
- **Q5 — sync vs detached spawn**: plan uses bounded-synchronous (deterministic + matches
  `run_bounded_env`). A detached/background mode is a possible future opt-in; note the
  worst-case send latency = sum of matching hook timeouts (mitigated by the clamp + the
  small `MAX_POST_SEND_HOOKS`).
- **Q6 — hook recursion**: a hook that itself runs `weave send` could loop. weave does not (and
  must not) shell out, and the hook program is operator-authored, so this is an operator
  footgun, not a weave bug — but DOCUMENT it in SECURITY/OPERATIONS (a hook should not call
  back into `weave send` for the same event). No code guard planned (would require tracking a
  re-entrancy flag across processes); flag for owner decision.
- **Q7 — broadcast fan-out**: a broadcast `send` fires the hook ONCE per send call (matched on
  the broadcast recipient alias), NOT once per fanned-out reader — confirm this is the intended
  semantic (recommend yes: the hook observes the SEND, not each delivery). The fire point is the
  send call-site, so this is naturally once-per-send; call it out so a future per-reader hook
  isn't accidentally added in the drain path.
- **Missing binary**: handled — `resolve_trusted_program` returns `None` ⇒ logged to stderr,
  send unaffected (covered by the security test).
