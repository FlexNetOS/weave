# Weave architecture graphs — control, data, and domain flows

This document is an operator-oriented companion to `ARCHITECTURE.md`. It uses
ASCII graphs so the live orchestration plane can be reviewed in terminals,
handoffs, and PR comments without a diagram renderer.

## Legend

```text
PRIMARY / authoritative
  DB write/read       durable truth; safe even when live wake fails

SECONDARY / advisory
  mux nudge           content-free or capped live wake into a terminal pane
  HTTP push           remote delivery request; receiver still writes its own DB
  responder ACK       status message, not final answer unless ask is answered/acked

OBSERVABILITY
  delivery_log        queued/injected/not_injectable/inject_failed/drained/answered
  reads               read receipts
  asks/jobs/leases    lifecycle truth
  doctor/scan         derived diagnostics, not a second source of truth
```

## 1. Component graph

```text
                         +-----------------------------+
                         |        HUMAN / AGENT        |
                         | CLI, MCP client, dashboard, |
                         | Telegram, Slack, hooks      |
                         +--------------+--------------+
                                        |
                                        v
+--------------------------------------------------------------------------------+
|                                  weave binary                                  |
|                                                                                |
|  +------------------+   +-------------------+   +---------------------------+  |
|  | weave/src/main   |   | weave-mcp         |   | surfaces / bridges        |  |
|  | CLI + hooks      |   | MCP + HTTP/API    |   | dashboard/telegram/slack  |  |
|  +--------+---------+   +---------+---------+   +-------------+-------------+  |
|           |                       |                           |                |
|           +-----------+-----------+---------------------------+                |
|                       |                                                        |
|                       v                                                        |
|  +----------------------------------------------------------------------------+|
|  |                              weave-core                                    ||
|  | model.rs | store.rs | store_libsql.rs | config.rs | sign.rs | memory.rs    ||
|  | archive/export/session/webpolicy/job/ask/lease/permission primitives       ||
|  +---------------------------+------------------------------------------------+|
|                              |                                                 |
|                              v                                                 |
|  +----------------------------------------------------------------------------+|
|  |                             persistence                                    ||
|  | SQLite default OR libSQL backend                                            ||
|  | messages, reads, peers, asks, jobs, leases, delivery_log, outbox, keys      ||
|  +----------------------------------------------------------------------------+|
|                                                                                |
|  +----------------------------------------------------------------------------+|
|  |                             weave-inject                                   ||
|  | mux detection + safe argv-only injection/spawn/kill                        ||
|  | tmux, zellij, kitty, wezterm, screen, iTerm2 boundaries                    ||
|  +----------------------------------------------------------------------------+|
+--------------------------------------------------------------------------------+
```

Why these boundaries exist:

- `weave/src/main.rs` owns operator-local CLI, lifecycle hooks, and feature-gated
  subcommands.
- `weave-mcp` owns protocol surfaces: MCP stdio, HTTP/API, dashboard dispatch,
  and token-light progressive disclosure.
- `weave-core` owns durable model/store/config/security primitives.
- `weave-inject` owns terminal/mux side effects and keeps shell execution out of
  the data/model layers.
- The store is the primary channel. Mux injection is only a secondary wake/nudge.

## 2. Primary and secondary local session flow

```text
LOCAL MACHINE
=============

+-------------------+                         +-------------------+
| Session A         |                         | Session B         |
| orchestrator      |                         | worker/reviewer   |
| CLI/MCP/hook      |                         | CLI/MCP/hook      |
+---------+---------+                         +---------+---------+
          |                                             ^
          | weave ask/send/notify                       |
          v                                             |
+---------+---------------------------------------------+---------+
|                          Weave store                            |
| messages | reads | asks | jobs | peers | delivery_log | leases  |
+---------+---------------------------------------------+---------+
          |                                             ^
          | durable row is truth                         |
          |                                             |
          +------------------+                          |
                             |                          |
                             v                          |
                    +--------+--------+                 |
                    | weave-inject    |                 |
                    | mux nudge       |-----------------+
                    +-----------------+
                             |
                             v
                    tmux/zellij/kitty/etc pane
```

Ask/answer lifecycle:

