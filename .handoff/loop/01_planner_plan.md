# WL-049 — obscura governed web access (implements ADR-0002)

Worktree: `/home/drdave/Desktop/meta/weave-wl049-obscura` · branch `feat/wl049-obscura` · base `origin/develop @ 7a89b45`.
Plan only — no code in this packet.

## Goal

Make weave the **governance plane** for obscura's stealth web access. ALL 35 `browser_*`
operations exposed by `obscura-mcp` become reachable through weave — each gated by weave's
*existing* permission / lease / job primitives, **deny-by-default**. weave does NOT link V8
or any obscura crate. Instead, behind a new default-OFF `--features obscura`, weave **spawns
`obscura mcp` as a child (argv-only `std::process::Command`, no shell)** and acts as a minimal
hand-rolled **MCP client** speaking newline-delimited JSON-RPC over the child's stdio, built on
`std::io` + the already-present `serde_json` — zero new runtime deps, no tokio, no async. The
agent-facing surface is **one** token-light dispatcher: a single `weave_web {action, args}` MCP
tool + a `weave web <op> [args]` CLI subcommand (ADR-0003), so the standing MCP tool table grows
by ~1, not 35. Closes WL-049 and promotes ADR-0002 proposed→accepted.

## Verified ground truth (anchors — build on these)

- obscura binary is `obscura`; `obscura mcp` = stdio MCP server, newline-delimited JSON-RPC, one
  message per line (`obscura/crates/obscura-mcp/src/lib.rs:201-240` `run()`); flags `--http --host
  --port --proxy --user-agent --stealth` (`obscura-cli/src/main.rs:154-172` `Command::Mcp`).
- obscura `initialize` returns `protocolVersion:"2024-11-05"`, `serverInfo.name:"obscura-mcp"`
  (lib.rs:242-260). `tools/call` dispatch is `handle_tool_call` (lib.rs:642-705): reads
  `params.name` + `params.arguments`; **success and tool-error BOTH return JSON-RPC `result`**
  with `{"content":[{"type":"text","text":...}]}`; a tool error additionally sets `"isError":true`
  (lib.rs:690-704). The 35 op names are the `match name` arms at lib.rs:649-687.
- weave MCP dispatch: `dispatch_request` (mcp.rs:312) → `tools/call` (mcp.rs:359) → `call_tool`
  (mcp.rs:400, `match name` at :410). Standing tool schemas built by `tools()` (mcp.rs:356).
  Dangerous-tool gate `DANGEROUS_TOOLS` (mcp.rs:271) + `is_dangerous_tool` (mcp.rs:362). weave's
  own success/error envelope mirrors obscura's (mcp.rs:381-388).
- Governance Store methods to REUSE (do not invent a parallel gate):
  - permission: `store.ask(from,to,…,AskKind::ToolPermission,Some(options),…)` (store.rs:416,
    via `tool_ask_permission` mcp.rs:4195); verdict `store.permission_verdict(id,timeout)`
    (store.rs:729, via `tool_permission_status` mcp.rs:4254); `store.list_permissions` (store.rs).
  - lease: `store.reserve_lease(me,resource,ttl,note)` (store.rs:689, via `tool_lease_reserve`
    mcp.rs:4306); `store.release_lease` (store.rs:700); `store.list_leases` (store.rs:705).
  - job: `store.create_job(creator, JobSpec)` (store.rs:506, via `tool_job_create` mcp.rs:2836);
    `store.update_job(id, attempt_id, JobPatch)` (store.rs:535) for audit/terminal stamping.
- Spawn discipline to MIRROR: `inject::spawn` (inject.rs:670) resolves argv[0] to a TRUSTED
  absolute path via `resolve_trusted` / `trusted_dirs` (inject.rs:1144-1180; `WEAVE_MUX_DIR` opt-in
  at :1149 — the same env tests use); bounded child via `run_bounded_env` / `run_capture_env`
  (inject.rs:878-937) with `INJECT_TIMEOUT=5s` (inject.rs:45), `try_wait` loop + `kill`+`wait` on
  timeout. `MAX_SPAWN_ARGS` / `spawn_arg_ok` (inject.rs:487-497) cap argv.
