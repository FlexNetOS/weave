# Plan: WL-031 + WL-032 — Message priority levels & per-peer contact policies

## WL-031: Message importance / priority levels

### Schema (both backends)
- `messages` table: add `priority TEXT NOT NULL DEFAULT 'normal'`
- `outbox` table: add `priority TEXT NOT NULL DEFAULT 'normal'` (for cross-store intents)
- Migration: guarded `ALTER TABLE ADD COLUMN` for legacy DBs

### Model
- `MessagePriority` enum: `Low`, `Normal`, `High`, `Urgent` (default `Normal`)
- `parse_priority(s: &str) -> MessagePriority`

### Store trait
- `send` and related methods gain `priority: Option<MessagePriority>` param
- `inbox` gains `min_priority: Option<MessagePriority>` filter
- `history` gains `min_priority: Option<MessagePriority>` filter
- `search` gains `min_priority: Option<MessagePriority>` filter

### CLI
- `weave send --priority low|normal|high|urgent`
- `weave notify --priority ...`
- `weave broadcast-notify --priority ...`
- `weave inbox --min-priority ...`
- `weave history --min-priority ...`
- `weave search --min-priority ...`

### MCP
- `priority` param on `weave_send`, `weave_notify`, `weave_broadcast_notify`
- `min_priority` param on `weave_inbox`, `weave_history`, `weave_search`

## WL-032: Per-peer contact policies

### Schema (both backends)
- `peers` table: add `contact_policy TEXT NOT NULL DEFAULT 'open'`
- Migration: guarded `ALTER TABLE ADD COLUMN`

### Model
- `ContactPolicy` enum: `Open`, `Auto`, `ContactsOnly`, `BlockAll` (default `Open`)
- `parse_contact_policy(s: &str) -> ContactPolicy`

### Store trait
- `set_peer_policy(peer: &str, policy: ContactPolicy) -> Result<bool>`
- `get_peer_policy(peer: &str) -> Result<ContactPolicy>`
- `list_peers` returns policy in the peer view

### Policy enforcement
- `send`: check recipient's policy before creating message
  - `BlockAll` → reject
  - `ContactsOnly` → check if sender is in recipient's contacts (simplified: allow if sender has ever sent to recipient, or use a contacts list)
  - `Auto` / `Open` → allow
- `inbox`: filter out messages from blocked senders at read time

### CLI
- `weave peers --set-policy <peer> --policy open|auto|contacts_only|block_all`
- `weave peers` human output shows policy

### MCP
- `weave_set_peer_policy` tool

## Test layers
- Unit: enum parsing, policy logic
- Integration: send blocked by policy, inbox filtered by priority, policy roundtrip
- Full gate on both backends
