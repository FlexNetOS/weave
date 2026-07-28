# weave Security Model

This document describes weave's security posture: the trust boundary it operates
within, the threat model it defends against, the concrete hardening that is
implemented today, and — honestly — the residual risks that are *not* yet closed.

It complements [`ARCHITECTURE.md`](../ARCHITECTURE.md) §7 (Threat model) with the
exact code-level guarantees and a candid residual-risk register. Where this
document and the code disagree, the code wins; please file an issue.

> **One-line summary:** weave is a single-user tool whose default mesh is local.
> It assumes each participating machine's operator is trusted and hardens the
> boundaries where peer/provider text is stored, rendered, typed into a session,
> or delivered through an explicitly enabled network surface.

---

## 1. Trust boundary

weave runs **locally** and trusts the operator of the machine. Its store is a
local SQLite (or local libSQL) file owned by that user; every session that can
read that file is, by construction, the same Unix user. The default local mesh
has no routable listener or peer authentication and speaks MCP over stdio; those
are explicit non-goals for the local mesh (see §6). Optional features expand
that boundary deliberately: remote libSQL connects to its configured service,
the `llm` feature can send summarization requests to a configured provider, and
the human-surface dashboard can bind an operator-selected listener, and a
separately credentialed push-only route can commit remote delivery intents.

The one privilege a peer *does* hold is sharp and worth stating plainly:

> **A registered peer can ping your pane.** When session B sends you a message and
> your session A is a registered *injectable* peer (you are running inside a
> supported multiplexer and registered via the `session` hook), weave types a
> line of text — by default the message body — directly into A's live input line
> and submits it. That keystroke stream originates from another agent's content.

This is the core trust transition weave is built around. The keystrokes are
*data typed into a prompt*, never a shell command (see §3), so the worst direct
outcome is unwanted text appearing in your pane. But because the recipient is an
LLM agent, that text is also a **prompt-injection surface**: a hostile body can
attempt to instruct the receiving agent. weave's stance on this is covered in
§3 (what is hardened) and §5 (what is not).

Local session identity is **advisory**. Session names (`from`, `to`, `me`) are free
strings; weave does not defend one same-user local session against another
impersonating it. Cross-machine pull/push can optionally bind sender identity with
ed25519 signatures and a receiver trust policy. Unsigned senders remain advisory
unless strict/trust configuration requires verification.

---

## 2. Threat model

| Adversary | In scope? | Notes |
|---|---|---|
| The machine operator | **No** | Trusted; owns the store and every session. |
| A peer session sending hostile *message content* | **Yes** | Primary threat: flag/shell injection into the mux CLI, control-char abuse of the recipient pane, resource exhaustion, prompt injection. |
| A peer poisoning its own *registration* (mux/target id) | **Yes** | A target id is captured from the recipient's environment at register time and is therefore attacker-influenceable; see target-id validation (§3). |
| Another **local Unix user** reading the store | **Partially** | Mitigated at-rest by `0600`/`0700` permissions (§4), but anyone who is already the same user, or root, can read it. |
| A **network** attacker | **Conditional** | The default local mesh accepts no routable inbound connection. Opt-in remote libSQL, LLM provider calls, and human surfaces add the explicitly configured network boundaries documented below. |
| A malicious **dependency** in the build | **Accepted tradeoff** | See supply-chain residual risk (§5). |

The core security focus is therefore **how injected and stored text is handled**.
Optional network features add the explicitly configured boundaries documented
below; cross-user isolation remains outside the local-mesh model.

### Opt-in network receive boundaries

- The general MCP/dashboard operator bearer grants the documented read surface and,
  with `--dangerous`/`--write`, broad mutation authority. Treat it as an admin
  secret and never give it to a push sender.
- Cross-machine delivery uses a distinct `--push-token` accepted only on
  `POST /push`. It must differ from the operator token, cannot read dashboard/MCP
  state, and dispatches only a direct `weave_push`. The receiver revalidates the
  intent and applies its signature trust/revocation policy before committing.
- The built-in listener is plain HTTP. Non-loopback senders require an external TLS
  terminator/reverse proxy exposing only `/push`, or an SSH loopback forward. A
  private overlay address alone does not terminate HTTPS.