```text
1. A opens ask
   A -> Store::ask()
      -> messages row for question
      -> asks row with correlation id
      -> delivery_log queued

2. Weave attempts live wake
   A -> weave-inject -> B mux pane
      -> delivery_log injected/ok OR not_injectable OR inject_failed

3. B receives
   B hook/inbox -> Store::inbox(mark_read=true)
      -> reads row
      -> delivery_log drained

4. B answers
   B -> Store::answer()
      -> answer message linked to ask
      -> asks state answered
      -> delivery_log answer queued/injected/etc.
```

## 3. Session identity and routing

```text
HUMAN-FACING NAME                    STABLE LIVE INSTANCE
------------------                   ---------------------
"flexnetos_runner" --------------+->  session_id=sess_abcd...
"weave"                          |    basis=birth_cert
"worker"                         |    pid/host/target fallback
                                 |    mux target/socket/pane
                                 |
                                 v
+----------------------------------------------------------------+
| peers table                                                     |
| name | birth_cert | host | pid | mux | target | socket | tags  |
+----------------------------------------------------------------+
                                 |
                                 v
+----------------------------------------------------------------+
| routing resolution                                              |
| - alias route: "worker"                                         |
| - exact route: "sess_<16-hex>" -> exactly one peer              |
| - ambiguous target: diagnostics + injection avoidance barrier     |
+----------------------------------------------------------------+
```

Current state:

- `peers`, `scan`, and `sessions` expose `session_id`.
- CLI and MCP `send`, `notify`, and `ask` accept `sess_<16-hex>` through the
  shared core resolver.
- CLI `job delegate` and MCP `weave_job_delegate` accept a peer alias or exact
  session id, create a queued assigned job, and notify the worker with
  `JOB_DELEGATED <job_id>`.
- Shared mux targets are now a safety barrier: point-to-point sends/asks/answers
  that resolve to an ambiguous `(mux, target, socket)` skip live injection, keep
  durable delivery queued, and record `not_injectable/ambiguous_target` in
  `delivery_log` instead of typing into the wrong pane.

## 4. Doctor and scan diagnostics

```text
+-------------------+
| weave doctor/scan |
+---------+---------+
          |
          v
+---------------------------+
| gather peer rows          |
| local + federated stores  |
+-------------+-------------+
              |
              v
+---------------------------+
| classify facts            |
| - TTL/PID liveness        |
| - mux/pane capability     |
| - recent response         |
| - shared target anomaly   |
| - routing anomalies       |
+-------------+-------------+
              |
              v
+---------------------------+
| render human + JSON       |
| responsive/reachable/etc  |
+---------------------------+
```

Important current nuance: a row can be process/heartbeat-stale but still have a
reachable mux target. Weave therefore exposes both folded human status tokens and
orthogonal JSON dimensions such as `registered`, `process_alive`, `pane_alive`,
`injectable`, `reachable`, `responsive_recently`, `last_heartbeat`,
`last_transport_success`, `last_response`, `stale_reason`, and `inject_probe`.
`doctor --json` aggregates the same dimensions so status is visible without
reconstructing it from ad-hoc CLI text.

## 5. Across-wire communication

```text
CROSS-STORE PULL
================

Machine A store                         Machine B store
+----------------+                      +----------------+
| outbox intent  |                      | inbox/messages |
+-------+--------+                      +--------+-------+
        |                                        ^
        | B pulls from allowed source            |
        +----------------------------------------+
             verify signature / idempotency
             B writes into B's own store


CROSS-MACHINE PUSH
==================

Machine A                                  Machine B
+-------------+                            +----------------------+
| weave push  | --HTTPS POST /push-------> | dashboard/serve      |
+------+------+   push token + signed     +----------+-----------+
       |          intent; distinct from              |
       |          operator /api token                |
       |                                             v
       |                                  commit_pulled pipeline
       |                                  verify/revalidate/dedup
       |                                             |
       |                                             v
       |                                  B writes B's own inbox
       |                                             |
       |                                             v
       |                                  optional local nudge
```

Ownership rule: A never writes B's database directly. B receives, verifies, and
commits into B's own store.

## 6. Obscura domain flow