- Hand-rolled blocking IO precedent: `weave-core/src/llm.rs` uses `reqwest::blocking` gated behind
  the `llm` feature (llm.rs:111) — i.e. a heavyweight optional dep behind a flag is the established
  pattern. weave-mcp/src/http.rs is the hand-rolled HTTP framing precedent. We will NOT pull
  reqwest; std stdio pipes + serde_json suffice.
- Feature wiring precedent: `sign`/`libsql`/`surfaces` thread weave-core → weave-mcp → weave
  (weave-core/Cargo.toml:9-17, weave-mcp/Cargo.toml:9-16, weave/Cargo.toml:17-27). `surfaces` is the
  exact template for "default OFF, zero deps in default build".
- Input caps: `MAX_IDENT_LEN=128` / `bound_ident` (mcp.rs:206,217), `MAX_BODY` (store.rs), URL
  validator precedent `model::pr_url_valid` (model.rs:1184). Fake-bin test harness:
  `make_fake_tmux`+`weave_with_fake_path` set `WEAVE_MUX_DIR` to a chmod-755 shell stub
  (weave/tests/integration.rs:962-1005, 3058-3134) — the pattern for a fake `obscura`.

## 1. ADR-0002 promotion (implementer does this in the same PR)

Edit `.handoff/decisions/ADR-0002-obscura-web-access-integration.md`:
- Status `proposed → accepted — 2026-06-13`.
- Replace the three "TO COMPLETE" open questions (line 39) with the RESOLVED decisions:
  (c) → **spawn-and-speak stdio MCP** (NOT a crate dep); no V8/no tokio in weave; deny-by-default
  governance via existing permission/lease/job; `weave_web` single dispatcher; `--features obscura`.
- The two remaining web-research items:
  - **(a) MCP server-to-server / egress security** — CLOSE it. The implementer/verifier WebSearches
    "MCP server-to-server capability composition security" + SSRF-in-headless-browser guidance and
    records 2-3 findings in the ADR. Our deny-by-default + SSRF/loopback policy (§6) is the concrete
    mitigation; the search must confirm it is sufficient and note any residual.
  - **(b) CDP stealth anti-detection / legal-ToS posture** — scope as **documented residual risk**
    in the ADR (operational/legal, not a code gate weave can enforce). Add a "Residual risk" para:
    stealth scraping ToS exposure is the operator's responsibility; weave's contribution is that
    every web op is gated+audited (non-ambient), which is security-positive but not a legal shield.
  Recommendation: **(a) closed by search, (b) documented residual.**

## 2. Architecture mapping (layer DAG respected)

| File | Layer | Change | Why |
|---|---|---|---|
| `weave-core/src/config.rs` | core | Add `obscura_bin`, `obscura_stealth`, `obscura_proxy`, `obscura_user_agent`, `obscura_policy` (allowed ops / allowed-domains / deny-by-default) config + `WEAVE_OBSCURA_*` env overlay, ALL behind `#[cfg(feature="obscura")]` | config is the lowest layer; mirror existing `llm_*`/pull env overlay style |
| `weave-core/src/webpolicy.rs` *(new)* | core | `#[cfg(feature="obscura")]` module: `WebOp` enum (the 35 ops, from `&str`), `WebPolicy` (deny-by-default decision), URL/SSRF validator (`web_url_ok`), arg caps | pure no-I/O logic → unit-testable like model.rs; lowest layer so both mcp + cli reuse |
| `weave-mcp/src/obscura.rs` *(new)* | mcp | `#[cfg(feature="obscura")]` MCP **client**: lazy spawn of `obscura mcp` (argv-only, trusted-path resolve), JSON-RPC framing (`initialize`→`tools/call`→read line), bounded reads + timeout, `Drop`/`stop()` kill, `ObscuraClient::call(op,args)->Result<String,String>` | client is mcp-layer glue (needs Store-free transport); sits beside http.rs/dashboard.rs |
| `weave-mcp/src/mcp.rs` | mcp | Add `weave_web` arm to `call_tool` (mcp.rs:410) + `tool_web` handler (governance flow §4) + a `weave_web` entry in `tools()` (one schema, `action`+`args`+optional `describe`) + add `"weave_web"` to `DANGEROUS_TOOLS` (mcp.rs:271) | one token-light dispatcher; reuse permission/lease/job Store methods named in §0 |
| `weave-mcp/src/lib.rs` | mcp | `#[cfg(feature="obscura")] pub mod obscura;` | expose the client module |
| `weave/src/main.rs` | bin | Add clap `web` subcommand (`weave web <op> [--arg…] [--stop]`) under `#[cfg(feature="obscura")]`, routing through the SAME `tool_web`/policy path (CLI parity, zero-standing-token path per ADR-0003) | bin is top layer; CLI mirrors MCP |
| `weave-core/src/config.rs` (default path) | core | When feature OFF, no symbols compiled — default build unaffected | dependency-light invariant |

