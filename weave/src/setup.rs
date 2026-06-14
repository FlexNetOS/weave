//! `weave setup` / `weave uninstall` — wire weave into a coding-agent host
//! (register the MCP server and/or merge lifecycle hooks into the host's own
//! config file, idempotently).
//!
//! `weave setup --provider <claude|codex|gemini|aider>` selects the host. The
//! default (`claude`) registers the `weave` MCP server and merges four lifecycle
//! hooks into `~/.claude/settings.json` — its behavior is unchanged (WL-042 kept
//! the Claude path byte-for-byte). The other providers each write into THEIR own
//! config file Rust-natively, using the SAME discipline:
//!
//!   * never clobber foreign content (only weave's own entry is touched),
//!   * idempotent (re-running refreshes a stale weave entry in place, never
//!     appends a duplicate),
//!   * atomic temp+rename write via `write_private` (0o600, secrets-safe) with a
//!     one-time `.weave.bak` snapshot, and
//!   * read-back verified (re-read + re-parse + assert weave's entry landed and
//!     every foreign entry survived) before reporting success — a non-NotFound
//!     read error ABORTS without writing (never truncate a populated file).
//!
//! Per-provider target file + mechanism (and the unconfirmed-mechanism caveats)
//! are documented on [`Provider`]. Setup is safe to re-run; `uninstall --provider
//! <p>` removes only weave's own entry from that provider's file.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which coding-agent host `weave setup` / `weave uninstall` targets.
///
/// Each variant writes into that host's own config file (sidecar config, NOT a
/// build/runtime input of weave — so no language drift, no new dependency). The
/// command weave wires is always `<exe> hook <arg>` (single-quoted exe).
///
/// * **`Claude`** (CONFIRMED — the baseline): `~/.claude/settings.json`; registers
///   the MCP server via the `claude` CLI and merges four `hooks.{event}` entries.
///   Unchanged by WL-042.
/// * **`Codex`** (mechanism partially confirmed): `~/.codex/config.toml`; sets the
///   top-level `notify` argv key — Codex's documented automation hook, invoked on
///   events — to `["<exe>", "hook", "wake"]`. Codex does not expose Claude's full
///   per-event granularity, so `notify`→`wake` (drain) is the closest analogue.
///   Written Rust-natively; the repo's `.codex/*.toml` ecc sidecar is unrelated
///   and is NOT a source of truth weave mirrors.
/// * **`Gemini`** (mechanism UNCONFIRMED — scaffold-with-caveat): `~/.gemini/
///   settings.json`. Gemini CLI uses a Claude-shaped JSON settings file; the exact
///   lifecycle-hook key is not confirmed, so weave scaffolds the documented
///   best-known shape — a `hooks.{event}[]` block mirroring Claude — and flags the
///   assumption in code + docs. If a future Gemini release confirms a different
///   key this writer should be updated.
/// * **`Aider`** (mechanism LIMITED — scaffold-with-caveat): `~/.aider.conf.yml`.
///   Aider has no rich lifecycle-hook surface; its closest documented config knob
///   is a startup-command list. weave hand-templates a minimal YAML stanza (no
///   YAML dependency added) recording the weave hook command, with a read-back
///   line-presence check. This is a best-effort scaffold; Aider may ignore it
///   until it grows a hook surface (documented as a tracked gap).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
    Aider,
}

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

/// Path to the user-scope Claude Code settings file. `pub` so `backup`/`restore`
/// (WL-035) can include / restore weave's installed hooks, which live merged into
/// this file (`setup.rs` does not drop standalone hook scripts).
pub fn settings_path() -> PathBuf {
    home().join(".claude").join("settings.json")
}

/// Wire weave into the selected coding-agent host. Dispatches to the per-provider
/// writer; the Claude branch (`run_claude`) is the original `run` body,
/// byte-for-byte. The default-Claude path is preserved unchanged.
pub fn run_provider(exe: &str, provider: Provider) -> Result<()> {
    match provider {
        Provider::Claude => run_claude(exe),
        Provider::Codex => run_codex(exe),
        Provider::Gemini => run_gemini(exe),
        Provider::Aider => run_aider(exe),
    }
}

