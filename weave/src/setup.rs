//! `weave setup` / `weave uninstall` — wire weave into Claude Code (register the
//! MCP server and merge lifecycle hooks into ~/.claude/settings.json idempotently).
//!
//! Setup is safe to re-run: registering the MCP server and adding hooks never
//! duplicates an existing entry, and we never clobber unrelated hooks (rtk,
//! repowire, …). Uninstall removes only weave's own entries.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The lifecycle hooks weave installs, as (Claude event name, hook argument).
/// The command Claude runs is `<exe> hook <arg>`.
const HOOKS: &[(&str, &str)] = &[
    ("SessionStart", "session"),
    ("UserPromptSubmit", "prompt"),
    ("Stop", "wake"),
    ("SubagentStop", "wake"),
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

/// Create/truncate `path` with the given bytes, forcing owner-only `0o600`
/// permissions atomically at open time via `O_CREAT` + `mode`. Used for both the
/// `.bak` snapshot and the `.tmp` staging file: settings.json env blocks can carry
/// secrets (API keys, tokens), so these derived files must never be created
/// world- or group-readable, not even for the brief window before a later
/// `set_permissions`. `OpenOptions::mode` is applied by the kernel only when the
/// file is newly created, so we also explicitly tighten it afterwards in case the
/// path already existed (e.g. a stale `.tmp` from a previous crash).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {} (0o600)", path.display()))?;
    // Defend against a pre-existing file whose mode O_CREAT left untouched.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0o600 {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.flush()
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(())
}

/// Write settings.json pretty-printed, creating parent dirs as needed. Adds a
/// trailing newline to match conventional editors.
///
/// The write is atomic: we serialize to a temp file in the SAME directory and
/// `rename` it over the target, so a crash or full disk mid-write can never leave
/// the user's settings truncated. Before the first time weave mutates an existing
/// settings file we also drop a one-time `settings.json.weave.bak` snapshot.
///
/// Both the `.bak` and `.tmp` files are created with owner-only `0o600`
/// permissions because settings.json env blocks can carry secrets. We capture the
/// original file's mode before the rename and re-apply it to the renamed result so
/// the live settings file keeps whatever permissions the user chose (we don't
/// silently force the live file to 0o600 — only the derived sidecar files).
fn write_settings(v: &Value) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Capture the pre-existing file's mode (if any) so we can preserve it on the
    // renamed result; the .tmp file is created 0o600 and rename keeps the tmp's
    // mode, which would otherwise tighten the live file unexpectedly.
    let original_mode: Option<u32> = std::fs::metadata(&path)
        .ok()
        .map(|m| m.permissions().mode());

    // One-time backup of the pre-existing file (best-effort, never created twice).
    // The snapshot may contain secrets, so write it 0o600.
    if path.exists() {
        let bak = path.with_extension("json.weave.bak");
        if !bak.exists() {
            if let Ok(original) = std::fs::read(&path) {
                let _ = write_private(&bak, &original);
            }
        }
    }

    let mut out = serde_json::to_string_pretty(v)?;
    out.push('\n');

    // Write to a sibling temp file then rename over the target (atomic on POSIX).
    // The temp file is created 0o600 so secrets are never briefly world-readable.
    let tmp = path.with_extension("json.weave.tmp");
    write_private(&tmp, out.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;

    // Restore the original file's permissions on the renamed result. If the file
    // is brand new (no original mode) we leave it at the tmp's 0o600, which is a
    // safe default for a file that may hold secrets.
    if let Some(mode) = original_mode {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}

/// Single-quote a string for safe use as one POSIX shell word, escaping any
/// embedded single quotes via the standard `'\''` idiom. `foo` → `'foo'`,
/// `a'b` → `'a'\''b'`. A path containing a space (e.g. a `cargo run` target under
/// `~/My Projects/`) would otherwise word-split and the hook would silently never
/// run — quoting makes it a single argument.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// The command string weave installs for a given hook argument.
///
/// The exe is single-quoted so a path containing spaces or other shell-special
/// characters is passed as one word; without this an exe like
/// `/home/u/My Projects/weave` would word-split and the hook would never fire.
fn hook_command(exe: &str, arg: &str) -> String {
    format!("{} hook {arg}", shell_single_quote(exe))
}

/// True iff `prefix` (the text that precedes `weave hook <event>` in an UNQUOTED
/// command) names weave's binary via a clean path component, i.e. either:
///   * empty            → bare `weave hook wake`, or
///   * a clean absolute path ending in `/` → `/home/u/.cargo/bin/weave hook wake`.
///
/// We reject anything containing shell operators (`&&`, `;`, `|`) or interior
/// whitespace runs so a crafted hook like `echo x && /weave hook wake` or
/// `: ;/weave hook wake` is never mistaken for ours. A clean absolute path starts
/// with `/`, ends with `/`, and contains no whitespace or shell metacharacters.
fn is_clean_unquoted_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    // Must be an absolute path component boundary.
    if !prefix.starts_with('/') || !prefix.ends_with('/') {
        return false;
    }
    !prefix.chars().any(is_shellish) && !prefix.contains("//")
}

