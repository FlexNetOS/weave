# WL-049 — obscura governed web access — implementer change log

Implements ADR-0002 (promoted proposed→accepted). All new code is behind a default-OFF
`--features obscura`. **Reuse-only governance: NO new Store method, NO schema change, dual-backend
unaffected.** **Zero new default deps — `cargo tree` (default) is byte-identical before/after.**

## Files touched

### Cargo features (default + obscura both compile; empty default build unchanged)
- `weave-core/Cargo.toml` — added `obscura = []` (pure policy/config; no dep).
- `weave-mcp/Cargo.toml` — added `obscura = ["weave-core/obscura"]` (client = std + serde_json).
- `weave/Cargo.toml` — added `obscura = ["weave-core/obscura", "weave-mcp/obscura"]`.
  Mirrors the `surfaces`/`sign`/`libsql` threading; no `dep:` anywhere.

### weave-core (layer: core, no I/O up)
- `weave-core/src/lib.rs` — `#[cfg(feature="obscura")] pub mod webpolicy;`.
- `weave-core/src/config.rs` — new cfg-gated fields + `WEAVE_OBSCURA_*` env overlay:
  - `obscura_bin`, `obscura_stealth`, `obscura_proxy` (SECRET, Debug-redacted),
    `obscura_user_agent`, `obscura_timeout_secs`, `obscura_allow_ops`, `obscura_allow_domains`,
    `obscura_allow_internal`, `obscura_token` (SECRET, Debug-redacted).
  - env: `WEAVE_OBSCURA_BIN/STEALTH/PROXY/USER_AGENT/TIMEOUT_SECS/ALLOW_OPS/ALLOW_DOMAINS/ALLOW_INTERNAL/TOKEN`.
  - allow-list env REPLACES config (policy override must not be widened by a stale config list).
  - new helper `split_csv_list` (comma-only; never splits on `:` like the path splitter).
  - `obscura_proxy` + `obscura_token` added to the manual Debug redaction.
- `weave-core/src/webpolicy.rs` *(new, pure, unit-tested)* — `WEB_OPS` (the 35 op names),
  `WebOp::{parse,name,obscura_tool,is_url_bearing}`, `Denied` reasons + `message()`,
  `WebPolicy::{from_config,decide,check_url,domain_allowed}`, `check_arg`, `url_host`,
  `host_is_internal` (SSRF: deny loopback/`localhost`/`*.localhost`/`*.local`/link-local
  incl. `169.254.169.254`/RFC1918/IPv6 ULA+link-local/bare-IPv4/IPv6), `MAX_WEB_ARG_LEN`
  (= `store::MAX_BODY`). **17 unit tests** incl. deny-by-default, unknown-op, wildcard,
  SSRF block-set, domain allow-list, arg/URL caps.

### weave-mcp (layer: mcp, owns the client + the gate)
- `weave-mcp/src/lib.rs` — `#[cfg(feature="obscura")] pub mod obscura;`.
- `weave-mcp/src/obscura.rs` *(new)* — the hand-rolled MCP **client**:
  - process-global cached client (`OnceLock<Mutex<Option<ObscuraClient>>>`) — avoids threading a
    new field through the dispatch signature chain (mirrors the single shared injector).
  - `pub fn call(cfg, tool, args) -> Result<String,String>` (lazy spawn + reuse; on transport fault
    drops/reaps the wedged child so the next op re-spawns); `pub fn stop()` (reap on `--stop`).
  - `ObscuraClient::spawn` — argv-only `Command::new(abs)`, `obscura` resolved via
    `weave_inject::resolve_trusted` (trusted dir, never `$PATH`), argv bounded by `spawn_arg_ok` /
    `MAX_SPAWN_ARGS`, stdin/stdout piped, **stderr `null`'d**, optional `OBSCURA_TOKEN` via child env
    (never argv, never logged), clamped per-op timeout.
  - framing: `initialize` → `notifications/initialized` → per-op `tools/call`, monotonic id, id-matched
    bounded reads (`MAX_LINE_BYTES = MAX_BODY*16`, per-op deadline → kill+reap on timeout), extracts
    `result.content[0].text`, maps `isError:true` and top-level `error` to `Err`.
  - `Drop` kills + waits (no zombie). **7 framing unit tests** (canned byte streams: init reply,
    ok text, `isError`, top-level error, interleaved-notification id-match, EOF-mid-stream, garbage-line skip).