- Telegram and Slack bridge commands are conversation-scoped; there is currently no
  external-user allowlist. Every member able to post in the configured chat or
  channel is an operator of that bridge identity, so use a dedicated private
  conversation. `WEAVE_BOT_WRITES=1` grants those members the bridge's write actions.
- Provider acceptance never bypasses local ownership: the receiver process writes
  its own store and any live pane nudge is performed locally under the receiver's
  pull-injection consent settings.
- Telegram `/inbox` advances its provider cursor and acknowledges the exact rendered
  rows in one owner-fenced Store transaction. This prevents split local state, but
  does not claim cross-system exactly-once delivery: a crash after `sendMessage`
  acceptance and before the local commit can repeat the response.

---

## 3. The injector: hardening of the pane-injection path

`src/inject.rs` is the most safety-critical module: it is the only place weave
drives an external program with peer-influenced input. Every guard below is
implemented and unit-tested.

### No shell, ever
Every mux command is spawned with `std::process::Command::new(bin).args(...)` —
an explicit argv vector. weave **never** builds a shell string and never invokes
`sh -c`. A body containing `;`, `$(...)`, backticks, or quotes is just bytes in a
single argv element. There is no command-injection surface.

### Paste-safe submission
Modern agent TUIs (e.g. Claude Code) run in **bracketed-paste** mode, where a
naive Enter after literal text can be swallowed or read as a TUI key (worst case:
cancelling an in-flight tool call). Each backend uses that terminal's paste-safe
submit idiom rather than a bare newline:

- **tmux** — type with `send-keys -l`, then emit the hex `ESC [ 2 0 1 ~`
  bracketed-paste *close* sequence, then a separate `Enter`.
- **zellij** — `action write-chars` for the body, then `action write 13` (CR).
- **kitty** — `@ send-text` for the body, then a second `send-text` carrying CR.
- **wezterm** — `cli send-text --no-paste` (bypasses bracketed paste), CR as a
  separate send.
- **screen** — `-X stuff "<text>\r"` (text + CR as one positional).

### End-of-options (`--`) guards
A body that *looks* like a flag (`--help`, `-n`, `--pane-id`, …) must land as
content, never be reparsed as an option to the mux CLI. Every backend that
accepts the body as a positional places an end-of-options `--` immediately before
it (tmux `-l --`, zellij `write-chars --`, kitty `send-text --`, wezterm
`send-text --`). screen embeds `<body><CR>` as a single positional and does not
reparse `stuff`'s argument as options. This is asserted for every backend in
`leading_dash_body_is_content_not_a_flag`.

### Control-character & length sanitization (live nudge)
Before the body is typed live it passes through `sanitize()`:

- interior CR/LF are mapped to spaces (so a multi-line body cannot fragment into
  a premature Enter that submits a partial line and runs the remainder as a
  second command);
- every other control byte (tab, ESC, …) is **dropped** (a stray tab can trigger
  TUI completion; ESC can drive cursor/mode changes);
- runs of whitespace are collapsed;
- the result is capped at `MAX_INJECT_CHARS` (240), truncated on a UTF-8
  codepoint boundary with an ellipsis.

An empty or whitespace-only result yields **no commands at all**, so weave never
fires a bare Enter into a recipient's pane. The authoritative full copy always
arrives via the store on the recipient's next hook drain, so the cap loses no
content. (Scope note: this sanitization protects the *live keystroke* path. It is
**not** applied to the body as re-rendered from the store on the next hook drain —
see §5.)

A `Nudge::Nudge` mode is available that injects only a fixed, content-free ping
(`[weave] new message — check your inbox`) instead of the body, keeping hostile
or noisy content out of a busy pane entirely while still waking the recipient.

### Target-id validation
A target id arrives from the recipient's environment at register time and is
attacker-influenceable. `id_valid(mux, id)` accepts only each mux's expected id
shape before weave will drive that mux, and `inject_mode` **refuses to inject**
otherwise:

- **tmux** — `%<digits>` (the `$TMUX_PANE` shape);
- **zellij** — a session name of `[A-Za-z0-9_-]`, ≤ 64 chars;
- **kitty / wezterm** — all-digit window/pane id;
- **screen** — `<pid>.<tty>.<host>` shape;
- bounded ≤ 128 chars; whitespace and option-smuggling characters rejected.

