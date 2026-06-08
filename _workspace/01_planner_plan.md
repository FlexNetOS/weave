# WL-020 Implementation Plan: GitHub Review Queue Integration

## Goal
Track PR review state across peers via `review_queue`, `mark_reviewed(pr_url)` through CLI and MCP tools (repowire parity).

## Current State
- weave has no concept of PR review queue
- No store tables or models for tracking external PR state
- No CLI subcommands or MCP tools for review management

## Changes

### 1. Model (`model.rs`)
- New `ReviewItem` struct: `id`, `pr_url`, `title`, `author`, `repo`, `state` (open/merged/closed), `review_requested_at`, `reviewed_at`, `reviewed_by`, `created_at`
- New `ReviewQueueFilter` enum: `All`, `Open`, `Pending` (unreviewed), `Reviewed`

### 2. Store trait + both backends
- `review_queue(&self, filter: ReviewQueueFilter, limit: usize) -> Result<Vec<ReviewItem>>`
- `add_review_item(&self, item: &ReviewItem) -> Result<()>`
- `mark_reviewed(&self, id: &str, reviewer: &str) -> Result<()>`
- `remove_review_item(&self, id: &str) -> Result<()>`
- Schema migration: new `reviews` table with columns matching `ReviewItem`

### 3. CLI (`main.rs`)
- `weave review queue [--filter open|pending|reviewed] [--limit N]` — list review items
- `weave review add <pr_url> [--title <title>] [--author <author>] [--repo <repo>]` — add item
- `weave review mark <id>` — mark as reviewed (self as reviewer)
- `weave review remove <id>` — remove item

### 4. MCP (`mcp.rs`)
- `weave_review_queue(filter?, limit?)` — returns JSON array of ReviewItems
- `weave_review_add(pr_url, title?, author?, repo?)` — adds item
- `weave_review_mark(id)` — marks reviewed
- `weave_review_remove(id)` — removes item

### 5. Tests
- Unit tests for model validation (id format, url format, cap enforcement)
- Store roundtrip tests for both backends (add, list, mark, remove)
- CLI integration tests for all subcommands
- MCP tool tests for JSON shapes and error paths
- Security tests: cap enforcement on title/author/repo, URL validation rejects non-GitHub URLs