No upward deps: client lives in weave-mcp (already depends on core+inject), policy + config in core,
CLI in bin. The client may call `weave_inject::resolve_trusted` (weave-mcp already depends on
weave-inject) to resolve the `obscura` binary to a trusted absolute path — reuse, don't duplicate.

## 3. The MCP-client protocol (weave → `obscura mcp`)

Lifecycle (mirror inject.rs spawn discipline + llm.rs timeout discipline):
1. **Lazy spawn on first web op.** Resolve `config.obscura_bin` (default `"obscura"`) via
   `weave_inject::resolve_trusted` → trusted absolute path or clean error "obscura not found in a
   trusted directory". Build argv `[obscura_abs, "mcp", (--stealth)?, ("--proxy",p)?,
   ("--user-agent",ua)?]` — argv vector ONLY, never a command string. `Command::new(obscura_abs)`
   `.args(argv[1..])` `.stdin(piped).stdout(piped).stderr(piped/null)`. Each argv element validated
   by a `spawn_arg_ok`-style cap.
2. **Handshake** (write a line, read a line; bounded):
   - send `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"weave","version":<v>}}}\n`
   - read one line → parse, assert `result.serverInfo.name == "obscura-mcp"` (tolerant: log+continue
     on mismatch, fail only on transport error).
   - send notification `{"jsonrpc":"2.0","method":"notifications/initialized"}\n` (no id, no reply —
     obscura skips id-less messages, lib.rs:227).
3. **Per op:** send `{"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"browser_<op>","arguments":{…}}}\n`;
   read newline-delimited lines until the line whose `id == N` (skip notifications). Result envelope:
   `result.content[0].text` is the payload; if `result.isError == true`, map to weave `Err(text)`.
   A JSON-RPC top-level `error` object → `Err`.
4. **Reuse** the same child for the whole weave session (one `ObscuraClient` cached behind a
   `OnceCell`/`Mutex` in the server state, like the injector). Monotonic request id counter.
5. **Timeouts + bounded reads:** a per-op read deadline (new `OBSCURA_TIMEOUT`, default ~30s — web
   nav is slower than inject's 5s; make it config `obscura_timeout_secs`, clamped). Bound the read
   line length (cap, e.g. `MAX_BODY`-class) so a runaway child cannot OOM weave. On timeout: kill +
   reap the child (mirror run_bounded_env :896), drop the cached client, return a clean timeout Err.
6. **Clean shutdown:** `Drop for ObscuraClient` and an explicit `weave web --stop` / `stop()` send
   `Command::kill` + `wait` (argv-only, no shell) so no zombie obscura lingers. Best-effort; never
   panic on a missing child.
7. **Error mapping:** spawn-missing → "obscura binary not found"; transport/EOF → "obscura exited";
   `isError` → the obscura message; unknown op → policy rejects before spawn (deny-by-default).

## 4. Governance flow for `weave_web {action, args}`

In `tool_web(store, me_default, args, …)`:
(a) **Identity:** resolve caller via the existing `ident(args,"me",def)` / `me_default` pattern used
    by every tool (e.g. mcp.rs:4202,4276).
(b) **Policy gate (deny-by-default):** parse `action` → `WebOp` (reject unknown). Consult
    `config.obscura_policy` via `webpolicy::WebPolicy::decide(op, url, caller)`. If not explicitly
    allowed → return `Err` WITHOUT spawning obscura. For an interactive grant, open a tracked
    permission ask: `store.ask(&caller, &policy_owner, …, AskKind::ToolPermission,
    Some(&format!("weave_web\n{action} {url}")), …)` (reuse `tool_ask_permission` path mcp.rs:4226)
    and gate on `store.permission_verdict` before proceeding. Default config = deny.
(c) **Lease (optional, rate / mutual-exclusion):** if policy marks the op (or a domain) as
    lease-required, `store.reserve_lease(&caller, "web:<domain>", ttl, note)` (store.rs:689); a
    conflict → clean Err "web resource leased by <holder>". Release via `store.release_lease` after.
(d) **Job (optional audit/durable):** if policy enables audit, `store.create_job(&caller, JobSpec{…
    web op + url …})` (store.rs:506) before forward, then `store.update_job(id, attempt, JobPatch{
    state: terminal, progress_note: outcome })` (store.rs:535) after — the append-only event log is
    the audit trail. (All writes parameterized — Store methods already use `params!`.)
(e) **Forward:** `ObscuraClient::call("browser_<action>", &args.args)` (§3). Map result/`isError`.
(f) **Return:** the obscura `content[0].text` (truncated to a weave cap) as the weave tool result.

`tools()` schema for `weave_web`: `{action: string (enum of the 35 ops), args: object,
describe?: bool}` — when `describe:true`, return the op's arg schema on demand (progressive
disclosure) instead of forwarding, so per-op schemas never sit in the standing table (ADR-0003).

## 5. Cargo feature wiring (default build gains ZERO deps)

- `weave-core/Cargo.toml` `[features]`: add `obscura = []` (pure-Rust policy/config; no new dep).
- `weave-mcp/Cargo.toml` `[features]`: add `obscura = ["weave-core/obscura"]` (client uses std +
  serde_json, both already present).
- `weave/Cargo.toml` `[features]`: add `obscura = ["weave-core/obscura", "weave-mcp/obscura"]`.
- **No new dependency anywhere.** `std::process` + `std::io::{BufRead,Write}` + existing
  `serde_json` suffice. Do NOT add a JSON-RPC helper crate; the framing is ~30 lines (cf. http.rs).
- Verify: `cargo tree` on the DEFAULT build is byte-identical before/after (guardian check).
  `cargo tree --features obscura` adds no external crate (only enables cfg'd local code).

## 6. Invariants in scope

- **No-shell / argv-only** (`weave-mcp/src/obscura.rs`, `weave/src/main.rs`): spawn `obscura mcp`
  with `Command::new(abs)` + argv vector; resolve `obscura` to a TRUSTED absolute path via
  `resolve_trusted` (never ambient `$PATH`, never `sh -c`, never a built string); validate each
  argv element with a `spawn_arg_ok`-style cap; kill via `Command::kill` argv-only.
- **Input caps + URL/SSRF** (`weave-core/src/webpolicy.rs`): cap web-arg lengths
  (`MAX_BODY`-class) and URL length (`pr_url_valid` style); `web_url_ok` REJECTS by default any URL
  whose host is loopback / RFC1918 / link-local / `*.local` / a bare IP unless the policy explicitly
  allowlists it — weave SHOULD block obscura from reaching internal/localhost addresses by default
  (SSRF guard). Recommendation: deny internal hosts unless `obscura_policy.allow_internal=true`.
- **MCP stdout discipline** (`weave-mcp/src/obscura.rs`): weave's OWN stdout stays pure JSON-RPC;
  the obscura child's stdout is a PIPE weave READS — it must NEVER be forwarded to weave's stdout.
  All weave logging → stderr via the existing `log()`.
- **Child output / token redaction** (WL-048 lesson): obscura child STDERR and any auth tokens /
  proxy creds passed to obscura must NEVER be logged by weave; redact before any log line.
- **Parameterized SQL**: all job/permission/lease writes go through existing Store methods (already
  `params!`-bound) — no new SQL literals. (No store schema change expected; dual-backend N/A unless
  a Store method is added — see Dual-backend below.)
- **Deny-by-default**: the policy gate refuses every web op unless explicitly permitted.
- **Strict layering**: config+policy in core, client in mcp, CLI in bin — no upward dep.
- **Dependency-light / token-light**: feature default OFF; one `weave_web` standing tool, not 35.

### Dual-backend?
**No** — provided the implementer reuses the EXISTING `ask` / `permission_verdict` /
`reserve_lease` / `create_job` / `update_job` Store methods (they already exist in BOTH
`store.rs` and `store_libsql.rs`). If a NEW Store method is found necessary, it MUST be mirrored in
`weave-core/src/store.rs` AND `weave-core/src/store_libsql.rs` and tested on `--features
"libsql obscura"`. Recommendation: **reuse, add no Store method.** Flag if that proves impossible.

## 7. Test layers required (feature-gated `#[cfg(feature="obscura")]`; NO real browser in CI)

1. **Unit — JSON-RPC framing/parse** (`weave-mcp/src/obscura.rs` `#[cfg(test)]`): feed canned
   newline-delimited bytes (an `initialize` reply, a `tools/call` ok reply, an `isError:true`
   reply, a top-level `error`, a multi-line stream with interleaved notifications) into the
   parser; assert correct id-matching, `content[0].text` extraction, and Err mapping. No process.
2. **Unit — webpolicy** (`weave-core/src/webpolicy.rs` `#[cfg(test)]`): deny-by-default;
   allowed-op path; `web_url_ok` rejects `http://localhost`, `http://127.0.0.1`, `http://169.254.*`,
   `http://10.x`, bare-IP; allows an allowlisted public host; arg/URL cap rejects oversize.
3. **Unit — governance** (mcp.rs `#[cfg(test)]` with the in-test fake Store): deny-by-default
   refuses without spawning; permission-granted path proceeds; lease conflict → clean Err; job
   audit row written.
4. **Integration — fake `obscura` binary** (`weave/tests/integration.rs`): clone the
   `make_fake_tmux`/`weave_with_fake_path` pattern (integration.rs:962-1005) — write a chmod-755
   stub named `obscura` to a temp dir, point `WEAVE_OBSCURA_BIN`/trusted dir at it; the stub reads
   stdin lines and echoes canned MCP `initialize` + `tools/call` replies. Assert `weave web navigate
   --url https://example.com` (policy-allowed in the test config) drives the stub and returns the
   canned text — NO real browser.
5. **MCP `weave_web` tests** (mcp.rs `McpServer`-style, incl. FAILURE paths): obscura missing →
   clean error; op denied (deny-by-default) → error, no spawn; obscura returns `isError` → mapped
   error; the success path via the fake stub.
6. **Security** (`weave/tests/security.rs`): deny-by-default holds under adversarial args; no-shell
   spawn (a shell-metachar `obscura_bin` / arg is rejected, never interpreted); SSRF/localhost
   policy blocks internal hosts; child stdout/stderr + tokens are NOT leaked into weave stdout/logs;
   argv cap enforced.
7. **Both backends build**: `cargo build/clippy/test --features obscura` AND
   `cargo build/clippy/test --no-default-features --features "libsql obscura"` (and add `sign` combo
   if CI gates it). Default build (`cargo tree`) unchanged.

## 8. Docs to sync (same PR)

- **README** — new "Governed web access (obscura)" section: the `--features obscura` flag, the
  obscura runtime dependency (separate binary, not linked), `weave web <op>` usage, deny-by-default.
- **ARCHITECTURE.md** — §0 mark WL-049 closed; add a "Governance plane: stealth web access"
  section (the spawn-and-speak MCP-client model, the permission/lease/job gate, the one-dispatcher
  surface); add a threat-model subsection (SSRF/loopback, child-process trust, ToS/stealth residual
  risk, child-output redaction).
- **CHANGELOG.md** — `[Unreleased]`: "obscura governed web access behind `--features obscura`
  (ADR-0002): `weave_web` MCP dispatcher + `weave web` CLI; deny-by-default permission/lease/job
  gating; no V8/tokio in the default build."
- **docs/REPOWIRE-PARITY.md** — §0 bottom-line + §9 ("the more"): note weave now EXCEEDS repowire's
  hosted-relay web reach with *governed stealth browsing* (reach without a daemon, gated+audited);
  update §10 remaining-work to mark the web gap closed.
- **CONTRIBUTING.md** — add the obscura feature + fake-`obscura` test pattern to the test-layer
  checklist (mirror the dual-backend note).
- **.handoff/loop/backlog.md** — check off WL-049 (line 79).
- **.handoff/decisions/ADR-0002-…md** — proposed→accepted (§1).

## 9. Edit order (leaf-first, dependency-respecting)

1. `weave-core/Cargo.toml` — add `obscura = []` feature.
2. `weave-core/src/config.rs` — `WEAVE_OBSCURA_*` config + env overlay (cfg-gated).
3. `weave-core/src/webpolicy.rs` (new) + register in `weave-core/src/lib.rs` — `WebOp`, `WebPolicy`,
   `web_url_ok`, caps + unit tests (test layer 2).
4. `weave-mcp/Cargo.toml` — `obscura = ["weave-core/obscura"]`.
5. `weave-mcp/src/obscura.rs` (new) + `weave-mcp/src/lib.rs` mod decl — the MCP client + framing
   unit tests (test layer 1).
6. `weave-mcp/src/mcp.rs` — `tool_web` (governance flow §4), `weave_web` in `call_tool` + `tools()`,
   add to `DANGEROUS_TOOLS`; governance + MCP unit tests (layers 3, 5).
7. `weave/Cargo.toml` — `obscura = [...]` feature.
8. `weave/src/main.rs` — `weave web` clap subcommand routing through `tool_web`.
9. `weave/tests/integration.rs` — fake-`obscura` stub + `weave web navigate` test (layer 4).
10. `weave/tests/security.rs` — security tests (layer 6).
11. Docs (§8) + ADR promotion (§1) + backlog tick.
12. Full gate on BOTH backends + `--features obscura` (layer 7).

## 10. Risks / open questions (recommended defaults)

- **Hand-rolled MCP-client correctness** (biggest risk): id-matching across interleaved
  notifications, partial-line reads, EOF mid-handshake. Mitigation: layer-1 unit tests with canned
  byte streams cover every branch before any real obscura.
- **obscura process lifecycle / zombies**: ensure `Drop` + `weave web --stop` reap the child; a
  panicked weave must not orphan obscura. Default: one cached child per session, killed on drop.
- **SSRF via `browser_navigate` to internal hosts**: RECOMMEND default-deny loopback/RFC1918/
  link-local in `web_url_ok`, opt-in `allow_internal`. Confirm with owner if any internal-host use
  case is required day-1 (default: no).
- **Deny-by-default UX**: first-run `weave web` returns "denied — no web policy configured" with a
  one-line hint to set `obscura_policy`. Confirm the grant mechanism: config allowlist vs.
  interactive permission ask. RECOMMEND: config allowlist for unattended; permission-ask path
  available for interactive grant (both wired, policy chooses).
- **CI must not need a real browser**: ALL tests use the fake `obscura` stub; a real-obscura
  smoke test (if any) is `#[ignore]`d. Non-negotiable.
- **35-op surface area**: the dispatcher forwards opaquely (`browser_<action>` + args) — weave need
  NOT re-declare 35 schemas; `describe:true` fetches obscura's own schema on demand. Open Q: should
  weave validate per-op required args, or forward and let obscura error? RECOMMEND forward-and-map
  (obscura already validates, e.g. navigate's "Missing url") — keeps weave thin and the 35 ops in
  ONE place. Add a weave-side URL/SSRF check only for nav-class ops.
- **Dual-backend**: confirmed reuse-only avoids it; if a new Store method becomes necessary, it is a
  dual-backend change — flag to the leader before proceeding.

## Handoff
Implementer reads THIS file as spec. Start at edit-order step 1. Reuse the named Store methods —
do not invent a parallel gate. Keep the default `cargo tree` byte-identical.