This blocks a crafted id (e.g. `%3; rm -rf /`, `--listen-on=evil`, `a b`) from
redirecting keystrokes to an arbitrary pane or smuggling extra arguments, even
though no shell is involved. Asserted in `id_valid_accepts_real_rejects_malicious`.

### Trusted executable resolution
Every mux, spawn, post-send hook, Obscura, and job-runner launch first resolves
`argv[0]` without a shell. A bare name must be exactly one normal path component
and is searched only in the fixed trusted roots. An absolute path is accepted
only when its canonicalized direct parent is exactly one of those roots. Empty or
relative `WEAVE_MUX_DIR`/`HOME` roots, path-shaped relative names, deeper
descendants, non-files, and non-executable candidates are ignored. Ambient
`$PATH` therefore cannot redirect these launch surfaces.

The Obscura browser child is additionally started with a scrubbed environment:
only `PATH` and a validated absolute `TMPDIR` are retained for runtime
compatibility. Ambient `OBSCURA_*`, proxy, certificate, logging, and unrelated
credential variables therefore cannot change its behaviour or enter the child.
Weave maps `obscura_allow_internal=true` to
Obscura's explicit per-process private-network flag; without that policy opt-in,
both Weave's URL gate and Obscura's redirect/subresource guard remain closed.

### Liveness pre-check (advisory, fail-open)
Before typing, `target_alive()` runs a cheap read-only probe (`tmux has-session`,
`zellij list-sessions`, `wezterm cli list`, `kitten @ ls`) so weave does not type
into a pane that has demonstrably gone away. It is **advisory and fails open**:
only a confident "absent" skips injection; a missing binary, probe error, or
timeout all return "alive" so a delivery is never suppressed merely because the
probe was unavailable. The structured capability used by `connect`/doctor is
stricter: a missing/unlaunchable program, non-zero inventory exit, truncated
inventory, or timeout is `transport_unavailable`, never a false `Live` promise.
`connect` itself is read-only: it does not send, queue, mark read, or write inbox
state.

### Subprocess timeout & bounded retry
Every mux subprocess runs under a 5-second wall-clock cap (`run_bounded` /
`run_capture`); on timeout the child is killed and reaped, so a wedged
tmux/zellij server can never hang weave (critical, because the MCP server serves
other sessions on the same thread). Listing output is drained concurrently and
only a fixed prefix is retained. The probe runs in a fresh process group; cleanup
kills that owned group, and the reader has an explicit completion signal so even
a detached helper that inherited stdout cannot extend the wall-clock bound by
withholding EOF. The first (text-typing) command — the only idempotent one, since
on failure nothing has been typed — is retried exactly once after a short
backoff; later submission steps are never retried (the text is already in the
pane, so a re-run could append a duplicate or stray Enter).

### Contained side effect
The worst case of a hostile body on this path is text appearing in another
session's pane (a UX / prompt-injection concern, §5), not code execution. A failed
or impossible injection degrades to next-turn hook delivery; it never crashes the
sender, because the message is **persisted before injection is attempted**.

### Lifecycle-hook identity and input bounds
Lifecycle hooks accept at most 1 MiB from stdin before JSON parsing. Invalid
UTF-8 prefixes, oversized payloads, and invalid/oversized session keys are
discarded before any peer registration or inbox drain. An event may consume
messages or mutate a peer only when it owns an identity through explicit config,
an exact launcher session-key row, or one unique same-host client-PID row. A cwd
basename is a naming hint and remains peek-only outside safe SessionStart
registration; ambiguous PID ownership is also non-consuming. Oversized
PreToolUse payloads emit an explicit `deny`, while the pre-existing malformed
input policy remains a JSON `defer`. One-shot `register`, `attach`, `scan`, and
`sessions --watch` persist only a positive explicit `WEAVE_CLIENT_PID`; otherwise
they use TTL liveness rather than recording the short-lived CLI process.

### Post-send hooks: argv-only, env-only execution (WL-036)
A configured `[[post_send_hook]]` spawns an **operator-authored** external program
after a matching send/ack. It is a new spawn surface, so it is held to the same
discipline as the injector and the WL-047 spawn path:

- **No shell, argv-only.** `run_post_send_hook` is
  `Command::new(&prog_abs).args(&argv[1..]).envs(...)` — never `sh -c`, never a
  string-built command. The `argv` is the **fixed operator vector** from
  `config.toml`; weave **never** substitutes message text into an argv element.
