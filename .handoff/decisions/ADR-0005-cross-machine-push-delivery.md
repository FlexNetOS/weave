# ADR-0005 — Cross-machine PUSH delivery (consent-based, over the bearer-gated serve endpoint)

- **Status:** accepted — 2026-06-17 (owner pre-approved restoring cross-machine push)
- **Plane:** agent-mesh
- **Owner:** drdave
- **Scope:** weave Tier-2 federation — add an **opt-in, consent-based cross-machine
  PUSH** path so a sender A on machine 1 can deliver an Intent to recipient B on
  machine 2 and have **B's pane light up without B polling**. Receive path is a new
  bearer-gated HTTP action behind the existing `--features surfaces` `serve`
  endpoint; send path is a CLI verb / catalog op. No change to weave's core
  invariants or default binary.
- **Supersedes/relates:** ARCHITECTURE §10 (Tier-2 request-pull "Option C" — PUSH is
  its dual, not its replacement); §8 + §10 (the "cross-machine is a non-goal for now"
  statement, **revisited here**); ADR-0004 (the `--features surfaces` `std::net` HTTP
  transport + WL-022 bearer auth, reused); ADR-0003 (token-light — PUSH adds **no**
  standing MCP tool); WL-052a (the bearer-gated `POST /api` action surface, the
  receive seam extended here); the `sign` feature (verify-on-commit, reused verbatim).

## Context

weave's whole reason to exist is **push** — the sender injects a live nudge into the
recipient's pane so the recipient does not poll. But that push is **local-mesh only**:
it works because every mux CLI can address any pane *on the same machine*. The moment
the recipient lives in a different store on a different machine, weave falls back to
**Tier-2 cross-store delivery (§10)**, which is **pull-initiated**: A deposits a
signed `Intent` in **A's own** `outbox`; B — and only B, on B's own schedule —
opens A read-only on its next drain (`prompt`/`stop` hook, `weave watch`, `weave
pull`), commits the intent into its own inbox, and (with `inject_pulled`) nudges its
own pane. The cross-store consent nudge therefore fires **only when B next polls**.

The absorption audit flagged this precisely: repowire had a real cross-machine push
(via its hosted-relay daemon / outbound WSS); weave's parity matrix marks
cross-machine **"❌ (non-goal for now)"** (ARCHITECTURE §8) and the hosted-relay row
**"🧭 SUPERSEDED"** by Tier-2 *pull* (REPOWIRE-PARITY §8). The capability that is
genuinely absent is **latency-free remote delivery**: A → B where B's pane lights up
*without B having to poll first*. Tier-2 pull has the data path and all the safety
machinery; what it lacks is an A-initiated trigger.

The reason cross-machine push was made a non-goal is **not** that the data model
can't do it — it's that the obvious implementations break weave's non-negotiables: a
relay process violates no-daemon; A writing B's store violates owner-only-writes; an
always-on inbound listener violates no-daemon-by-default and adds standing attack
surface. This ADR's job is to show that **push is achievable without breaking any of
them** — because the receive side is exactly a Tier-2 pull-commit, just *triggered by
A's HTTP request instead of B's poll*, and the trigger rides a surface B already had
to opt into (`weave serve`).

## Decision (proposed)

**Cross-machine push = B accepting a signed Intent over the bearer-gated `serve`
endpoint and committing it to B's OWN store** — the Tier-2 pull-commit pipeline,
A-initiated. Concretely:

1. **Receive = a new write action on the existing `--features surfaces` HTTP
   surface.** Extend the WL-052a `POST /api` action set (the same surface that
   already routes mutating ops through the shared `dispatch_request`) with a
   `weave_push` action — `{from, to, subject?, body, sig, to_host?, idempotency_key?,
   trace_id?, priority?, ttl?}` — i.e. the wire form of an `Intent`. No new socket,
   no new listener, no new always-on process. B gains a receive path **only** by
   running `weave serve` (the existing surface), which is already opt-in and behind
   `--features surfaces` (default OFF).

2. **The receive handler is the Tier-2 commit pipeline, verbatim.** The handler
   deserializes the body into an `Intent`, then runs it through the **existing**
   `store::commit_pulled` path with the receiver's `VerifyPolicy`: re-validate
   (`check_ident`/`check_body`/`to == me`), **verify the signature**
   (`verify_pulled_intent` → `sign::verify_intent`, the `sign`-feature decision
   table unchanged), commit into **B's own** inbox via `Store::send` (B assigns
   id/ts), then fire the **caller-side** consent nudge into **B's own** pane via the
   existing `nudge_pulled` seam — gated by `inject_pulled` + `inject_allowed_from`
   exactly as a pull is. **A never writes B's store; A never touches B's pane.**

