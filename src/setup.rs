//! `weave setup` / `weave uninstall` — wire weave into Claude Code (register the
//! MCP server and merge lifecycle hooks into ~/.claude/settings.json idempotently).
//!
//! Setup is safe to re-run: registering the MCP server and adding hooks never
//! duplicates an existing entry, and we never clobber unrelated hooks (rtk,
//! repowire, …). Uninstall removes only weave's own entries.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

/// The lifecycle hooks weave installs, as (Claude event name, hook argument).
/// The command Claude runs is `<exe> hook <arg>`.
const HOOKS: &[(&str, &str)] = &[
    ("SessionStart", "session"),
    ("UserPromptSubmit", "prompt"),
    ("Stop", "stop"),
];

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Path to the user-scope Claude Code settings file.
fn settings_path() -> PathBuf {
    home().join(".claude").join("settings.json")
}

/// Wire weave into Claude Code: register the MCP server (user scope) and merge
/// our lifecycle hooks into ~/.claude/settings.json.
pub fn run(exe: &str) -> Result<()> {
    register_mcp(exe);
    let added = merge_hooks(exe).context("merging weave hooks into settings.json")?;

    println!("weave setup complete:");
    println!("  exe:      {exe}");
    println!("  settings: {}", settings_path().display());
    println!("  MCP:      weave (user scope) -> {exe} mcp");
    if added.is_empty() {
        println!("  hooks:    already present (no changes)");
    } else {
        println!("  hooks:    added {}", added.join(", "));
    }
    Ok(())
}

/// Reverse [`run`]: remove the MCP registration and weave's own hook entries,
/// leaving every other hook (rtk, repowire, …) intact.
pub fn uninstall() -> Result<()> {
    // Remove the MCP server (best-effort).
    match claude_mcp_remove() {
        Ok(true) => println!("removed MCP server 'weave' (user scope)"),
        Ok(false) => println!("MCP server 'weave' was not registered (or `claude` unavailable)"),
        Err(err) => eprintln!("note: `claude mcp remove` failed: {err}"),
    }

    let removed = prune_hooks().context("removing weave hooks from settings.json")?;
    if removed == 0 {
        println!("no weave hooks found in {}", settings_path().display());
    } else {
        println!(
            "removed {removed} weave hook(s) from {}",
            settings_path().display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MCP registration
// ---------------------------------------------------------------------------

/// Register the weave MCP server at user scope via the `claude` CLI. Best-effort:
/// if `claude` is not on PATH we print a clear note and continue (hooks still get
/// installed, and the user can register manually later).
fn register_mcp(exe: &str) {
    // Remove first so re-running setup updates an existing registration in place.
    let _ = claude_mcp_remove();

    match Command::new("claude")
        .args(["mcp", "add", "weave", "--scope", "user", "--", exe, "mcp"])
        .status()
    {
        Ok(status) if status.success() => {
            println!("registered MCP server 'weave' (user scope)");
        }
        Ok(status) => {
            eprintln!(
                "note: `claude mcp add weave` exited with {status}; you can register manually:"
            );
            eprintln!("      claude mcp add weave --scope user -- {exe} mcp");
        }
        Err(_) => {
            eprintln!("note: `claude` not found on PATH — skipping MCP registration.");
            eprintln!("      once Claude Code is installed, run:");
            eprintln!("      claude mcp add weave --scope user -- {exe} mcp");
        }
    }
}

/// `claude mcp remove weave -s user`, ignoring (and reporting) failures.
/// Returns Ok(true) if the command ran and succeeded, Ok(false) if it ran but
/// failed (e.g. not registered) or `claude` is missing, Err only on spawn errors
/// we cannot classify.
fn claude_mcp_remove() -> Result<bool> {
    match Command::new("claude")
        .args(["mcp", "remove", "weave", "-s", "user"])
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(_) => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// settings.json hook merge / prune
// ---------------------------------------------------------------------------

/// Read settings.json. Returns an empty object ONLY when the file does not exist
/// (or exists but is blank). Any *other* read error (permission denied, EIO, …)
/// is propagated so callers abort WITHOUT overwriting — otherwise a transient
/// read failure on a populated file would let setup truncate it to weave-only
/// hooks, destroying every unrelated hook (rtk, repowire, …). See the BLOCKER fix.
fn read_settings() -> Result<Value> {
    let path = settings_path();
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "reading {} (refusing to continue: overwriting now would clobber \
                     existing hooks)",
                    path.display()
                )
            });
        }
    };
    if s.trim().is_empty() {
        return Ok(json!({}));
    }
    let v: Value =
        serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
    if v.is_object() {
        Ok(v)
    } else {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
}

/// Write settings.json pretty-printed, creating parent dirs as needed. Adds a
/// trailing newline to match conventional editors.
///
/// The write is atomic: we serialize to a temp file in the SAME directory and
/// `rename` it over the target, so a crash or full disk mid-write can never leave
/// the user's settings truncated. Before the first time weave mutates an existing
/// settings file we also drop a one-time `settings.json.weave.bak` snapshot.
fn write_settings(v: &Value) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // One-time backup of the pre-existing file (best-effort, never created twice).
    if path.exists() {
        let bak = path.with_extension("json.weave.bak");
        if !bak.exists() {
            if let Ok(original) = std::fs::read(&path) {
                let _ = std::fs::write(&bak, original);
            }
        }
    }

    let mut out = serde_json::to_string_pretty(v)?;
    out.push('\n');

    // Write to a sibling temp file then rename over the target (atomic on POSIX).
    let tmp = path.with_extension("json.weave.tmp");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;
    Ok(())
}

