# WL-019 Implementation Plan: Co-orchestrator Support

## Goal
Allow multiple live orchestrators to coexist in the same circle for resilience against rate limits or credit caps. Non-force claims are additive (become a co-orchestrator); force claims still steal (demote all others).

## Current State
- `claim_orchestrator_role`: non-force claim is REFUSED if a live orchestrator exists; force claim demotes all existing orchestrators
- `orchestrator_status`: returns a SINGLE `holder` (the most-recently-seen live orchestrator)
- `OrchestratorStatus`: has `holder: Option<Peer>`

## Changes

### 1. Model (`model.rs`)
- Change `OrchestratorStatus.holder: Option<Peer>` → `holders: Vec<Peer>`
- `present` stays (true if any live orchestrator exists)

### 2. Store trait + both backends
- `claim_orchestrator_role`: remove the "refuse if live orchestrator exists" check for non-force claims. Non-force claims always succeed and do NOT demote existing orchestrators. Force claims still demote all existing orchestrators.
- `orchestrator_status`: collect ALL live orchestrators into `holders`, not just the most recent one
- Update `OrchestratorStatus` construction

### 3. CLI (`main.rs`)
- `orchestrator status`: print all live orchestrators (comma-separated or multi-line)
- `orchestrator claim`: success message updated for co-orchestrator

### 4. MCP (`mcp.rs`)
- `weave_orchestrator_status`: JSON returns `holders` array instead of single `holder`
- `weave_orchestrator_claim`: non-force no longer returns "refused", always returns success unless error

### 5. Tests
- Update store unit tests for co-orchestrator behavior
- Update integration tests for CLI/MCP
- Add tests: two orchestrators coexist, force still demotes all, status lists all live ones

### 6. Backward compat
- `OrchestratorStatus` JSON changes from `holder` to `holders` — this is a breaking API change for consumers. Add `#[serde(rename = "holder")]`? No, better to just change it and update consumers. The consumers are CLI and MCP which we control.

## Single-cycle scope
- No schema changes (uses existing `role` column)
- No new dependencies
- Focused: change claim logic, status query, status struct, CLI/MCP output, tests