```text
AGENT REQUEST
=============
"open page / search web / browser op"
          |
          v
+------------------------------+
| weave_web / weave web        |
| governance entrypoint        |
+--------------+---------------+
               |
               v
+------------------------------+
| weave-core::webpolicy        |
| deny-by-default policy       |
| SSRF / localhost / private   |
| URL validation               |
+--------------+---------------+
               |
               v
+------------------------------+
| coordination gates           |
| permission ask?              |
| lease reserve?               |
| job record/update?           |
+--------------+---------------+
               |
               v
+------------------------------+
| ObscuraClient                |
| hand-rolled MCP client       |
| stdio JSON-RPC               |
+--------------+---------------+
               |
               v
+------------------------------+
| external obscura mcp process |
| browser automation / stealth |
+--------------+---------------+
               |
               v
+------------------------------+
| web/browser/domain result    |
+--------------+---------------+
               |
               v
+------------------------------+
| result back through Weave    |
| stdout protocol-safe         |
| secrets never logged         |
+------------------------------+
```

Why this shape:

- Weave owns governance: policy, permissions, leases, job trail, MCP/CLI surface.
- Obscura owns browser mechanics.
- Weave does not link browser/runtime internals into the default binary.
- Obscura child stderr must never leak into MCP JSON-RPC stdout.
- Internal/private URL access is denied by default.

## 7. CC Switch provider/vendor flow

Source evidence for this map: the restored upstream archive at
`cc-switch-main.zip` (`package.json` says `cc-switch` v3.16.3) plus Weave's
bridge in `weave/src/provider_switch.rs`. CC Switch is not a tiny provider DB;
it is a Tauri desktop control plane with UI, local DB, live config
writers, MCP/prompt/skill sync, a local proxy, failover, health, usage accounting,
DeepLink import, and per-app providers.

### 7.1 Upstream CC Switch shape, as dropped in the archive

```text
cc-switch-main.zip
└─ cc-switch-main/                         All-in-One Assistant for Claude Code,
   ├─ package.json                         Codex & Gemini CLI (v3.16.3)
   ├─ src/                                 React/Vite frontend
   │  ├─ App.tsx                           app tabs: providers, proxy, MCP,
   │  ├─ components/providers/*            prompts, skills, sessions, settings
   │  ├─ hooks/useProviderActions.tsx
   │  └─ config/*ProviderPresets.ts
   │
   └─ src-tauri/src/                       Rust/Tauri backend
      ├─ database/schema.rs                SQLite SSOT schema
      ├─ database/dao/providers.rs         provider rows/current marker
      ├─ provider.rs                       Provider + ProviderMeta model
      ├─ services/provider/live.rs         live config writers
      ├─ services/proxy.rs                 local route takeover/backup
      ├─ proxy/provider_router.rs          failover/circuit breaker routing
      ├─ mcp/*                             MCP sync to host apps
      ├─ prompt.rs / prompt_files.rs       prompt sync
      ├─ services/skill.rs                 skill sync
      ├─ claude_* / codex_config.rs /
      │  gemini_* / opencode_config.rs /
      │  openclaw_config.rs / hermes_config.rs
      └─ deeplink/*                        ccswitch:// import flow
```

### 7.2 CC Switch internal data/control plane

```text
                          +------------------------------+
                          | CC Switch desktop UI         |
                          | React/Vite tabs + forms      |
                          +---------------+--------------+
                                          |
                                          | Tauri commands
                                          v
+--------------------------------------------------------------------------------+
|                              CC Switch Rust backend                             |
|                                                                                |
| +-----------------------+    +-----------------------+    +------------------+ |
| | database/schema.rs    |    | provider.rs           |    | app_config.rs    | |
| | SQLite SSOT           |    | Provider/ProviderMeta |    | AppType + modes  | |
| +-----------+-----------+    +-----------+-----------+    +---------+--------+ |
|             |                            |                          |          |
|             v                            v                          v          |
| +-----------------------+    +-----------------------+    +------------------+ |
| | providers table       |    | settings_config JSON  |    | app capability   | |
| | app_type, id, name,   |    | auth/config/env/meta  |    | switch/additive  | |
| | is_current, meta,     |    | modelCatalog/routes   |    | claude/codex/... | |
| | sort/failover fields  |    +-----------+-----------+    +---------+--------+ |
| +-----------+-----------+                |                          |          |
|             |                            v                          |          |
|             |              +----------------------------+            |          |
|             |              | services/provider/live.rs  |            |          |
|             |              | write_live_with_common_*   |            |          |
|             |              | sync_current_provider_*    |            |          |
|             |              +-------------+--------------+            |          |
|             |                            |                           |          |
|             v                            v                           v          |
| +----------------------+    +-------------------------+    +------------------+|
| | proxy_config         |    | MCP / prompt / skills   |    | proxy logs,      ||
| | provider_health      |    | sync stores             |    | usage rollups    ||
| | failover queue       |    | mcp_servers/prompts     |    | stream checks    ||
| +----------+-----------+    +------------+------------+    +------------------+|
|            |                             |                                      |
|            v                             v                                      |
| +-----------------------+   +-----------------------------------------------+  |
| | proxy/provider_router |   | host app config writers                        |  |
| | current/failover      |   | Claude, Claude Desktop, Codex, Gemini,         |  |
| | circuit breakers      |   | OpenCode, OpenClaw, Hermes                    |  |
| +-----------------------+   +-----------------------------------------------+  |
+--------------------------------------------------------------------------------+
```