3. **Send = a CLI verb / catalog op, not a standing tool.** A pushes via `weave push
   --to <name> --host <url:port> [--token …]` (and the equivalent meta-tool catalog
   op `weave_push` reached through `call`), which signs the canonical `(from,to,body)`
   if A has a key (reusing `sign_intent_if_keyed`) and POSTs the Intent JSON to B's
   `/api` with `Authorization: Bearer <token>`. This is the **dual of `weave send
   --to-store`**: instead of depositing the intent in A's outbox for B to pull, A
   delivers it to B's endpoint for B to commit. Both end in the identical
   `commit_pulled`.

4. **Bind posture is an explicit operator opt-in.** Cross-machine means the listener
   must bind beyond `127.0.0.1`. `serve_http`/`serve_dashboard` currently hard-code
   `127.0.0.1`; this ADR adds a `--bind <addr>` flag (default unchanged: `127.0.0.1`)
   so the operator must *deliberately* expose B (e.g. `--bind 0.0.0.0` or a specific
   interface/Tailscale address). A non-loopback bind **requires a non-empty bearer
   token** (refuse to start an open listener on a routable address — fail-closed).

5. **Default builds gain nothing.** PUSH lives entirely behind `--features surfaces`
   (the receive action) + the `sign` feature (the verification it relies on for an
   unforgeable `from`). The default `cargo build` is byte-identical; no standing MCP
   tool is added (ADR-0003); no new crate, no new heavyweight dep (the surface is the
   existing `std::net` transport).

## How each non-negotiable is preserved

- **owner-only-writes.** A POSTs; **B's own handler** does every write — the inbox
  commit and the cursor/dedup are local to B via `Store::send`/`commit_pulled`. A
  opens no handle on B's store and has no injection path into B. This is *structurally*
  the §10 invariant: the receive handler is the same `commit_pulled` a pull uses, so
  "only a store's owner writes it" holds across the network exactly as it holds across
  local files. The only inversion is **who triggers** the commit (A's request vs B's
  poll) — not **who performs** it.
- **no-daemon-by-default.** No relay, no broker process, no always-on listener on the
  default path. B has a receive path **iff** B is running `weave serve` (already an
  explicit, `--features surfaces`-gated opt-in). A default build/session is
  byte-identical and gains no listener. The DB is still the broker; `serve` is a
  *surface*, not a daemon weave starts on its own.
- **signed-identity / verify-on-commit.** The inbound Intent carries `sig`; the
  handler runs the **unchanged** `verify_pulled_intent` decision table under B's
  `VerifyPolicy` (tri-state `strict_verify_override`, trust set, revocation). A
  forged/tampered signature is rejected before any write (the canonical
  `(from,to,body)` binding makes a spoofed `from` unforgeable); a revoked-key
  signature is rejected unconditionally (R1). Bearer auth gates *transport*;
  the signature gates *identity* — defense in depth.
- **token-light.** PUSH adds **no standing MCP tool**. Receive is an HTTP action on
  the existing `/api` surface (zero standing tokens); send is a CLI verb plus a
  catalog op reached through the `weave` meta-tool's `call` mode (ADR-0003). The
  `MAX_STANDING_TOOLS_BYTES` budget test is unaffected.
- **dual-backend + no new crate.** The receive handler calls only the existing
  `Store` methods (`Store::send`, the keys lookups) through `commit_pulled`, which is
  already mirrored on both backends; no schema change, no new trait method. The
  transport is the existing hand-rolled `std::net` HTTP. No new crate, no async
  runtime, no heavyweight dep.

## Security model

- **Authentication = bearer + signature, layered.** The `serve` endpoint is
  bearer-gated (WL-022) exactly like every other weave HTTP surface — an
  unauthenticated POST is `401`. Above that, the Intent's ed25519 signature
  authenticates the *sender identity* (`verify_pulled_intent`): even a caller who
  holds the bearer token cannot forge a `from` that B trusts under a configured trust
  set + strict policy. Bearer compromise alone cannot impersonate a signed peer.
- **Bind posture / exposure.** Default `--bind 127.0.0.1` (unchanged — local-only,
  the only safe default). Cross-machine requires the operator to *opt in* to a
  routable bind; a non-loopback bind **must** carry a non-empty token or `serve`
  refuses to start (no open listener on a routable address). Recommended deployment
  is a private overlay (Tailscale / WireGuard / SSH tunnel) so the listener is never
  on the public internet — documented as the contract, mirroring §10's
  "server-enforced read-only token" deployment guidance.
- **SSRF / exposure considerations.** The **send** side targets an operator-supplied
  `--host <url:port>`; this is an outbound request weave makes on the operator's
  behalf, so the send path is **not** auto-fed from untrusted message bodies — it is
  an explicit CLI/catalog op. (We deliberately do *not* let an inbound message cause
  weave to POST to an arbitrary host.) The **receive** side accepts a body capped at
  `MAX_BODY` and re-validated at commit; idents are `check_ident`-bounded; the body
  never reaches a shell (no-shell invariant) and is committed only via parameterized
  `Store::send`. Rate/size is bounded by the existing `Content-Length` read + body
  caps. The threat surface added is exactly "an authenticated, signed peer can cause
  one capped, validated inbox row + one capped pane nudge on B" — the same residual
  risk §10 already documents for `inject_pulled` (accepting delivery grants a
  live-pane ping), now reachable push-style instead of pull-style. `WEAVE_INJECT_PULLED=false`
  disables the nudge; `WEAVE_ALLOW_INJECT_FROM` narrows it; an empty trust set keeps
  delivery advisory exactly as today.