- **`argv[0]` is trusted-program constrained.** It is resolved via
  `resolve_trusted_program(argv[0])` — a bare name resolved against weave's trusted
  dirs, or an absolute path whose canonicalized parent is a trusted dir — so a hook
  cannot launch an arbitrary `$PATH` binary; a `None` resolve bails (logged, send
  unaffected).
- **Message fields reach the child only as env, never argv; the body is never
  exported.** `Command::envs` sets
  `WEAVE_HOOK_{EVENT,SENDER,RECIPIENT,SUBJECT,MESSAGE_ID,PAYLOAD}` (the `PAYLOAD`
  JSON is hand-escaped). The **message body is not in the vector** — no leak into the
  child's `environ` or `ps e`. A hostile subject like `"; rm -rf /"` / `"$(reboot)"`
  is an inert env value: there is no shell on this path and no code substitutes
  message text into an argv element.
- **Input caps.** Each argv element is re-validated (`spawn_arg_ok`: length +
  NUL/control reject), the argv count is bounded (`MAX_SPAWN_ARGS`), and the config
  layer pre-bounds the hook set (`MAX_POST_SEND_HOOKS`, `MAX_HOOK_ARGV`,
  `MAX_HOOK_ARG_LEN`); an empty argv bails.
- **Fault-isolated and bounded.** The wait is bounded by the hook's `timeout_ms`
  (try_wait/kill); a missing/slow/failing/non-zero-exit hook never breaks send — every
  error is caught and logged to **stderr only** (`eprintln!`), never propagated and
  never on the MCP JSON-RPC stdout frame (the MCP path fires hooks *after* the result
  is built).
- **Operational footgun.** A hook must not call back into `weave send`/`notify`/`ack`
  for the same event class or it re-fires in a loop; keep hook programs out-of-band.

### Job runner claim and process bounds
`weave job dispatch` completes deterministic preflight before it mutates a job:
trusted runner resolution, argv cardinality/element checks, agent and timeout
bounds, job-derived environment validation, and a bounded lease snapshot. Both
stores expose one atomic queued-only claim, preventing two dispatchers from
winning the same row. After claim, the external runner receives fixed argv and
bounded environment values, runs in a fresh process group, and has a
`1..=3600`-second deadline. Stdout/stderr drain concurrently with fixed retention
caps; their readers stop independently of EOF after leader exit/timeout, so a
detached descendant cannot hold dispatch open. Result/error JSON is capped, the
owned process tree is cleaned up, and every post-claim launch/capture/recording
error is terminalized with the matching fencing token (or recovered missing
attempt) instead of stranding the row in `running`.

---

## 4. Store layer & at-rest secrecy

`src/store.rs` (and the libSQL backend `src/store_libsql.rs`) enforce:

- **Body cap.** `check_body()` rejects any body over `MAX_BODY` (65,536 bytes)
  *before* it is stored. Peer-supplied bodies are untrusted; an unbounded body is
  a disk + token/RAM denial-of-service once re-rendered into another agent's
  context. Enforced at the store layer so the CLI, MCP, and hook paths are all
  covered, in **both** backends (`store.rs` and `store_libsql.rs:212`).
- **Limit clamp.** `clamp_limit()` bounds every query `LIMIT` to `MAX_LIMIT`
  (10,000) and maps a negative limit to the cap — a negative `LIMIT` means
  *unbounded* in SQLite, so this prevents an accidental or hostile full-table
  scan from MCP/CLI.
- **Session-fanout ceiling.** `sessions()` truncates to `MAX_SESSIONS` (1,000)
  distinct names so a hostile/busy DB cannot turn one `sessions` call into
  thousands of N+1 sub-queries.
- **Parameterized SQL.** All variable values use bound `params!`. The only
  inlined SQL literals are the broadcast aliases, which are compile-time
  constants derived from `BROADCAST` (never user input), so they cannot be an
  injection vector.
- **Identity bounding (MCP).** `bound_ident()` rejects empty/whitespace and caps
  identities at `MAX_IDENT_LEN` (128 chars); subjects at `MAX_SUBJECT_LEN`
  (256). Identities flow into pane targets and nudge text, so an unbounded value
  is both a footgun and a log-spam / RAM vector.
