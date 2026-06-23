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
| - ambiguous target: diagnostics flag now; avoidance is backlog  |
+----------------------------------------------------------------+
```

Current state:

- `peers`, `scan`, and `sessions` expose `session_id`.
- CLI `send`, `notify`, `ask`, and `job delegate` accept `sess_<16-hex>`.
- MCP session-id recipient resolution and MCP job delegation are tracked backlog
  work, because MCP still validates `to` as a peer alias in several paths.

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
reachable mux target. A single folded status token can hide that nuance, so the
backlog calls for dimensional fields such as `registered`, `process_alive`,
`pane_alive`, `injectable`, `responsive_recently`, `last_heartbeat`,
`last_transport_success`, `last_response`, and `stale_reason`.

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
| weave push  | --HTTP POST /api---------> | dashboard/serve write|
+------+------+   bearer + signed intent  +----------+-----------+
       |                                             |
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
- Obscura child stderr/tokens must never leak into MCP JSON-RPC stdout.
- Internal/private URL access is denied by default.

## 7. CC Switch provider/vendor flow

```text
CC Switch DB
~/.cc-switch/cc-switch.db
providers + settings
        |
        | weave provider-switch list/current/models/switch/switch-model
        v
+------------------------------+
| weave/src/provider_switch.rs |
| Rust-native bridge           |
+--------------+---------------+
               |
               v
+------------------------------+
| provider snapshot transform  |
| - common config merge        |
| - app-specific model update  |
| - hook/MCP preservation      |
+-------+----------+-----------+
        |          |           |
        v          v           v
   Claude       Codex       Gemini
 settings.json  config.toml .env/settings.json
        |          |           |
        v          v           v
 live host model/vendor config updated
```

Current limits to track:

- The bridge supports `claude`, `codex`, and `gemini`, but it is CLI-only and
  sqlite-build-only.
- It preserves lifecycle hooks where the target file shape is known, but provider
  switching is not yet tied into Weave's session/job/runner policy model.
- `models` can probe local Ollama; owner policy now says Ollama must remain until
  shimmy/ruvllm replacement is proven, so future model discovery should account
  for shimmy/ruvllm without prematurely deleting Ollama support.
- CC Switch provider state is not yet surfaced in `doctor`, `scan`, or worker
  delegation/runner routing diagnostics.

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
lands in MCP first, CLI parity must also be checked.

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
          | claim/update/result
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
policy_owner=weave                  agent_source=weave
```

Ownership boundary:

- Weave: live coordination, session identity, asks/messages/jobs, routing policy.
- handoff: durable task continuity ledger and restart truth.
- Rusty-IDD: code intelligence / symbol graph.
- flexnetos_runner: execution-plane mechanics.
- ATC/model backends: actual model/provider execution.