- **Replay / idempotency.** The Intent carries `idempotency_key`; `commit_pulled`
  commits through `Store::send` which dedups on it, so a re-POSTed Intent does not
  double-deliver. (Note: unlike the pull path's per-source `pull_cursor` high-water
  mark, push has no source cursor — idempotency rests on the idempotency key, which
  the send path always populates; see the implementation plan's open decision on a
  synthetic key for keyless pushes.)

## Alternatives considered (rejected)

- **Always-on inbound listener / relay daemon (the repowire model).** Rejected:
  violates no-daemon-by-default and adds a permanent attack surface. weave starts no
  process on its own; the receive path exists only while the operator runs `weave
  serve`, which they already had to opt into.
- **A writes B's store directly (shared remote libSQL, A commits B's inbox).**
  Rejected: violates owner-only-writes — the load-bearing §7 invariant. The whole
  point is that B commits its own rows; A only *delivers a request*.
- **A pushes by writing to a shared Turso replica B reads (no HTTP).** Rejected as
  the *push* mechanism: that is just Tier-2 remote **pull** (already shipped, §10
  "Remote sources") and still requires B to poll the replica — it does not light B's
  pane without B polling, which is the exact capability this ADR adds.
- **A new standing `weave_push` MCP tool.** Rejected: standing-token cost (ADR-0003).
  Receive is an HTTP action; send is a CLI verb + catalog op behind the meta-tool.
- **Push the live nudge from A directly into B's pane over SSH/mux-over-network.**
  Rejected: A would be driving B's mux (owner-only-writes violation in spirit), needs
  ambient remote shell access, and bypasses verify-on-commit. The pane nudge must be
  fired by B, caller-side, after B's own verified commit.

## Consequences

- Closes the last genuine cross-machine gap **without a daemon, without owner-only-
  writes violation, and with zero default-build weight**: REPOWIRE-PARITY §8
  hosted-relay row can move from "🧭 SUPERSEDED (pull-only)" to a true ✅ once shipped,
  and ARCHITECTURE §8's "cross-machine ❌ non-goal" is retired in favor of "✅ consent-
  based push (opt-in, `--features surfaces` + `sign`)".
- Push and pull become **duals over one commit pipeline** (`commit_pulled`): pull is
  B-initiated, push is A-initiated, both verify-then-commit-to-own-store. No second
  delivery code path to keep in sync.
- A **new exposed write surface** enters the threat model (an authenticated, signed,
  capped inbox-commit endpoint). It is gated three ways — `--features surfaces` build,
  `weave serve` runtime opt-in, bearer token — plus signature verification and the
  explicit non-loopback-requires-token fail-closed. Documented residual risk: a
  trusted signed peer can cause one capped inbox row + one capped pane nudge on B.
- CI gains push-path coverage on the existing `surfaces` job (both backends), incl.
  the forged-signature-rejected and bearer-missing-403 cases; the default build job
  stays byte-identical.

## Research / Cross-references

- ARCHITECTURE §10 — Tier-2 cross-store delivery (the request-pull "Option C",
  `outbox`/`pull_cursor`/`keys`, owner-only-writes, `commit_pulled`,
  `verify_pulled_intent`, the `inject_pulled` consent toggle, `strict_verify_override`
  tri-state). PUSH is the A-initiated dual of this exact pipeline.
- `weave-mcp/src/http.rs` — `serve_http` / `handle_connection` (WL-022 bearer auth)
  and `handle_dashboard_connection`'s WL-052a `POST /api` write action surface (the
  receive seam extended here); `127.0.0.1` bind (the `--bind` opt-in target).
- `weave-core/src/store.rs` — `commit_pulled`, `verify_pulled_intent`, `VerifyPolicy`,
  `Pulled.committed_sources` (the commit + verify machinery reused unchanged).
- `weave-core/src/sign.rs` — `verify_intent` / `sign_intent` / `canonical_message`
  (the unforgeable-`from` binding; verify-on-commit decision table).
- `weave-core/src/model.rs` — `Intent { id, ts, to, to_host, from, subject, body, sig,
  idempotency_key, trace_id, priority, ttl }` (the push wire form).
- `weave-mcp/src/mcp.rs` — `tool_send`'s `to_store` Tier-2 enqueue branch + `nudge_pulled`
  (the caller-side consent-nudge seam) + `dispatch_request` (the shared handler the
  `/api` surface routes through); `sign_intent_if_keyed`.
- ADR-0004 (the `--features surfaces` `std::net` HTTP transport, reused) and ADR-0003
  (token-light: CLI verb + catalog op, not a standing tool).
- repowire's hosted-relay outbound-WSS push — the capability closed here Rust-native,
  daemon-free, owner-only-writes-preserving.