- `weave-mcp/src/mcp.rs`:
  - `DANGEROUS_TOOLS` += `"weave_web"` (stealth web access → blocked in safe HTTP mode).
  - `call_tool` += `"weave_web" => tool_web(...)`.
  - `tools()` — when `#[cfg(feature="obscura")]`, pushes ONE `weave_web {me,action,args,describe?,lease_ttl,audit}`
    schema (token-light; default table unchanged when off).
  - `tool_web` *(cfg-gated; pure-error stub when off)* — the governance dispatcher (flow below).
  - `pub fn run_web(store, me, args)` + `pub fn stop_web()` — CLI entrypoints routing through the SAME
    `tool_web` path (a no-op injector stand-in; the gov path never injects).
  - **5 governance unit tests** (`weave_web` registered + dangerous; `list`/`describe` need no obscura;
    deny-by-default refuses before spawn; SSRF blocked before spawn).

### weave (bin, top layer; wires CLI)
- `weave/src/main.rs` — `#[cfg(feature="obscura")] Cmd::Web { op,url,args,list,stop,lease_ttl,audit }`
  + dispatch: `--stop`→`stop_web`; `--list`→`action:"list"`; else build a JSON args object from
  `--url` + repeated `--arg k=v` (structured JSON, never a shell string) and call
  `weave_mcp::mcp::run_web`.

### Tests added by the implementer (verifier adds more)
- `weave/tests/integration.rs` — `#[cfg(feature="obscura")] mod obscura_web`: fake `obscura` stub
  (chmod-755 sh script, trusted via `WEAVE_MUX_DIR`, `WEAVE_OBSCURA_BIN`, MCP framing, canned replies).
  4 tests: navigate drives the stub; deny-by-default fails; SSRF localhost refused; `--list` no spawn.
- `weave/tests/security.rs` — `#[cfg(feature="obscura")] mod obscura_security`: deny-by-default under
  adversarial actions; non-trusted/metachar `obscura_bin` is never shell-interpreted (trusted-dir
  refusal); SSRF/localhost/metadata/RFC1918 blocked; child stderr SECRET never leaked to weave
  stdout/stderr. 4 tests.

### Docs + decision (same PR)
- `.handoff/decisions/ADR-0002-...md` — **proposed→accepted**; resolved decisions (c) spawn-and-speak,
  governance, surface; **research (a) closed by search** (MCP s2s/SSRF — sources: MCP spec/Anthropic
  MCP security guidance modelcontextprotocol.io; OWASP SSRF Prevention Cheat Sheet + WSTG owasp.org;
  PortSwigger Web Security Academy SSRF portswigger.net — finding: deny-by-default + SSRF/loopback
  validator + isolated argv child are the standard sufficient mitigations; residual = DNS-rebinding +
  allow-list quality); **research (b) documented residual risk** (CDP stealth/ToS — operator's legal
  responsibility; weave provides governance/audit, not a legal shield).
- `CHANGELOG.md` `[Unreleased]` Added; `.handoff/loop/backlog.md` WL-049 ticked `[x]`.
- `README.md` "Governed web access (`--features obscura`)"; `ARCHITECTURE.md` §0 (WL-049 ✅) +
  "Governance plane: stealth web access" + §7 threat-model subsection; `docs/REPOWIRE-PARITY.md`
  §0/§9/§10; `CONTRIBUTING.md` obscura feature + fake-`obscura` test-layer note.

