# Plan — Telegram and Slack bridge runtime health

## Goal

Make both optional bot bridges reliable, observable, and independently routable
without weakening the default-off surfaces boundary, the no-shell contract, or
SQLite/libSQL parity. Tests are written RED first; live credentials and live
chat workspaces are never required by the test suite.

## Workstreams

### Explicit configuration and routing

- Resolve bounded, control-free tokens and external conversation identifiers at
  the use seam; require every value needed for two-way operation.
- Give Telegram and Slack distinct identities and explicit internal recipients.
  Preserve the legacy shared identity only when it is unambiguous; refuse a
  simultaneous identity collision.
- Relay ordinary inbound text from the platform bridge identity to the configured
  internal recipient, retaining bounded external attribution and a stable event
  idempotency key.
- Filter Telegram updates to the configured chat and validate exact bot-command
  addressing when a bot username is configured.

### Confirmed outbound delivery

- Read bridge inbox rows without marking them. Mark exactly one row read only
  after HTTP status and the platform `ok` result confirm acceptance.
- Add dual-backend single-message read acknowledgement and metadata-only
  `relayed` / `relay_failed` delivery stages.
- Keep a failed or ambiguous delivery unread for retry. Bound outbound text to
  the platform contract on a Unicode boundary and bound every response body.
- Make bot `/inbox` delivery follow the same deferred-ack rule so a failed reply
  cannot consume mail.

### Durable inbound progress and ownership

- Persist token-free per-platform state: identity, recipient, cursor, owner,
  heartbeat, last poll/success/delivery timestamps, and classified last error.
- Atomically claim one live runtime per platform identity and fence state updates
  to that owner; stale ownership is recoverable.
- Advance Telegram only after each update is durably handled. Persist stable
  update-id idempotency keys.
- Bootstrap Slack only after a valid response; compare timestamps exactly,
  process oldest-first, follow cursor pagination without dropping a gap, and
  persist progress so restart replay is idempotent.

### Testable transport and status

- Refactor each blocking loop around a bounded single-iteration transport seam.
  Fake transports cover transport failure, HTTP failure, API-level failure,
  malformed/oversized bodies, pagination, and retry without external access.
- Add read-only `--status` and bounded `--check` modes. Surface configured,
  ready, active, stale, and degraded states without tokens in CLI doctor, MCP
  doctor, dashboard JSON, or HTML.
- Make the command ledger, config template, README, architecture/parity docs,
  testing guidance, and changelog match the tested behavior.

## RED gates

- Store, SQLite and libSQL: one-row acknowledgement is recipient-scoped;
  bridge-state claim/fencing/cursor updates are atomic and migration-safe.
- Telegram: wrong-chat updates are ignored; local failure does not advance the
  cursor; retry deduplicates; failed post/reply leaves mail unread; confirmed
  post marks only its row; HTTP/API/oversize failures remain bounded.
- Slack: failed bootstrap stays bootstrap; exact timestamp ordering, oldest-first
  processing, cursor pagination, restart replay, retry idempotency, and deferred
  acknowledgement hold.
- Configuration/status: missing or invalid token/conversation/identity/recipient
  is not ready; two platforms cannot own one inbox; no token byte appears in any
  error, Debug, status, doctor, dashboard, or delivery trace.
- Run every store/state/bridge black-box test on default SQLite and
  `--no-default-features --features "libsql surfaces"`.

## Final gates

- Full default, SQLite surfaces/maximal, libSQL surfaces/maximal test matrices.
- Strict clippy for both supported backends and maximal feature graphs.
- Formatting, diff hygiene, documentation freshness, dependency trees,
  supply-chain audit, independent verifier, guardian review, and remote CI.

## Delivery

Branch `fix/bridge-health`, based on merged `origin/develop` at `b0ccd22`
(Cycle B PR #180). Deliver as an isolated PR before command/target-smoke work.