/// The original Claude wiring: register the MCP server and merge the four
/// lifecycle hooks into `~/.claude/settings.json`. UNCHANGED by WL-042.
fn run_claude(exe: &str) -> Result<()> {
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

/// Reverse [`run_provider`]: remove weave's own entry from the selected host's
/// config, leaving every other entry intact.
pub fn uninstall_provider(provider: Provider) -> Result<()> {
    match provider {
        Provider::Claude => uninstall_claude(),
        Provider::Codex => uninstall_codex(),
        Provider::Gemini => uninstall_gemini(),
        Provider::Aider => uninstall_aider(),
    }
}

/// The original Claude uninstall: remove the MCP registration and weave's own
/// hook entries, leaving every other hook (rtk, repowire, …) intact.
fn uninstall_claude() -> Result<()> {
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

/// Read a JSON config file. Returns an empty object ONLY when the file does not
/// exist (or exists but is blank). Any *other* read error (permission denied,
/// EIO, …) is propagated so callers abort WITHOUT overwriting — otherwise a
/// transient read failure on a populated file would let setup truncate it to
/// weave-only entries, destroying every unrelated hook (rtk, repowire, …). See
/// the BLOCKER fix. Shared by the Claude and Gemini (JSON settings) paths.
fn read_json(path: &Path) -> Result<Value> {
    let s = match std::fs::read_to_string(path) {
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

/// Append `suffix` to `path`'s full file name (not its extension). For
/// `settings.json` + `.weave.bak` → `settings.json.weave.bak` (byte-identical to
/// the old `with_extension("json.weave.bak")` Claude used, since that REPLACED the
/// `json` extension with `json.weave.bak`); for `config.toml` + `.weave.bak` →
/// `config.toml.weave.bak`. Using a name-append rather than `with_extension`
/// keeps the derived-sidecar names correct for the non-`.json` provider files
/// (`config.toml`, `.aider.conf.yml`) where replacing the extension would mangle
/// the name.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Write `bytes` to `path` atomically (temp-in-same-dir + rename), creating parent
/// dirs as needed, dropping a one-time `<name>.weave.bak` snapshot of any
/// pre-existing file, and preserving the original file's mode. The `.bak`/`.tmp`
/// sidecars are created 0o600 (they may carry secrets); a brand-new target is left
/// at 0o600 as a safe default. Shared by every provider write so all of them get
/// the same atomicity + backup + secrets-safe discipline as the Claude path.
fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Capture the pre-existing file's mode (if any) so we can preserve it on the
    // renamed result; the .tmp file is created 0o600 and rename keeps the tmp's
    // mode, which would otherwise tighten the live file unexpectedly.
    let original_mode: Option<u32> = std::fs::metadata(path).ok().map(|m| m.permissions().mode());

    // One-time backup of the pre-existing file (best-effort, never created twice).
    // The snapshot may contain secrets, so write it 0o600.
    if path.exists() {
        let bak = sidecar(path, ".weave.bak");
        if !bak.exists() {
            if let Ok(original) = std::fs::read(path) {
                let _ = write_private(&bak, &original);
            }
        }
    }

    // Write to a sibling temp file then rename over the target (atomic on POSIX).
    // The temp file is created 0o600 so secrets are never briefly world-readable.
    let tmp = sidecar(path, ".weave.tmp");
    write_private(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;

    // Restore the original file's permissions on the renamed result. If the file
    // is brand new (no original mode) we leave it at the tmp's 0o600, which is a
    // safe default for a file that may hold secrets.
    if let Some(mode) = original_mode {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}

/// Write a JSON value to `path`, pretty-printed with a trailing newline, via the
/// shared atomic+backup [`write_bytes_atomic`]. Shared by the Claude and Gemini
/// JSON-settings paths.
fn write_json_atomic(path: &Path, v: &Value) -> Result<()> {
    let mut out = serde_json::to_string_pretty(v)?;
    out.push('\n');
    write_bytes_atomic(path, out.as_bytes())
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

/// Merge weave's lifecycle hooks into Claude's settings.json idempotently. Thin
/// wrapper over [`merge_hooks_at`] at [`settings_path`].
fn merge_hooks(exe: &str) -> Result<Vec<String>> {
    merge_hooks_at(&settings_path(), exe)
}

/// Merge weave's lifecycle hooks into a Claude-shaped JSON settings file at
/// `path`, idempotently. Returns the list of event names that were newly added
/// (empty if all already present). Shared by the Claude and Gemini paths — the
/// `hooks.{event}[]` shape and the never-clobber/idempotent/read-back discipline
/// are identical; only the target file differs.
fn merge_hooks_at(path: &Path, exe: &str) -> Result<Vec<String>> {
    let mut settings = read_json(path)?;

    // WL-041: snapshot the foreign (non-weave) hook commands BEFORE we mutate, so
    // the post-write read-back can prove the merge clobbered none of them.
    let foreign_before = foreign_commands(&settings);

    // Ensure `hooks` is an object we can index into.
    let root = settings
        .as_object_mut()
        .context("settings root is not an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        anyhow::bail!("`hooks` is not an object in {}", path.display());
    }
    let hooks = hooks.as_object_mut().unwrap();

    let mut added = Vec::new();

    for (event, arg) in HOOKS {
        let command = hook_command(exe, arg);

        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !entries.is_array() {
            anyhow::bail!("hooks.{event} is not an array in {}", path.display());
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

    write_json_atomic(path, &settings)?;

    // WL-041: never trust the write blindly — re-open the file, re-parse it, and
    // confirm weave's intended entries landed AND every pre-existing foreign hook
    // survived. Mirrors the WL-035 backup read-back contract.
    let written = read_json(path).with_context(|| {
        format!(
            "{} read-back verification failed: cannot re-read after merge \
             (recover from the .weave.bak snapshot)",
            path.display()
        )
    })?;
    verify_settings_merged(&written, &foreign_before, exe)?;

    Ok(added)
}

/// Collect the set of FOREIGN (non-weave) inner `command` strings present under any
/// event in a parsed settings.json. Used to prove a merge/prune clobbered nothing:
/// every command captured here must still be present after the write. Read-only — it
/// never mutates `settings`.
fn foreign_commands(settings: &Value) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for entries in hooks.values() {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        for entry in entries {
            let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for h in inner {
                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                    if !is_weave_command(cmd) {
                        out.insert(cmd.to_string());
                    }
                }
            }
        }
    }
    out
}

/// True iff `settings` contains a weave hook command equal to `hook_command(exe, arg)`
/// for the given event argument (read-only; the read-back analogue of
/// [`find_weave_command_mut`]).
fn has_weave_command_for(settings: &Value, exe: &str, arg: &str) -> bool {
    let want = hook_command(exe, arg);
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    hooks.values().any(|entries| {
        entries
            .as_array()
            .map(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command").and_then(|c| c.as_str()) == Some(want.as_str())
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Read-back predicate for [`merge_hooks`]: assert the re-read settings.json carries
/// a weave command pointing at `exe` for **every** installed event, and that every
/// foreign command captured before the merge (`foreign_before`) is still present.
/// On any failure, returns a descriptive `Err` naming the recovery `.bak`.
fn verify_settings_merged(
    settings: &Value,
    foreign_before: &std::collections::BTreeSet<String>,
    exe: &str,
) -> Result<()> {
    for (event, arg) in HOOKS {
        if !has_weave_command_for(settings, exe, arg) {
            anyhow::bail!(
                "settings.json read-back verification failed: weave hook for `{event}` is \
                 missing or does not point at `{exe}` after the merge \
                 (recover from settings.json.weave.bak)"
            );
        }
    }
    let foreign_after = foreign_commands(settings);
    if let Some(lost) = foreign_before.difference(&foreign_after).next() {
        anyhow::bail!(
            "settings.json read-back verification failed: a pre-existing foreign hook was \
             lost during the merge: `{lost}` (recover from settings.json.weave.bak)"
        );
    }
    Ok(())
}

/// Read-back predicate for [`prune_hooks`]: assert the re-read settings.json carries
/// NO weave command under any event, and that every foreign command captured before
/// the prune (`foreign_before`) is still present.
fn verify_settings_pruned(
    settings: &Value,
    foreign_before: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if let Some(hooks) = settings.get("hooks").and_then(Value::as_object) {
        for entries in hooks.values() {
            let Some(entries) = entries.as_array() else {
                continue;
            };
            for entry in entries {
                let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
                    continue;
                };
                for h in inner {
                    if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                        if is_weave_command(cmd) {
                            anyhow::bail!(
                                "settings.json read-back verification failed: a weave hook \
                                 survived the prune: `{cmd}` \
                                 (recover from settings.json.weave.bak)"
                            );
                        }
                    }
                }
            }
        }
    }
    let foreign_after = foreign_commands(settings);
    if let Some(lost) = foreign_before.difference(&foreign_after).next() {
        anyhow::bail!(
            "settings.json read-back verification failed: a pre-existing foreign hook was \
             lost during the prune: `{lost}` (recover from settings.json.weave.bak)"
        );
    }
    Ok(())
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

/// Remove every weave hook entry from Claude's settings.json. Thin wrapper over
/// [`prune_hooks_at`] at [`settings_path`].
fn prune_hooks() -> Result<usize> {
    prune_hooks_at(&settings_path())
}

/// Remove every weave hook entry from the Claude-shaped JSON settings file at
/// `path`, leaving all other hooks untouched. Returns the number of inner
/// `command` hooks removed. Shared by the Claude and Gemini paths.
fn prune_hooks_at(path: &Path) -> Result<usize> {
    // Nothing to do if there's no settings file yet.
    if !path.exists() {
        return Ok(0);
    }

    let mut settings = read_json(path)?;
    // WL-041: snapshot foreign hook commands before the prune so the read-back can
    // prove only weave's own entries were removed.
    let foreign_before = foreign_commands(&settings);
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
        write_json_atomic(path, &settings)?;

        // WL-041: re-open and confirm no weave hook survived and every foreign hook
        // is intact before reporting success.
        let written = read_json(path).with_context(|| {
            format!(
                "{} read-back verification failed: cannot re-read after prune \
                 (recover from the .weave.bak snapshot)",
                path.display()
            )
        })?;
        verify_settings_pruned(&written, &foreign_before)?;
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

    // Drop the handle so the read-back sees the flushed, closed file.
    drop(f);

    // WL-041: re-read the hook and confirm the append landed AND nothing
    // pre-existing was clobbered (we only ever append; the install-preflight
    // "never clobber a foreign hook" rule).
    verify_git_hook_written(&hook_path, &guard_line, &existing)?;

    println!(
        "  git hook: installed pre-commit guard -> {}",
        hook_path.display()
    );
    Ok(())
}

/// Read-back predicate for [`install_git_precommit_hook`]: re-open the hook file and
/// assert (1) it exists and contains `guard_line`, (2) if freshly created it starts
/// with the `#!/bin/sh` shebang, and (3) every byte of the pre-existing `existing`
/// content survived (we only append — foreign hook lines must be preserved). On any
/// failure returns a descriptive `Err`.
fn verify_git_hook_written(hook_path: &Path, guard_line: &str, existing: &str) -> Result<()> {
    let reread = std::fs::read_to_string(hook_path).with_context(|| {
        format!(
            "pre-commit hook read-back verification failed: cannot re-read {}",
            hook_path.display()
        )
    })?;

    if !reread.contains(guard_line) {
        anyhow::bail!(
            "pre-commit hook read-back verification failed: the weave guard line is absent \
             from {} after install",
            hook_path.display()
        );
    }

    if existing.is_empty() {
        if !reread.starts_with("#!/bin/sh") {
            anyhow::bail!(
                "pre-commit hook read-back verification failed: a freshly created {} is \
                 missing its `#!/bin/sh` shebang",
                hook_path.display()
            );
        }
    } else {
        // We only appended; the original content (modulo a single trailing newline we
        // may have added) must remain a prefix of the file.
        let kept = reread.starts_with(existing)
            || (!existing.ends_with('\n') && reread.starts_with(&format!("{existing}\n")));
        if !kept {
            anyhow::bail!(
                "pre-commit hook read-back verification failed: pre-existing content in {} \
                 was not preserved (a foreign hook may have been clobbered)",
                hook_path.display()
            );
        }
    }
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

// ===========================================================================
// WL-042: multi-provider lifecycle hook templates (Codex / Gemini / Aider).
//
// Every provider writer below mirrors the Claude discipline: read existing
// config (NotFound → empty, any other read error ABORTS without writing),
// merge only weave's own entry (never clobber foreign content), write atomically
// via `write_bytes_atomic`/`write_json_atomic` (0o600 sidecars + one-time .bak),
// be idempotent (re-run = no-op / in-place refresh), and READ-BACK verify the
// result before printing success.
// ===========================================================================

// --- Codex CLI -------------------------------------------------------------
// Target: ~/.codex/config.toml. Mechanism (partially confirmed): Codex's
// documented automation hook is a top-level `notify` key whose value is an argv
// array, invoked by Codex on events. We set `notify = ["<exe>", "hook", "wake"]`
// (the closest analogue to Claude's drain; Codex does not expose per-event
// granularity). Written Rust-natively with a LINE-BASED merge so we add no `toml`
// dependency to the bin crate and never clobber foreign keys — top-level keys in
// TOML must precede any `[table]` header, so we replace an existing top-level
// `notify = …` line in place, or insert ours before the first table header
// (appending if the file has none).

/// Path to Codex CLI's user config (`~/.codex/config.toml`).
pub fn codex_config_path() -> PathBuf {
    home().join(".codex").join("config.toml")
}

/// The Codex `notify` line weave installs: a TOML array of the argv weave wants
/// Codex to run on a lifecycle event. Each element is a TOML basic string, so the
/// exe (and the literal `hook`/`wake` args) are quoted; backslashes and quotes in
/// the exe path are escaped. Example: `notify = ["/home/u/.cargo/bin/weave", "hook", "wake"]`.
fn codex_notify_line(exe: &str) -> String {
    format!(
        "notify = [{}, {}, {}]",
        toml_basic_string(exe),
        toml_basic_string("hook"),
        toml_basic_string("wake"),
    )
}

/// Quote a string as a TOML basic string (double-quoted, with `\` and `"`
/// escaped). Sufficient for an exe path / fixed argv tokens — no control chars
/// are expected in a binary path, but we escape the two structural characters.
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// True iff a trimmed config line is a top-level `notify = …` assignment (the key
/// weave owns). Used to recognize/replace/prune our line without a TOML parser.
fn is_notify_line(line: &str) -> bool {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("notify") {
        let rest = rest.trim_start();
        return rest.starts_with('=');
    }
    false
}

/// True iff a trimmed config line opens a TOML table/array-of-tables header
/// (`[x]` / `[[x]]`). Top-level bare keys must appear BEFORE the first such
/// header, so this is where we stop scanning for a place to insert `notify`.
fn is_table_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

/// Read `~/.codex/config.toml` as raw text. NotFound → empty string; any OTHER
/// read error ABORTS (never truncate a populated config). The Codex equivalent of
/// [`read_json`]'s BLOCKER-fix discipline.
fn read_codex_text() -> Result<String> {
    let path = codex_config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "reading {} (refusing to continue: overwriting now would clobber \
                 existing Codex config)",
                path.display()
            )
        }),
    }
}

/// Merge weave's `notify` line into Codex config text. Replaces an existing
/// top-level `notify = …` line in place; otherwise inserts ours just before the
/// first table header (or appends to the end if there is none). Returns the new
/// text and whether anything changed (idempotency: an already-correct line → no
/// change). Foreign keys/tables are never touched.
fn merge_codex_notify(existing: &str, exe: &str) -> (String, bool) {
    let want = codex_notify_line(exe);
    let lines: Vec<&str> = existing.lines().collect();

    // Idempotent replace if a top-level notify line already exists.
    if let Some(idx) = lines.iter().position(|l| is_notify_line(l)) {
        if lines[idx].trim_end() == want {
            return (existing.to_string(), false); // already correct.
        }
        let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        out[idx] = want;
        return (join_with_trailing_newline(&out), true);
    }

    // No existing notify line: insert before the first table header, else append.
    let insert_at = lines.iter().position(|l| is_table_header(l));
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    match insert_at {
        Some(i) => out.insert(i, want),
        None => out.push(want),
    }
    (join_with_trailing_newline(&out), true)
}

/// Remove weave's top-level `notify` line from Codex config text. Returns the new
/// text and whether a line was removed. Foreign content is left byte-for-byte.
fn prune_codex_notify(existing: &str) -> (String, bool) {
    let mut removed = false;
    let kept: Vec<String> = existing
        .lines()
        .filter(|l| {
            if is_notify_line(l) {
                removed = true;
                false
            } else {
                true
            }
        })
        .map(|s| s.to_string())
        .collect();
    if !removed {
        return (existing.to_string(), false);
    }
    (join_with_trailing_newline(&kept), true)
}

/// Join lines with `\n` and a single trailing newline (matching conventional
/// editors). An empty list yields an empty string.
fn join_with_trailing_newline(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// Wire weave into Codex CLI: set the `notify` argv in `~/.codex/config.toml`.
fn run_codex(exe: &str) -> Result<()> {
    let path = codex_config_path();
    let existing = read_codex_text()?;
    let (merged, changed) = merge_codex_notify(&existing, exe);

    if changed {
        write_bytes_atomic(&path, merged.as_bytes())
            .context("writing weave notify into Codex config.toml")?;
        // Read-back verify: re-read and confirm our notify line is present.
        let reread = read_codex_text().context(
            "config.toml read-back verification failed: cannot re-read after merge \
             (recover from config.toml.weave.bak)",
        )?;
        verify_codex_notify_present(&reread, exe)?;
    }

    println!("weave setup complete (codex):");
    println!("  exe:    {exe}");
    println!("  config: {}", path.display());
    println!("  notify: {}", codex_notify_line(exe));
    if changed {
        println!("  status: notify key written");
    } else {
        println!("  status: already present (no changes)");
    }
    println!(
        "  note:   Codex's `notify` hook maps to weave's drain (`hook wake`); Codex does \
         not expose Claude's per-event granularity."
    );
    Ok(())
}

/// Reverse [`run_codex`]: remove weave's `notify` line from Codex config.
fn uninstall_codex() -> Result<()> {
    let path = codex_config_path();
    if !path.exists() {
        println!("no Codex config at {}", path.display());
        return Ok(());
    }
    let existing = read_codex_text()?;
    let (pruned, removed) = prune_codex_notify(&existing);
    if removed {
        write_bytes_atomic(&path, pruned.as_bytes())
            .context("removing weave notify from Codex config.toml")?;
        let reread = read_codex_text().context(
            "config.toml read-back verification failed: cannot re-read after prune \
             (recover from config.toml.weave.bak)",
        )?;
        if reread.lines().any(is_notify_line) {
            anyhow::bail!(
                "config.toml read-back verification failed: a `notify` line survived the \
                 prune (recover from config.toml.weave.bak)"
            );
        }
        println!("removed weave notify from {}", path.display());
    } else {
        println!("no weave notify found in {}", path.display());
    }
    Ok(())
}

/// Read-back predicate for [`run_codex`]: assert the re-read Codex config carries
/// exactly weave's intended `notify` line.
fn verify_codex_notify_present(text: &str, exe: &str) -> Result<()> {
    let want = codex_notify_line(exe);
    if text.lines().any(|l| l.trim_end() == want) {
        Ok(())
    } else {
        anyhow::bail!(
            "config.toml read-back verification failed: the weave `notify` line is absent \
             or does not point at `{exe}` after the merge (recover from config.toml.weave.bak)"
        )
    }
}

// --- Gemini CLI ------------------------------------------------------------
// Target: ~/.gemini/settings.json. Mechanism (UNCONFIRMED — scaffold-with-caveat):
// Gemini CLI uses a Claude-shaped JSON settings file, but the exact lifecycle-hook
// key is NOT confirmed at implementation time. We scaffold the documented
// best-known shape — the SAME `hooks.{event}[]` structure Claude uses — so that if
// Gemini adopts Claude-compatible hooks it works, and the merge/prune/read-back
// machinery is identical. The assumption is recorded here, in the CLI output, and
// in README/parity docs; update this writer if Gemini confirms a different key.

/// Path to Gemini CLI's user settings (`~/.gemini/settings.json`).
pub fn gemini_settings_path() -> PathBuf {
    home().join(".gemini").join("settings.json")
}

/// Wire weave into Gemini CLI by merging the four lifecycle hooks into
/// `~/.gemini/settings.json`, reusing the exact JSON `hooks.{event}` merge the
/// Claude path uses (just at a different target file). Gemini has no `claude
/// mcp add`-style CLI, so we only scaffold hooks (no MCP registration step).
fn run_gemini(exe: &str) -> Result<()> {
    let path = gemini_settings_path();
    let added = merge_hooks_at(&path, exe).context("merging weave hooks into Gemini settings")?;

    println!("weave setup complete (gemini):");
    println!("  exe:      {exe}");
    println!("  settings: {}", path.display());
    if added.is_empty() {
        println!("  hooks:    already present (no changes)");
    } else {
        println!("  hooks:    added {}", added.join(", "));
    }
    println!(
        "  note:   Gemini CLI's exact lifecycle-hook key is UNCONFIRMED; weave scaffolds the \
         Claude-compatible `hooks` shape. Update if Gemini confirms a different key."
    );
    Ok(())
}

/// Reverse [`run_gemini`]: remove weave's hooks from Gemini settings.
fn uninstall_gemini() -> Result<()> {
    let path = gemini_settings_path();
    let removed = prune_hooks_at(&path).context("removing weave hooks from Gemini settings")?;
    if removed == 0 {
        println!("no weave hooks found in {}", path.display());
    } else {
        println!("removed {removed} weave hook(s) from {}", path.display());
    }
    Ok(())
}

// --- Aider -----------------------------------------------------------------
// Target: ~/.aider.conf.yml. Mechanism (LIMITED — scaffold-with-caveat): Aider has
// no rich lifecycle-hook surface. Its closest documented config knob is a
// startup-command list; we hand-template a minimal YAML stanza recording the weave
// hook command under a weave-namespaced block, with NO YAML dependency (manual
// string compose + read-back line-presence check). Aider may IGNORE this until it
// grows a hook surface — this is a tracked best-effort scaffold, documented as a
// gap in README/parity. We never add serde_yaml or any dep.

/// Path to Aider's user config (`~/.aider.conf.yml`).
pub fn aider_config_path() -> PathBuf {
    home().join(".aider.conf.yml")
}

/// The weave-owned marker line that opens our YAML stanza. We recognize/replace/
/// prune our block by this exact marker so foreign YAML is never touched.
const AIDER_MARKER: &str = "# weave: lifecycle hook (scaffold — Aider hook support is limited)";

/// Build weave's Aider YAML stanza: a marker comment plus a `weave-hook:` mapping
/// recording the command weave would have Aider run. Hand-templated (no YAML dep).
/// The exe is single-quoted as a YAML single-quoted scalar (doubling any interior
/// `'`), so a path with spaces stays one scalar.
fn aider_stanza(exe: &str) -> String {
    let scalar = yaml_single_quote(exe);
    format!("{AIDER_MARKER}\nweave-hook: {scalar} hook session\n")
}

/// Quote a string as a YAML single-quoted scalar: wrap in `'…'`, doubling any
/// interior single quote (the YAML escape). `a'b` → `'a''b'`.
fn yaml_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Read `~/.aider.conf.yml` as raw text. NotFound → empty; any OTHER read error
/// ABORTS (never truncate a populated config). Aider equivalent of the BLOCKER-fix.
fn read_aider_text() -> Result<String> {
    let path = aider_config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "reading {} (refusing to continue: overwriting now would clobber \
                 existing Aider config)",
                path.display()
            )
        }),
    }
}

/// Merge weave's stanza into Aider config text. If our marker is already present,
/// the stanza is already installed → no change (idempotent). Otherwise we APPEND
/// the stanza (foreign content is never rewritten). Returns new text + changed?.
fn merge_aider_stanza(existing: &str, exe: &str) -> (String, bool) {
    if existing.lines().any(|l| l.trim_end() == AIDER_MARKER) {
        return (existing.to_string(), false); // already installed.
    }
    let mut out = String::new();
    if !existing.is_empty() {
        out.push_str(existing);
        if !existing.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&aider_stanza(exe));
    (out, true)
}

/// Remove weave's stanza (marker line + the following `weave-hook:` line) from
/// Aider config text. Returns new text + whether anything was removed. Only the
/// two weave-owned lines are dropped; foreign lines are preserved verbatim.
fn prune_aider_stanza(existing: &str) -> (String, bool) {
    let mut removed = false;
    let mut kept: Vec<&str> = Vec::new();
    let mut skip_next_hook = false;
    for line in existing.lines() {
        if line.trim_end() == AIDER_MARKER {
            removed = true;
            skip_next_hook = true;
            continue;
        }
        if skip_next_hook {
            skip_next_hook = false;
            if line.trim_start().starts_with("weave-hook:") {
                continue; // drop weave's value line.
            }
        }
        kept.push(line);
    }
    if !removed {
        return (existing.to_string(), false);
    }
    let owned: Vec<String> = kept.iter().map(|s| s.to_string()).collect();
    (join_with_trailing_newline(&owned), true)
}

/// Wire weave into Aider: append weave's lifecycle stanza to `~/.aider.conf.yml`.
fn run_aider(exe: &str) -> Result<()> {
    let path = aider_config_path();
    let existing = read_aider_text()?;
    let (merged, changed) = merge_aider_stanza(&existing, exe);

    if changed {
        write_bytes_atomic(&path, merged.as_bytes())
            .context("writing weave stanza into Aider config")?;
        let reread = read_aider_text().context(
            "aider.conf.yml read-back verification failed: cannot re-read after merge \
             (recover from .aider.conf.yml.weave.bak)",
        )?;
        if !reread.lines().any(|l| l.trim_end() == AIDER_MARKER) {
            anyhow::bail!(
                "aider.conf.yml read-back verification failed: the weave stanza marker is \
                 absent after the merge (recover from .aider.conf.yml.weave.bak)"
            );
        }
    }

    println!("weave setup complete (aider):");
    println!("  exe:    {exe}");
    println!("  config: {}", path.display());
    if changed {
        println!("  status: weave stanza written");
    } else {
        println!("  status: already present (no changes)");
    }
    println!(
        "  note:   Aider has no rich lifecycle-hook surface; this stanza is a best-effort \
         scaffold and may be ignored until Aider grows hook support."
    );
    Ok(())
}

/// Reverse [`run_aider`]: remove weave's stanza from Aider config.
fn uninstall_aider() -> Result<()> {
    let path = aider_config_path();
    if !path.exists() {
        println!("no Aider config at {}", path.display());
        return Ok(());
    }
    let existing = read_aider_text()?;
    let (pruned, removed) = prune_aider_stanza(&existing);
    if removed {
        write_bytes_atomic(&path, pruned.as_bytes())
            .context("removing weave stanza from Aider config")?;
        let reread = read_aider_text().context(
            "aider.conf.yml read-back verification failed: cannot re-read after prune \
             (recover from .aider.conf.yml.weave.bak)",
        )?;
        if reread.lines().any(|l| l.trim_end() == AIDER_MARKER) {
            anyhow::bail!(
                "aider.conf.yml read-back verification failed: the weave stanza survived the \
                 prune (recover from .aider.conf.yml.weave.bak)"
            );
        }
        println!("removed weave stanza from {}", path.display());
    } else {
        println!("no weave stanza found in {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        aider_stanza, codex_notify_line, foreign_commands, hook_command, is_weave_command,
        merge_aider_stanza, merge_codex_notify, prune_aider_stanza, prune_codex_notify,
        shell_single_quote, verify_codex_notify_present, verify_git_hook_written,
        verify_settings_merged, verify_settings_pruned, AIDER_MARKER,
    };
    use serde_json::json;

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

    // --- WL-041 read-back verification predicates -------------------------

    /// A settings.json `Value` carrying a weave hook for every installed event,
    /// pointing at `exe`, plus the given foreign commands under SessionStart.
    fn settings_with(exe: &str, foreign: &[&str]) -> serde_json::Value {
        let mut session_entries = vec![json!({
            "matcher": "",
            "hooks": [ { "type": "command", "command": hook_command(exe, "session") } ]
        })];
        for f in foreign {
            session_entries.push(json!({
                "matcher": "",
                "hooks": [ { "type": "command", "command": *f } ]
            }));
        }
        json!({
            "hooks": {
                "SessionStart": session_entries,
                "UserPromptSubmit": [ { "matcher": "",
                    "hooks": [ { "type": "command", "command": hook_command(exe, "prompt") } ] } ],
                "Stop": [ { "matcher": "",
                    "hooks": [ { "type": "command", "command": hook_command(exe, "wake") } ] } ],
                "SubagentStop": [ { "matcher": "",
                    "hooks": [ { "type": "command", "command": hook_command(exe, "wake") } ] } ],
            }
        })
    }

    #[test]
    fn foreign_commands_excludes_weave_includes_others() {
        let s = settings_with("/bin/weave", &["rtk hook session", "repowire notify"]);
        let foreign = foreign_commands(&s);
        assert!(foreign.contains("rtk hook session"));
        assert!(foreign.contains("repowire notify"));
        // weave's own commands must NOT be counted as foreign.
        assert!(!foreign.iter().any(|c| is_weave_command(c)));
    }

    #[test]
    fn verify_merged_ok_on_complete_write() {
        let exe = "/home/u/.cargo/bin/weave";
        let s = settings_with(exe, &["rtk hook session"]);
        let foreign_before = foreign_commands(&settings_with(exe, &["rtk hook session"]));
        assert!(verify_settings_merged(&s, &foreign_before, exe).is_ok());
    }

    #[test]
    fn verify_merged_errs_when_a_weave_hook_is_missing() {
        let exe = "/bin/weave";
        // Drop the UserPromptSubmit event entirely — its `prompt` arg is unique to
        // it, so no other event's command can satisfy the read-back. The read-back
        // must catch the gap.
        let mut s = settings_with(exe, &[]);
        s["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("UserPromptSubmit");
        let foreign_before = std::collections::BTreeSet::new();
        let err = verify_settings_merged(&s, &foreign_before, exe).unwrap_err();
        assert!(err.to_string().contains("read-back verification failed"));
        assert!(err.to_string().contains("UserPromptSubmit"));
    }

    #[test]
    fn verify_merged_errs_when_a_foreign_hook_vanished() {
        let exe = "/bin/weave";
        // Write landed correctly for weave, but a foreign hook present BEFORE is gone.
        let after = settings_with(exe, &[]);
        let mut foreign_before = std::collections::BTreeSet::new();
        foreign_before.insert("rtk hook session".to_string());
        let err = verify_settings_merged(&after, &foreign_before, exe).unwrap_err();
        assert!(err.to_string().contains("foreign hook was lost"));
        assert!(err.to_string().contains("rtk hook session"));
    }

    #[test]
    fn verify_merged_errs_when_exe_points_elsewhere() {
        // The file has weave hooks, but for a DIFFERENT exe than we intended to write.
        let s = settings_with("/old/path/weave", &[]);
        let foreign_before = std::collections::BTreeSet::new();
        let err = verify_settings_merged(&s, &foreign_before, "/new/path/weave").unwrap_err();
        assert!(err.to_string().contains("read-back verification failed"));
    }

    #[test]
    fn verify_pruned_ok_when_no_weave_hook_and_foreign_kept() {
        // No weave entries; one foreign hook retained.
        let s = json!({
            "hooks": {
                "SessionStart": [ { "matcher": "",
                    "hooks": [ { "type": "command", "command": "rtk hook session" } ] } ]
            }
        });
        let mut foreign_before = std::collections::BTreeSet::new();
        foreign_before.insert("rtk hook session".to_string());
        assert!(verify_settings_pruned(&s, &foreign_before).is_ok());
        // Also OK on a fully empty settings object.
        assert!(verify_settings_pruned(&json!({}), &std::collections::BTreeSet::new()).is_ok());
    }

    #[test]
    fn verify_pruned_errs_when_a_weave_hook_survives() {
        let s = settings_with("/bin/weave", &[]);
        let err = verify_settings_pruned(&s, &std::collections::BTreeSet::new()).unwrap_err();
        assert!(err.to_string().contains("weave hook survived the prune"));
    }

    #[test]
    fn verify_pruned_errs_when_foreign_lost() {
        let s = json!({ "hooks": {} });
        let mut foreign_before = std::collections::BTreeSet::new();
        foreign_before.insert("rtk hook session".to_string());
        let err = verify_settings_pruned(&s, &foreign_before).unwrap_err();
        assert!(err.to_string().contains("foreign hook was lost"));
    }

    #[test]
    fn verify_git_hook_ok_fresh_file() {
        let dir = std::env::temp_dir().join(format!(
            "weave-ut-githook-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pre-commit");
        let guard = "'/bin/weave' lease guard";
        std::fs::write(&path, format!("#!/bin/sh\n{guard}\n")).unwrap();
        assert!(verify_git_hook_written(&path, guard, "").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_git_hook_errs_when_guard_absent() {
        let dir = std::env::temp_dir().join(format!(
            "weave-ut-githook-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pre-commit");
        // Wrote the shebang but the guard line never landed (simulated partial write).
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        let err = verify_git_hook_written(&path, "'/bin/weave' lease guard", "").unwrap_err();
        assert!(err.to_string().contains("guard line is absent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_git_hook_errs_when_foreign_content_clobbered() {
        let dir = std::env::temp_dir().join(format!(
            "weave-ut-githook-clobber-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pre-commit");
        let guard = "'/bin/weave' lease guard";
        // Pre-existing foreign content, but the re-read file does NOT preserve it.
        std::fs::write(&path, format!("{guard}\n")).unwrap();
        let existing = "#!/bin/sh\n# foreign rtk hook\nrtk pre-commit\n";
        let err = verify_git_hook_written(&path, guard, existing).unwrap_err();
        assert!(err.to_string().contains("was not preserved"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- WL-042: Codex notify line --------------------------------------------

    #[test]
    fn codex_notify_line_is_toml_argv_and_round_trips() {
        let exe = "/home/u/.cargo/bin/weave";
        let line = codex_notify_line(exe);
        assert_eq!(
            line,
            r#"notify = ["/home/u/.cargo/bin/weave", "hook", "wake"]"#
        );
        // The read-back predicate accepts exactly this generated line.
        assert!(verify_codex_notify_present(&line, exe).is_ok());
        // A path with a space stays one TOML basic string (quoted).
        let spaced = codex_notify_line("/home/u/My Tools/weave");
        assert_eq!(
            spaced,
            r#"notify = ["/home/u/My Tools/weave", "hook", "wake"]"#
        );
    }

    #[test]
    fn codex_merge_inserts_before_first_table_and_preserves_foreign() {
        let exe = "/bin/weave";
        let existing = "model = \"o1\"\n\n[tui]\ntheme = \"dark\"\n";
        let (merged, changed) = merge_codex_notify(existing, exe);
        assert!(changed);
        // notify inserted as a top-level key BEFORE the [tui] table header.
        let notify_pos = merged.find("notify = ").unwrap();
        let table_pos = merged.find("[tui]").unwrap();
        assert!(notify_pos < table_pos, "notify before table: {merged}");
        // Foreign keys/tables survive.
        assert!(merged.contains("model = \"o1\""));
        assert!(merged.contains("theme = \"dark\""));
        // Read-back predicate passes.
        assert!(verify_codex_notify_present(&merged, exe).is_ok());
    }

    #[test]
    fn codex_merge_is_idempotent_and_heals_stale_path() {
        let exe = "/bin/weave";
        let (first, c1) = merge_codex_notify("", exe);
        assert!(c1);
        // Re-running with the SAME exe is a no-op.
        let (second, c2) = merge_codex_notify(&first, exe);
        assert!(!c2, "idempotent: {second}");
        assert_eq!(first, second);
        // A stale exe path is healed in place (still exactly one notify line).
        let (healed, c3) = merge_codex_notify(&first, "/new/weave");
        assert!(c3);
        assert_eq!(healed.matches("notify = ").count(), 1);
        assert!(healed.contains("/new/weave"));
    }

    #[test]
    fn codex_prune_removes_only_notify() {
        let exe = "/bin/weave";
        let (merged, _) = merge_codex_notify("model = \"o1\"\n", exe);
        let (pruned, removed) = prune_codex_notify(&merged);
        assert!(removed);
        assert!(!pruned.contains("notify = "));
        assert!(pruned.contains("model = \"o1\""));
        // Pruning a file with no notify line is a no-op.
        let (again, removed2) = prune_codex_notify(&pruned);
        assert!(!removed2);
        assert_eq!(again, pruned);
    }

    // --- WL-042: Aider stanza -------------------------------------------------

    #[test]
    fn aider_stanza_carries_marker_and_quoted_exe() {
        let s = aider_stanza("/home/u/My Tools/weave");
        assert!(s.contains(AIDER_MARKER));
        // Exe single-quoted so a space-bearing path stays one scalar.
        assert!(s.contains("weave-hook: '/home/u/My Tools/weave' hook session"));
    }

    #[test]
    fn aider_merge_appends_once_and_preserves_foreign() {
        let exe = "/bin/weave";
        let existing = "model: gpt-4o\nauto-commits: false\n";
        let (merged, changed) = merge_aider_stanza(existing, exe);
        assert!(changed);
        // Foreign keys preserved verbatim.
        assert!(merged.contains("model: gpt-4o"));
        assert!(merged.contains("auto-commits: false"));
        assert!(merged.contains(AIDER_MARKER));
        // Idempotent: second merge is a no-op.
        let (again, changed2) = merge_aider_stanza(&merged, exe);
        assert!(!changed2);
        assert_eq!(again, merged);
        // Exactly one marker — no duplicate stanza.
        assert_eq!(merged.matches(AIDER_MARKER).count(), 1);
    }

    #[test]
    fn aider_prune_removes_only_weave_stanza() {
        let exe = "/bin/weave";
        let existing = "model: gpt-4o\n";
        let (merged, _) = merge_aider_stanza(existing, exe);
        let (pruned, removed) = prune_aider_stanza(&merged);
        assert!(removed);
        assert!(!pruned.contains(AIDER_MARKER));
        assert!(!pruned.contains("weave-hook:"));
        // Foreign content survives.
        assert!(pruned.contains("model: gpt-4o"));
        // Pruning again is a no-op.
        let (again, removed2) = prune_aider_stanza(&pruned);
        assert!(!removed2);
        assert_eq!(again, pruned);
    }
}