/// Characters that, if present in a path prefix, mean we must NOT treat the
/// command as weave's own (they signal command chaining, word-splitting, or
/// redirection rather than a plain binary path).
fn is_shellish(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '&' | ';'
                | '|'
                | '<'
                | '>'
                | '('
                | ')'
                | '{'
                | '}'
                | '$'
                | '`'
                | '\\'
                | '*'
                | '?'
                | '\''
                | '"'
        )
}

/// True if this command string is one of weave's own hooks, i.e. exactly
/// `<exe> hook <session|prompt|stop|wake>` where `<exe>`'s basename is `weave`.
///
/// Two installed shapes are recognized so uninstall/idempotency keep working
/// across versions:
///   * the current QUOTED form `'<exe>' hook <event>` (T2), and
///   * the legacy UNQUOTED form `<exe> hook <event>` written by older weave.
///
/// We match the precise installed shape (not a loose `contains("weave hook")`) so
/// we never delete an unrelated user hook such as `/usr/bin/myweave hook-runner`
/// or `echo 'run weave hook' && other`. The `weave` token must be a whole path
/// component, and (for the unquoted form) the path prefix must be clean — empty or
/// a shell-operator-free absolute path. See [`is_clean_unquoted_prefix`].
fn is_weave_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    ["session", "prompt", "stop", "wake"].iter().any(|event| {
        // Quoted form: '<exe>' hook <event>, where <exe> ends in (…/)weave.
        let quoted_suffix = format!("weave' hook {event}");
        if let Some(prefix) = cmd.strip_suffix(&quoted_suffix) {
            // `prefix` is everything up to and including the opening quote and the
            // path before `weave`. It must be `'…/` or `'` (bare) with a clean,
            // operator-free absolute path inside the quotes.
            if let Some(inner) = prefix.strip_prefix('\'') {
                // No stray unescaped quote may appear inside the path.
                return is_clean_quoted_inner(inner);
            }
            return false;
        }

        // Legacy unquoted form: <exe> hook <event>.
        let suffix = format!("weave hook {event}");
        match cmd.strip_suffix(&suffix) {
            Some(prefix) => is_clean_unquoted_prefix(prefix),
            None => false,
        }
    })
}

/// Validate the path that appears inside the single quotes of the quoted hook
/// form, i.e. the text between the opening `'` and the literal `weave` token
/// (e.g. `/home/u/.cargo/bin/` or `` for a bare `'weave' hook wake`). It must be
/// empty or an absolute path ending in `/`, with no embedded single quote (an
/// unescaped `'` would have closed the quoting, so a `'\''` escape inside a real
/// exe path is intentionally not recognized here — such a path cannot be a clean
/// binary location and is treated as foreign).
fn is_clean_quoted_inner(inner: &str) -> bool {
    if inner.is_empty() {
        return true;
    }
    if !inner.starts_with('/') || !inner.ends_with('/') {
        return false;
    }
    !inner.contains('\'') && !inner.contains("//")
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

/// Install (or merge) a weave pre-commit hook in `.git/hooks/pre-commit`
/// that calls `weave lease guard` to block commits on reserved files.
/// Idempotent: skips if the weave guard line is already present.
pub fn install_git_precommit_hook(exe: &str) -> Result<()> {
    let git_dir = find_git_dir().ok_or_else(|| anyhow::anyhow!("not inside a git repository"))?;
    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("pre-commit");

    let guard_line = format!("{} lease guard", shell_single_quote(exe));

    let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
    if existing.contains(&guard_line) || existing.contains("weave lease guard") {
        println!("  git hook: pre-commit already contains weave lease guard");
        return Ok(());
    }

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o755)
        .open(&hook_path)?;

    // If the file is non-empty but missing a trailing newline, add one.
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(f)?;
    }

    if existing.is_empty() {
        writeln!(f, "#!/bin/sh")?;
    }
    writeln!(f, "# weave lease guard — blocks commits on reserved files")?;
    writeln!(f, "{guard_line}")?;

    println!(
        "  git hook: installed pre-commit guard -> {}",
        hook_path.display()
    );
    Ok(())
}