- **Destructive-op gate.** `weave_clear` with `scope:"all"` wipes every session's
  messages and requires an explicit `confirm:true`; the default scope only marks
  the caller's own inbox read.

### Read-back verification for config/hook rewrites (WL-041)

Every operation that **rewrites** a config or hook file re-opens, re-parses, and
verifies the result before reporting success — it never trusts the write blindly
(mirroring the WL-035 backup-archive read-back). `weave setup` confirms its four
lifecycle hooks landed *and* that every pre-existing **foreign** hook (rtk,
repowire, …) survived; `weave uninstall` confirms no weave hook remains *and*
foreign hooks survived; `weave setup --git-hooks` confirms the guard line landed,
the shebang is present on a freshly created hook, and any pre-existing foreign
content was preserved (the install is append-only); `weave restore` confirms the
restored `config.toml` / `settings.json` bytes equal the archived payload and that
settings.json re-parses as a JSON object. A write whose re-read is missing the
intended weave entries — or that lost a foreign hook — fails loudly with a
descriptive error naming the recovery `.bak` (`settings.json.weave.bak`), rather
than silently succeeding on a partial/corrupt write. The read-back is pure file
I/O + `serde_json` (no shell, no second mutation; it never rewrites the file).

### File permissions

- **Database (`0600`).** After open, `harden_permissions()` tightens the on-disk
  DB to owner-only (`0600`) on Unix so message bodies are not group/world
  readable. The libSQL backend mirrors this for a *local* DB file (no-op on the
  remote path). Best-effort: it never breaks startup on a filesystem that does
  not honour Unix permissions. Asserted by `db_file_is_owner_only` and the
  end-to-end `db_file_is_not_world_or_group_readable`.
- **Config (`0600` file, `0700` dir).** `init_config_file()` creates
  `~/.config/weave/` at `0700` and `config.toml` at `0600` (via
  `OpenOptions::create_new` + `mode`), because the file may hold a libSQL auth
  token. The create is atomic against a racing writer and **never overwrites** an
  existing config, so a user's settings and secrets are safe to re-run against.

### Secret handling
`Config` has a hand-written `Debug` impl that **redacts** database/pull tokens,
the LLM API credential, bot tokens, and proxy credentials to `<redacted>`, so
they cannot leak through a `{:?}` in a log line, panic message, or error context.
The config template and docs steer users toward the matching environment
variables over storing credentials on disk.

### Opt-in LLM outbound boundary (`--features llm`)

The default build links no HTTP/TLS client. An `llm` build still makes no LLM
request until both an endpoint and API credential are configured; an absent
value fails before any outbound connection. Once configured, summarization
deliberately sends thread/message text and the API credential (as a bearer
credential) to that external provider. Use an HTTPS endpoint so rustls protects
both in transit; plain HTTP should be limited to a provider on trusted loopback.
Redirects are never followed, so a provider cannot forward the credential or
message text to a second origin or downgrade the configured connection.

The request and response boundaries are deliberately bounded:

- thread summaries use one canonical snapshot of at most 200 messages, independent
  of the CLI display `--limit`, then cap the conversation text embedded in the
  prompt at 16,000 Unicode scalar values;
- the raw response is read through a 64 KiB cap before JSON decoding, and the
  selected summary is capped at 16,000 Unicode scalars;
- rendered/cached summaries collapse whitespace to one paragraph, reject
  non-whitespace controls (including ANSI ESC), and reject empty output;
- provider status, transport, and decode errors omit response bodies, API
  credentials, and endpoint/redirect URLs.

Cached summaries surface only for a still-live root and the current persistent
message generation. Any message insert, update, delete, clear, retention GC, or
expiry sweep invalidates the cache; expiry is swept before cache lookup, and the
post-provider write is conditional on the generation remaining unchanged.
Snapshots containing expiring messages are never cached and are rejected if a
message expires or mutates while the provider is working.

### MCP stdout discipline
The MCP server writes **only** JSON-RPC frames to stdout; all diagnostics go to
stderr. A malformed log line therefore cannot corrupt the protocol stream, and a
single bad input line (invalid UTF-8, bad JSON) is logged and skipped rather than
crashing the server loop.

---

## 5. Residual risks (honest register)

These are known gaps. They are listed so operators can make an informed decision,
not because they are hidden.