### 7.3 Weave bridge: what is wired today

```text
READ/SWITCH BRIDGE TODAY
========================

~/.cc-switch/cc-switch.db
providers(id, app_type, name, settings_config, category, is_current)
settings(common_config_<app>, current_provider_<app>, ...)
        |
        | sqlite query/update by `weave provider-switch`
        v
+------------------------------+
| weave/src/provider_switch.rs |
| small Rust-native bridge     |
+--------------+---------------+
               |
               | supports ProviderSwitchApp only:
               |   claude, codex, gemini
               v
+------------------------------+
| ProviderRow                  |
| id/name/category/is_current  |
| settings_config JSON         |
+--------------+---------------+
               |
     +---------+---------+-------------------+
     |                   |                   |
     v                   v                   v
+------------+     +-------------+     +-------------+
| list       |     | current     |     | models      |
| DB read    |     | DB read     |     | DB + catalog|
+------------+     +-------------+     +------+------+
                                             |
                                             | optional additive probe only
                                             v
                                      Ollama /api/tags
                                      (keep until shimmy/ruvllm
                                       are proven replacements)

READ-ONLY STATUS PATH TODAY
===========================

weave provider-switch status [--json]
        |
        v
open CC Switch DB read-only
        |
        v
report DB present/readable, provider schema coverage,
supported-vs-observed apps, current provider/model per supported app,
live config agreement, proxy/failover/health table presence
        |
        v
weave doctor / doctor --json provider_switch rollup

WRITE/SWITCH PATH TODAY
=======================

weave provider-switch switch --app <claude|codex|gemini> <provider_id>
        |
        v
load provider row + optional common_config_<app>
        |
        v
apply_live(app, provider)
        |
        +--> Claude: ~/.claude/settings.json
        |           preserve existing hooks + mcpServers if provider lacks them
        |
        +--> Codex: ~/.codex/config.toml
        |           preserve existing notify hook wake line
        |           write auth.json only when explicit OPENAI_API_KEY exists
        |
        +--> Gemini: ~/.gemini/.env and ~/.gemini/settings.json
                    merge/preserve settings outside provider-owned keys
        |
        v
update CC Switch DB:
  providers.is_current = 1 for selected provider
  settings.current_provider_<app> = provider_id
```

### 7.4 What is not wired yet, and why the graph matters

```text
CC SWITCH CAPABILITY               WEAVE STATE NOW             GAP
--------------------               ---------------             ---
Claude/Codex/Gemini providers ---> CLI + status/doctor -----> no MCP status yet
OpenCode/OpenClaw/Hermes --------> reported unsupported ----> no bridge semantics yet
Claude Desktop profiles ---------> reported unsupported ----> no proxy route mapping
local proxy/takeover ------------> table presence shown ----> no route/failover view
provider_health/failover --------> table presence shown ----> no orchestration signal
MCP/prompt/skill sync -----------> table presence shown ----> no CC Switch sync map
usage/proxy logs ----------------> table presence shown ----> no cost/health feedback
DeepLink import -----------------> not in bridge -----------> no import/status story
```

The correct integration boundary is therefore **not** "Weave owns CC Switch" and
not "copy random provider fields by hand." The boundary is:

```text
CC Switch owns: provider catalog, app-specific live config semantics, proxy,
                failover, health, usage, MCP/prompt/skill sync, DeepLink import.

Weave owns:     live agent/session orchestration, jobs, asks, routing, doctor,
                policy surfaces, and runner delegation.

Bridge owns:    exact, source-truth interop contracts:
                - read provider/app/proxy/health state from CC Switch DB
                - request a provider/model policy for a Weave job/session
                - apply switch only through semantics that preserve Weave hooks
                - surface drift/mismatch in doctor/scan/status
                - never silently drop unsupported CC Switch apps/capabilities
```

The status/doctor slice now reads enough of the actual CC Switch schema to make
unsupported coverage explicit instead of silent. Remaining implementation slices
are MCP read-only status (if it stays token-light), runner/job provider/model
policy requests, deeper app coverage beyond Claude/Codex/Gemini, and additive
shimmy/ruvllm model discovery while preserving Ollama until replacements are
parity-proven.

## 8. CLI/daemon-first vs MCP friction map

```text
ZERO-STANDING-COST PATH
=======================
agent/human -> CLI command -> output only when invoked
             `rtk weave ...` can compress output

LOW-FRICTION LIVE PATH
======================
hook/daemon/responder -> background store drain / heartbeat / ACK
                       -> main agent stays available

MCP PATH
========
agent -> standing tool schema -> tool call -> JSON-RPC -> handler
       good for structured calls and MCP-only hosts
       risky when the flat tool table becomes a context tax
```

Architecture consequence: MCP remains supported, but it should not be the only
control path. CLI and daemon/hook paths win for long-running orchestration because
they do not consume standing context, can run out-of-band, and keep the main
interactive agent free. This creates parity pressure: when a capability lands in
CLI first, MCP catalog/handler parity must be checked explicitly; when a capability
lands in MCP first, CLI parity must also be checked. The enforced ledger is
`weave tui --json --pane commands`: every command listed by `weave --help` must
carry an `mcp_decision` and a read-only `status_surface`, including sign-gated
`key`/`audit` and surfaces-gated `dashboard`/`push`/`telegram`/`slack`; background
paths (`daemon`, `hook`, `responder`) must advertise status/health visibility.

## 9. Orchestrator to worker/runner flow

```text
MAIN SESSION
orchestrator/control plane
          |
          | weave job delegate
          v
+-----------------------------+
| jobs table                  |
| queued job, owner, assignee |
+-------------+---------------+
              |
              | JOB_DELEGATED message
              v
+-----------------------------+
| worker inbox / live nudge   |
+-------------+---------------+
              |
              v
BACKGROUND WORKER
executor/reviewer/runner
          |
          | weave job dispatch
          | (claim -> runner -> update/result)
          v
+-----------------------------+
| job lifecycle               |
| queued -> running -> done   |
+-----------------------------+
```

Runner/model seam:

```text
Weave policy                         flexnetos_runner execution
-------------                        -------------------------
job/delegation intent  ------------> route/execute
WEAVE_FXRUN_AGENT -----------------> selected agent
WEAVE_JOB_ID / ATTEMPT -----------> fenced execution context
policy_owner=weave                  agent_source=weave
```

Ownership boundary:

- Weave: live coordination, session identity, asks/messages/jobs, routing policy.
- handoff: durable task continuity ledger and restart truth.
- Rusty-IDD: code intelligence / symbol graph.
- flexnetos_runner: execution-plane mechanics.
- ATC/model backends: actual model/provider execution.


## 10. Architecture graph freshness contract

These diagrams are review scaffolding, not replacement source truth. A change is
"graph-visible" when it changes any of these planes:

```text
PLANE                        REQUIRED GRAPH TOUCH
-----                        --------------------
crate/component boundary --> §1 component graph
message / ask / wake path -> §2 primary/secondary flow
routing/session identity --> §3 session identity and routing
doctor/scan/status facts --> §4 diagnostics
cross-store/network path --> §5 across-wire communication
web/browser governance ----> §6 Obscura domain flow
provider/model policy -----> §7 CC Switch provider/vendor flow
CLI/MCP/hook/daemon trade -> §8 friction map + command ledger
job/runner execution ------> §9 orchestrator to worker/runner flow
```

Review rule: if a PR changes one of those planes, either update this document and
`CHANGELOG.md`/`.handoff/loop/backlog.md`, or state explicitly why the graph is
unchanged. This keeps future handoffs from treating stale diagrams as current
operator truth.