/// Walk upward from cwd looking for a `.git` directory or file.
fn find_git_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let git = dir.join(".git");
        if git.exists() {
            if git.is_dir() {
                return Some(git);
            }
            // .git file (linked worktree) — parse gitdir line.
            let contents = std::fs::read_to_string(&git).ok()?;
            let gitdir = contents
                .lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix("gitdir:"))?;
            return Some(PathBuf::from(gitdir.trim()));
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hook_command, is_weave_command, shell_single_quote};

    #[test]
    fn matches_only_real_weave_hooks() {
        // Legacy UNQUOTED form (written by older weave) still recognized.
        assert!(is_weave_command("weave hook session"));
        assert!(is_weave_command("/home/u/.cargo/bin/weave hook wake"));
        assert!(is_weave_command("/usr/local/bin/weave hook prompt"));
        // must NOT match look-alikes
        assert!(!is_weave_command("/usr/bin/myweave hook session"));
        assert!(!is_weave_command("echo 'about to run weave hook' && other"));
        assert!(!is_weave_command("weave hook notification")); // not an installed event
        assert!(!is_weave_command("weave mcp"));
    }

    #[test]
    fn matches_current_quoted_form() {
        // Current QUOTED form (T2): '<exe>' hook <event>.
        assert!(is_weave_command("'weave' hook session"));
        assert!(is_weave_command("'/home/u/.cargo/bin/weave' hook wake"));
        assert!(is_weave_command("'/usr/local/bin/weave' hook prompt"));
        // A path with a space is legitimate INSIDE the quotes.
        assert!(is_weave_command("'/home/u/My Projects/weave' hook wake"));
        // Quoted look-alikes must NOT match.
        assert!(!is_weave_command("'/usr/bin/myweave' hook session"));
        assert!(!is_weave_command("'weave' mcp"));
        assert!(!is_weave_command("'weave' hook notification"));
    }

    #[test]
    fn rejects_unquoted_prefixes_with_shell_operators_or_whitespace() {
        // T3: an unquoted prefix containing shell operators or whitespace must be
        // rejected even though it ends in `weave hook <event>`.
        assert!(!is_weave_command("echo x && /weave hook wake"));
        assert!(!is_weave_command(": ;/weave hook wake"));
        assert!(!is_weave_command("a | /usr/bin/weave hook wake"));
        assert!(!is_weave_command("/usr/bin/ weave hook wake")); // interior whitespace
        assert!(!is_weave_command("rm -rf /; weave hook wake"));
        assert!(!is_weave_command("/opt/$X/weave hook wake")); // variable expansion
        assert!(!is_weave_command("/opt//weave hook wake")); // empty path component
        assert!(!is_weave_command("relative/path/weave hook wake")); // not absolute
                                                                     // The bare and clean-absolute forms remain accepted.
        assert!(is_weave_command("weave hook wake"));
        assert!(is_weave_command("/opt/bin/weave hook wake"));
    }

    #[test]
    fn shell_single_quote_escapes_correctly() {
        assert_eq!(shell_single_quote("foo"), "'foo'");
        assert_eq!(shell_single_quote("a b"), "'a b'");
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn hook_command_quotes_and_round_trips() {
        // Plain path.
        let cmd = hook_command("/home/u/.cargo/bin/weave", "wake");
        assert_eq!(cmd, "'/home/u/.cargo/bin/weave' hook wake");
        assert!(is_weave_command(&cmd));

        // Path with a space — the whole point of quoting.
        let spaced = hook_command("/home/u/My Projects/weave", "session");
        assert_eq!(spaced, "'/home/u/My Projects/weave' hook session");
        assert!(is_weave_command(&spaced));

        // Bare exe name.
        let bare = hook_command("weave", "prompt");
        assert_eq!(bare, "'weave' hook prompt");
        assert!(is_weave_command(&bare));
    }
}
