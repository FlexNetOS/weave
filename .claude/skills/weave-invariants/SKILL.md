---
name: weave-invariants
description: The non-negotiable security and correctness invariants for the weave Rust codebase — no-shell argv-only spawning, parameterized SQL, strict module layering, paste-safe injection, input caps, destructive-op gating, and MCP stdout discipline. ALWAYS consult before writing or reviewing any weave src/ code. Use whenever editing model/store/inject/mcp/config/main, adding a mux adapter or Store method or MCP tool, or reviewing a weave diff. Do NOT use for general Rust style questions unrelated to these specific invariants.
---

# weave invariants

These are weave's load-bearing rules — security and correctness properties, not preferences. weave types into other agents' live terminals and stores their messages, so the trust budget is real. Each rule below states the *why*, because an agent that understands the reason judges edge cases correctly. Full rationale lives in `ARCHITECTURE.md` §7 and `CONTRIBUTING.md`.

## 1. No shell, ever

Every external program is spawned with `std::process::Command::new(bin)` and an **explicit argv vector**. weave never builds a shell command string and never calls `sh -c`.

**Why:** message bodies and session names are untrusted text. If any reached a shell, a body containing `;`, `$(...)`, backticks, or quotes would be command injection. With argv-only spawning there is *no* shell-injection surface — the bytes reach the target program as a single argument, uninterpreted.

**Check:** grep a diff for `sh -c`, `Command::new("sh"`, `Command::new("bash"`, or any `format!`/string-concatenation that builds a command line. Any of these is a block.

## 2. Parameterize all SQL

Every variable value in a query uses a bound `params!` placeholder. The **only** inlined SQL literals are the broadcast aliases — and those are compile-time constants derived from `model::BROADCAST`, never user input.

**Why:** the same reason as no-shell, for the storage layer. A `recipient` or `body` interpolated into SQL text would be SQL injection. `BROADCAST_SQL` is allowed inline only because a drift-guard unit test asserts it stays byte-identical to the fragment derived from `BROADCAST`, so the Rust check (`is_broadcast`) and the SQL filter (`recipient IN (...)`) can never disagree.

**Check:** any runtime value formatted into a query string is a block. New broadcast aliases must be added to `BROADCAST` (the test keeps `BROADCAST_SQL` in sync), not hand-written into SQL.

## 3. Keep the module layering acyclic

```
model  (no I/O)
  ▲
  ├── inject   store   config     (depend only on model)
  ▲
  └── mcp   setup                 (depend on store/inject/config + model)
        ▲
        └── main                  (wires everything)
```

**Why:** `model` is pure and unit-testable precisely because it has no I/O and no upward knowledge. Layering keeps the pure core testable without a DB or a terminal, and keeps reasoning local. An upward dependency (e.g. `model` reaching into `store`) breaks that and tends to introduce cycles.

**Check:** a new `use` that points up the diagram is a block. Put shared types in `model`.

## 4. Paste-safe injection

The injector's `commands_for`/`commands_for_mode` are **pure functions** returning the exact argv vectors to run. Each mux adapter submits the message **paste-safely**.

**Why:** modern TUIs (Claude Code included) run in **bracketed-paste** mode. A naive Enter after literal text can be swallowed or misread as a TUI key — this was a real `repowire` bug where injection cancelled a tool call mid-flight. Each backend therefore uses its terminal's documented idiom: tmux closes bracketed paste with the hex `ESC[201~` sequence *before* Enter; wezterm uses `--no-paste`; zellij/kitty/screen append a carriage return (byte 13). User text is placed as a single argv element behind an end-of-options `--` where the CLI supports it, so a body starting with `-` is data, not a flag.

**Check:** a new mux arm without paste-safe submission, or that concatenates text into one string, is a block. Purity means every adapter has an exact-argv unit test.

## 5. Enforce input caps

| Cap | Value | Where |
|-----|-------|-------|
| `MAX_IDENT_LEN` | identity length | MCP layer rejects over-long session names |
| `MAX_BODY` | 65536 bytes | store layer (shared by CLI/MCP/hook) |
| `MAX_INJECT_CHARS` | 240 | injector truncates on a **UTF-8 boundary** with a `…` marker |
| `id_valid` | — | rejects malicious target ids (`%3; rm -rf /`, `--listen-on=evil`, embedded spaces) |

**Why:** caps bound resource use and close injection vectors at the boundary. Truncating on a UTF-8 boundary avoids emitting invalid UTF-8; `id_valid` stops a hostile peer registration from turning into a flag or command fragment at inject time.

**Check:** a new ingress path (CLI arg, MCP field, hook payload) that skips the relevant cap is a block.

## 6. Gate destructive operations

Anything that deletes/overwrites across sessions requires an explicit confirmation. `weave_clear {scope:"all"}` truncates every session's messages and **requires `confirm:true`**; the default scope only marks the caller's own inbox read (non-destructive).

**Why:** a stray or hostile tool call must not be able to wipe the mesh. The confirm flag makes destruction an explicit, deliberate act.

**Check:** a new destructive path without a confirm gate is a block.

## 7. MCP stdout discipline

In `mcp.rs`, **only** newline-delimited JSON-RPC 2.0 protocol frames go to stdout; **all** logging/diagnostics go to stderr.

**Why:** stdout *is* the protocol channel. A stray log line on stdout corrupts the JSON-RPC stream and breaks the client. Keeping diagnostics on stderr means a malformed log can never desync the protocol.

**Check:** a `println!`/`print!` (stdout) added in the MCP server path is a block unless it is emitting a protocol frame; use `eprintln!` for logs.

## 8. Dependency-light by default

No new heavyweight dependency in the **default** build. Date/time is handled without a date crate on purpose (`model::now()` / `model::fmt_ts` use UNIX seconds + a civil-from-days formatter). Anything pulling `tokio` or a large tree belongs behind a feature flag, as `libsql` is.

**Why:** weave's value proposition is "one static dependency-light binary, no Python, no daemon." Each added default dep erodes that and enlarges the attack/maintenance surface.

**Check:** a new `[dependencies]` entry (not `optional`/feature-gated) needs an explicit, recorded justification that std genuinely can't cover.

## Quick audit checklist

- [ ] No `sh -c` / string-built commands — argv-only spawning
- [ ] No runtime value interpolated into SQL — bound `params!` only
- [ ] No upward module dependency
- [ ] New mux arm is pure + paste-safe + has an exact-argv test
- [ ] Input caps + `id_valid` enforced on every new ingress
- [ ] New destructive path is `confirm`-gated
- [ ] No stdout writes in the MCP path except protocol frames
- [ ] No new heavyweight default dependency
