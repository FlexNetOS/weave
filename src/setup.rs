//! `weave setup` / `weave uninstall` — wire weave into Claude Code (register the
//! MCP server and merge lifecycle hooks into ~/.claude/settings.json idempotently).
//!
//! NOTE: stub — full implementation lands via the setup task. Kept compiling so
//! the CLI surface is stable.

use anyhow::Result;

pub fn run(_exe: &str) -> Result<()> {
    println!("weave setup: not yet implemented (stub)");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    println!("weave uninstall: not yet implemented (stub)");
    Ok(())
}