/// The command string weave installs for a given hook argument.
fn hook_command(exe: &str, arg: &str) -> String {
    format!("{exe} hook {arg}")
}

/// True if this command string is one of weave's own hooks, i.e. exactly
/// `<exe> hook <session|prompt|stop>` where `<exe>`'s basename is `weave`.
///
/// We match the precise installed shape (not a loose `contains("weave hook")`) so
/// we never delete an unrelated user hook such as `/usr/bin/myweave hook-runner`
/// or `echo 'run weave hook' && other`. The `weave` token must be a whole path
/// component: the text before it is empty (bare `weave hook stop`) or ends with a
/// path separator (`/home/u/.cargo/bin/weave hook stop`).
fn is_weave_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    ["session", "prompt", "stop"].iter().any(|event| {
        let suffix = format!("weave hook {event}");
        match cmd.strip_suffix(&suffix) {
            Some(prefix) => prefix.is_empty() || prefix.ends_with('/'),
            None => false,
        }
    })
}

/// Merge weave's lifecycle hooks into settings.json idempotently. Returns the
/// list of Claude event names that were newly added (empty if all already
/// present).
fn merge_hooks(exe: &str) -> Result<Vec<String>> {
    let mut settings = read_settings()?;

    // Ensure `hooks` is an object we can index into.
    let root = settings
        .as_object_mut()
        .context("settings root is not an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        anyhow::bail!("settings.json `hooks` is not an object");
    }
    let hooks = hooks.as_object_mut().unwrap();

    let mut added = Vec::new();

    for (event, arg) in HOOKS {
        let command = hook_command(exe, arg);

        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !entries.is_array() {
            anyhow::bail!("settings.json hooks.{event} is not an array");
        }
        let entries = entries.as_array_mut().unwrap();

        // Idempotency keyed on the same predicate uninstall uses (`is_weave_command`),
        // NOT exact string equality. If an existing weave hook for this event is
        // present — even one pointing at a different/old exe path (cargo run vs
        // ~/.cargo/bin, a moved binary, a symlink) — we refresh it in place to the
        // current command instead of appending a SECOND entry. This prevents the
        // duplicate-hook bug and auto-heals stale paths.
        if let Some(existing) = find_weave_command_mut(entries) {
            if existing.as_str() == Some(command.as_str()) {
                continue; // already correct — no change.
            }
            *existing = json!(command); // heal a stale path in place.
            added.push(format!("{event} (updated)"));
            continue;
        }

        entries.push(json!({
            "matcher": "",
            "hooks": [ { "type": "command", "command": command } ]
        }));
        added.push((*event).to_string());
    }

    write_settings(&settings)?;
    Ok(added)
}

/// Find the first `command` value under `entries` that is one of weave's hooks,
/// returning a mutable handle so it can be refreshed in place. Used to make setup
/// idempotent across different exe paths (matching uninstall's substring rule).
fn find_weave_command_mut(entries: &mut [Value]) -> Option<&mut Value> {
    for entry in entries.iter_mut() {
        if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
            for h in inner.iter_mut() {
                let is_weave = h
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(is_weave_command)
                    .unwrap_or(false);
                if is_weave {
                    return h.get_mut("command");
                }
            }
        }
    }
    None
}

/// Remove every weave hook entry from settings.json, leaving all other hooks
/// untouched. Returns the number of inner `command` hooks removed.
fn prune_hooks() -> Result<usize> {
    let path = settings_path();
    // Nothing to do if there's no settings file yet.
    if !path.exists() {
        return Ok(0);
    }

    let mut settings = read_settings()?;
    let mut removed = 0usize;

    let Some(root) = settings.as_object_mut() else {
        return Ok(0);
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(0);
    };

    // Collect event keys up-front to avoid borrow issues while mutating.
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };

        for entry in entries.iter_mut() {
            if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|h| {
                    !h.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_weave_command)
                        .unwrap_or(false)
                });
                removed += before - inner.len();
            }
        }

        // Drop entries left with an empty inner `hooks` list (i.e. they only
        // ever held a weave command), then drop the event if fully emptied.
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|inner| !inner.is_empty())
                .unwrap_or(true)
        });
    }

    // Remove now-empty event arrays so the file stays tidy.
    let empty_events: Vec<String> = hooks
        .iter()
        .filter(|(_, v)| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();
    for k in empty_events {
        hooks.remove(&k);
    }

    if removed > 0 {
        write_settings(&settings)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::is_weave_command;

    #[test]
    fn matches_only_real_weave_hooks() {
        assert!(is_weave_command("weave hook session"));
        assert!(is_weave_command("/home/u/.cargo/bin/weave hook stop"));
        assert!(is_weave_command("/usr/local/bin/weave hook prompt"));
        // must NOT match look-alikes
        assert!(!is_weave_command("/usr/bin/myweave hook session"));
        assert!(!is_weave_command("echo 'about to run weave hook' && other"));
        assert!(!is_weave_command("weave hook notification")); // not an installed event
        assert!(!is_weave_command("weave mcp"));
    }
}