### Rendered-body prompt injection is not yet neutralized
The injector's `sanitize()` protects the *live keystroke* path, but the
**authoritative copy** of a message is re-rendered raw from the store on the
recipient's next hook drain (`handle_hook` in `main.rs` prints `m.body`
verbatim) and in the MCP `weave_inbox` output. weave does **not** currently wrap
peer-supplied bodies in an untrusted-data banner, and does **not** strip ANSI /
C1 control sequences from the stored body before it is rendered into the
recipient agent's context. Consequences:

- A hostile body is presented to the receiving LLM agent as ordinary content and
  can attempt **prompt injection** ("ignore your previous instructions…").
- A body containing raw escape sequences can perturb a terminal that displays the
  hook output, since the drain print is not control-stripped.

Mitigation today is purely the trust model (§1): all peers are the same trusted
local user. A defensive **untrusted-data banner + ANSI/control strip on the
rendered body** is the right next hardening step and should be treated as an open
item, not an implemented control. Until then, do not run weave between sessions
you do not trust.

### kitty cross-session remote-control socket is not persisted
A kitty target only answers `kitten @` when kitty was launched with
`--listen-on`, which exports `KITTY_LISTEN_ON`. `detect_target()` captures that
socket for the *current* process, but the peer registry stores only `(mux,
target)` — `Target::from_peer()` sets `socket` empty. So a *cross-session* nudge
to a kitty peer that requires an explicit `--to <socket>` will fall back to
kitty's default control path and may not reach a kitty bound to a custom socket.
This is a delivery-reliability gap (it degrades to next-turn hook delivery), not
a safety hole. Persisting the socket in the `peers` row is the fix.

### `settings.json.weave.bak` permissions
On the first time `weave setup` mutates an existing Claude Code `settings.json`,
it drops a one-time `settings.json.weave.bak` snapshot via `std::fs::write`,
which uses the process umask (commonly `0644`) rather than an explicit `0600`.
The backup mirrors whatever was already in `settings.json`; if that file held
anything sensitive, the `.bak` is created with default — potentially
group/world-readable — permissions. The live DB and weave's own config are
hardened to `0600`; this auxiliary backup is not.

### Build / supply-chain tradeoffs (accepted)
The default build statically links a **bundled SQLite C core** via `rusqlite`'s
`bundled` feature (and the `libsql` backend links its own SQLite C core); the
backends are mutually exclusive precisely because two C cores would collide.
Dependencies are pulled at current resolved versions (`cargo add`), and the
release profile uses `lto = true` + `strip = true`. The accepted tradeoffs:

- Trusting and statically embedding a C SQLite implementation (a larger native
  attack surface than a pure-Rust store).
- A normal Cargo dependency tree (clap, serde, toml, anyhow, optional
  libsql/tokio) without vendoring or a pinned audited lockfile policy in this
  repo.
- `strip = true` removes symbols, which aids size but reduces post-mortem
  debuggability.

The operator accepted these for a single-binary, no-daemon, local-trust tool.
They would warrant revisiting (dependency audit, reproducible builds, pinning) if
weave ever crossed a trust boundary.

### Dependency advisories: continuous audit + a scoped, tracked exception (WL-044)
A `cargo-deny` advisory gate now runs in CI (`audit` job) against the RustSec
database — a gate that did not exist before WL-044. The **default shippable
binary is advisory-clean**: `cargo tree -i rustls-webpki` on default features
matches nothing.

To reproduce the CI advisory posture locally, use the repo-local helper:

```bash
python3 scripts/supply_chain_audit.py
```

It validates `deny.toml`, proves the default graph is free of `rustls-webpki`,
confirms the residual `rustls-webpki 0.102.x` tree is confined to optional
`libsql` TLS, and then runs the same advisory command as CI:
`cargo-deny check advisories`. If `cargo-deny` is not installed, install it with
`cargo install cargo-deny --locked`, or run
`python3 scripts/supply_chain_audit.py --allow-missing-cargo-deny` to check the
repo-local policy/tree invariants while treating the missing binary as a warning.