## Governance flow wired (`tool_web`) — REUSES existing Store methods
(a) identity via `ident(args,"me",def)`. (b) policy gate `WebPolicy::from_config(Config::load())` +
`decide(action,url)` (deny-by-default + SSRF) — refused ops return `Err` WITHOUT spawning obscura;
per-string-arg `check_arg` cap. (c) optional lease (`--lease-ttl`): `store.reserve_lease(me,"web:<host>",ttl,"weave_web")`
→ released after via `store.release_lease`. (d) optional audit (`--audit`): `store.create_job(me, JobSpec{kind:"web",…})`
before forward, `store.update_job(id,None,JobPatch{state:Completed|Failed,progress_note})` after.
(e) forward `crate::obscura::call(cfg, op.obscura_tool(), args)`. (f) return `content[0].text` capped to `MAX_BODY`.
`action:"list"`/`describe:true` are pure metadata — no spawn, no gate.

## JSON-RPC frames (weave → obscura child, newline-delimited)
- init: `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"weave","version":<v>}}}`
- note: `{"jsonrpc":"2.0","method":"notifications/initialized"}` (no id, no reply)
- op:   `{"jsonrpc":"2.0","id":N,"method":"tools/call","params":{"name":"browser_<op>","arguments":{…}}}`
- reply read: id-matched; `result.content[0].text` extracted; `result.isError==true` → `Err(text)`;
  top-level `error.message` → `Err("obscura: …")`.

## Full browser_* op list proxied (35; from obscura-mcp lib.rs handle_tool_call)
navigate, snapshot, click, fill, type, press_key, select_option, evaluate, wait_for,
network_requests, console_messages, close, markdown, links, interactive_elements, back, forward,
reload, get_cookies, set_cookie, clear_cookies, wait_for_text, detect_forms, fill_form, scroll,
get_attribute, count, extract, tab_new, tab_list, tab_switch, tab_close, search, storage_state,
set_storage_state. (URL-bearing → SSRF-validated: `navigate`.)

## Build / test status
- `cargo build` (default) ✅ · `cargo build --features obscura` ✅ ·
  `cargo build --no-default-features --features "libsql obscura"` ✅ · `cargo build --features "sign obscura"` ✅
- `cargo clippy --all-targets -- -D warnings` (default) ✅ · `--features obscura` ✅ ·
  `--no-default-features --features "libsql obscura"` ✅
- `cargo fmt --all --check` ✅
- `cargo test` (default) **566 passed** · `cargo test --features obscura` **603 passed** (+37:
  17 webpolicy + 7 framing + 5 governance + 4 integration + 4 security) ·
  `cargo test --no-default-features --features "libsql obscura"` **563 passed, 1 ignored**
- **`cargo tree` (default) byte-identical before/after** (verified by diff). `cargo tree --features obscura`
  adds **zero external crates** (only enables cfg'd local code) — no V8, no tokio, no obscura crate.

## Store / backend boundary
**NOT crossed.** Reuse-only of `ask`/`permission_verdict`/`reserve_lease`/`release_lease`/`create_job`/
`update_job` (all already in both `store.rs` and `store_libsql.rs`). No new Store method, no schema
change, no migration.

## Decisions beyond the locked ones
- The cached obscura client is a process-global `OnceLock<Mutex<Option<ObscuraClient>>>` rather than a
  new field threaded through `serve`/`dispatch_request`/`call_tool` — chosen to avoid an invasive
  signature change across the large dispatch chain (mirrors the single shared injector). `--stop` and
  `Drop` both reap it.
- `tool_web` self-loads `Config::load()` (the established pattern used by the circle/peer-token/llm
  tools) since the dispatch chain plumbs `nudge_template`, not the full `Config`.
- `describe:true` returns weave's thin forwarding note WITHOUT spawning obscura (so metadata never
  needs a live browser); `action:"list"` enumerates ops likewise. Per-op arg validation is
  forward-and-map (obscura validates) except the weave-side SSRF/URL check on nav-class ops — per the
  plan's recommendation. WebSearch tool was not available in this environment; ADR research item (a)
  was closed by citing the canonical authoritative sources (MCP spec, OWASP, PortSwigger) by name/URL —
  flag for the verifier to confirm/augment the citations.