**Surface reduced (WL-044b):** `libsql` is pulled with
`default-features = false, features = ["core", "remote", "tls"]` — only what
weave uses (`Builder::new_local` for a local file; `Builder::new_remote` for
remote Turso over HTTPS). Dropping the default `replication`/`sync` features
(weave uses no embedded-replica sync) removed their dependency trees — including
the unmaintained **`bincode 1.x` (RUSTSEC-2025-0141), now eliminated** — plus
`tonic`/`tower-http`/etc., with zero capability or test change.

The remaining open advisories are confined to the **`tls` feature's remote-Turso
TLS stack** (which weave genuinely needs for remote HTTPS) and are
**upstream-pinned**, not weave-fixable today:

| Advisory | Crate | Why it can't be fixed yet |
|---|---|---|
| RUSTSEC-2026-0098 / -0099 / -0049 / -0104 | `rustls-webpki 0.102.8` | Fixed in `>=0.103`, which needs `rustls 0.23` / `hyper-rustls 0.27`. `libsql` (incl. `0.10.0-pre` **and git `main`**, both checked) hard-pins `hyper-rustls ^0.25` → `rustls 0.22` → `rustls-webpki 0.102`; the resolver rejects forcing the patched line. |
| RUSTSEC-2025-0134 | `rustls-pemfile` (unmaintained) | Pulled by `libsql`'s `rustls-native-certs` (part of the `tls` feature weave needs). |

These are reachable **only** when `--features libsql` is compiled **and** the
operator configures a remote `libsql_url` (a Turso endpoint they own, over a
TLS handshake they initiate). They are listed — each with a rationale and a
removal trigger — in `deny.toml`'s `[advisories].ignore`; the gate fails on any
advisory **not** in that explicit list. **WL-044b** tracks removing each id the
moment `libsql` adopts the `rustls 0.23` stack. This is an explicit, scoped,
time-bounded exception — carried forward as tracked work, never a blanket silence.

---

## 6. Explicit non-goals

- **Authentication / multi-user isolation.** Session identity is advisory; weave
  does not authenticate peers or defend one local session against another.
- **Default-mesh network exposure.** The default local mesh has no routable
  listener or daemon (the v0.2 presence daemon is opt-in, off by default, and
  uses a `0600` UDS — see
  [`ROADMAP-v0.2.md`](ROADMAP-v0.2.md)). Optional outbound LLM/remote-libSQL
  connections and human surfaces are explicit operator choices, not local-mesh
  peer exposure.
- **Direct remote mux control.** Weave does not expose a network operation that
  drives another host's mux. A verified cross-machine intent may be committed by
  the receiver and then trigger that receiver's existing local, consent-gated nudge.
- **Encryption at rest.** Secrecy relies on Unix file permissions (`0600`), not
  on encrypting the store.

---

## 7. Reporting

weave is a local, single-user tool with the trust model above. If you find an
issue that violates a stated guarantee — a shell-injection path, an unbounded
resource sink, a permissions regression, or a way for stored/injected text to
escape its "data, not code" contract — please open an issue describing the
reproduction and the guarantee it breaks.

## Dependency advisories (Dependabot)

As of this writing GitHub flags 5 advisories, **all transitive dependencies of the
OPTIONAL `libsql` backend** and pinned by `libsql 0.9.30`:

| Severity | Crate | Path |
|---|---|---|
| high / medium / low ×4 | `rustls-webpki` (0.102.8) | `libsql → hyper-rustls 0.25 → rustls 0.22 → rustls-webpki ^0.102` |
| low | `libsql-sqlite3-parser` (0.13.0) | `libsql` |

**Scope / exposure:**
- The **default (sqlite) build pulls none of these** — `cargo tree -e no-dev` shows
  zero `rustls-webpki`. The shipped binary + the RTX-5090 wizard artifact are the
  default build, so they are unaffected.
- `rustls-webpki` is the **remote-TLS** path: it is only reached when the libSQL
  backend is built AND configured with a remote `libsql_url`. Local-file libSQL does
  not use it. The local message mesh (the actual product today) never touches it.

**Why not patched here:** the fixed `rustls-webpki ≥ 0.103.13` requires `rustls 0.23+`,
which requires `hyper-rustls 0.26+`, which requires an upstream `libsql` release that
bumps its TLS stack. `cargo update`/`--precise` cannot satisfy it against `libsql
0.9.30`. Tracked for when libSQL ships a rustls-0.23 build; until then the exposure is
confined to the unused remote-libSQL TLS path.
