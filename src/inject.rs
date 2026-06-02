//! Native injector: delivers text into a *running* agent session's terminal pane
//! by driving the terminal multiplexer / control-capable terminal it lives in.
//! No Python, no repowire — this is weave's own first-class injector.
//!
//! Cross-session injection needs no daemon: each supported multiplexer can target
//! an arbitrary pane/session/window from any process, so the sender injects
//! directly into the recipient's registered target.
//!
//! Supported backends:
//!   - tmux    (`send-keys`)            target = pane id      env TMUX_PANE
//!   - zellij  (`action write-chars`)   target = session name env ZELLIJ_SESSION_NAME
//!   - kitty   (`kitten @ send-text`)   target = window id    env KITTY_WINDOW_ID
//!   - wezterm (`cli send-text`)        target = pane id      env WEZTERM_PANE
//!   - screen  (`-X stuff`)             target = session      env STY
//!
//! Submission (pressing Enter) is the tricky part: modern TUIs (Claude Code) run
//! in **bracketed paste** mode, where a naive Enter after literal text can be
//! swallowed or interpreted as a TUI key. Each backend below uses the
//! paste-safe submission idiom for that terminal (e.g. tmux closes bracketed paste
//! with the hex `ESC [ 2 0 1 ~` sequence before sending Enter).

use crate::model::Peer;
use anyhow::{bail, Result};
use std::process::Command;

/// A carriage return — the byte a TUI reads as "Enter".
const CR: &str = "\r";

/// Hard cap on injected characters. A nudge is a short ping; a hostile or huge
/// message body must never flood the recipient's input line. Anything longer is
/// truncated with an ellipsis (the full body still arrives via the store).
const MAX_INJECT_CHARS: usize = 240;

/// Wall-clock cap for a single mux subprocess. A wedged tmux/zellij server must
/// never hang the caller (the MCP server serves other sessions).
const INJECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How much of a message a caller wants delivered live into the recipient's pane.
///
/// Both modes type a *single submitted line* — the difference is what that line
/// carries. `Full` injects the (sanitized, capped) message body; `Nudge` injects
/// only a quiet fixed ping that tells the recipient to check their inbox, never
/// the body itself. The authoritative copy always arrives via the store on the
/// recipient's next hook drain regardless of mode, so a `Nudge` loses no content —
/// it just keeps the body out of a busy pane / off-screen.
///
/// `Default` is `Full` to preserve today's behavior for callers that don't choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nudge {
    /// Inject the message body itself (current behavior).
    #[default]
    Full,
    /// Inject only a short, content-free ping line.
    Nudge,
}

/// The fixed text a `Nudge::Nudge` injects in place of the body. Deliberately
/// generic and short: it must read sensibly when typed into any agent's prompt
/// and must never leak the message content.
const NUDGE_PING: &str = "[weave] new message — check your inbox";

impl Nudge {
    /// The literal text to inject for this mode given the real message `body`.
    /// `Full` returns the body verbatim (sanitization/capping happen downstream
    /// in `commands_for`); `Nudge` returns the fixed ping, ignoring `body`.
    pub fn payload(self, body: &str) -> &str {
        match self {
            Nudge::Full => body,
            Nudge::Nudge => NUDGE_PING,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mux {
    Tmux,
    Zellij,
    Kitty,
    Wezterm,
    Screen,
    /// The absence of an injectable multiplexer — also the `Default`, so a
    /// `Target::default()` is the same inert, non-injectable target as
    /// `Target::none()`.
    #[default]
    None,
}

impl Mux {
    pub fn as_str(self) -> &'static str {
        match self {
            Mux::Tmux => "tmux",
            Mux::Zellij => "zellij",
            Mux::Kitty => "kitty",
            Mux::Wezterm => "wezterm",
            Mux::Screen => "screen",
            Mux::None => "none",
        }
    }

    pub fn parse(s: &str) -> Mux {
        match s {
            "tmux" => Mux::Tmux,
            "zellij" => Mux::Zellij,
            "kitty" => Mux::Kitty,
            "wezterm" => Mux::Wezterm,
            "screen" => Mux::Screen,
            _ => Mux::None,
        }
    }

    /// The CLI binary this backend drives.
    pub fn binary(self) -> &'static str {
        match self {
            Mux::Tmux => "tmux",
            Mux::Zellij => "zellij",
            Mux::Kitty => "kitten",
            Mux::Wezterm => "wezterm",
            Mux::Screen => "screen",
            Mux::None => "",
        }
    }
}

/// Where a session can be injected.
///
/// `socket` is an OPTIONAL kitty remote-control socket address (the value of
/// `KITTY_LISTEN_ON`, e.g. `unix:/tmp/mykitty` or `tcp:localhost:12345`). It is
/// empty for every other backend and ignored by them; only kitty's `commands_for`
/// arm consults it, passing `--to <socket>` so `kitten @` reaches a kitty that was
/// launched with `--listen-on` rather than relying on the default control path.
/// Defaulting it to empty keeps every existing constructor/caller working unchanged.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub mux: Mux,
    pub id: String,
    /// kitty `--to` socket (from `KITTY_LISTEN_ON`); empty = use kitty's default.
    pub socket: String,
}

impl Target {
    pub fn none() -> Self {
        Target {
            mux: Mux::None,
            id: String::new(),
            socket: String::new(),
        }
    }

    pub fn injectable(&self) -> bool {
        self.mux != Mux::None && !self.id.is_empty()
    }

    pub fn from_peer(p: &Peer) -> Self {
        Target {
            mux: Mux::parse(&p.mux),
            id: p.target.clone(),
            // Carry the peer's stored kitty remote-control socket (the value of
            // KITTY_LISTEN_ON captured at register time) so a cross-session inject
            // can reach a kitty launched with `--listen-on`. Empty for every other
            // backend (and for a kitty on its default control path), which keeps the
            // legacy `kitten @` shaping byte-for-byte unchanged.
            socket: p.socket.clone(),
        }
    }
}

/// Detect the *current* process's injectable target from environment variables
/// set by the multiplexer/terminal. Probed most- to least-specific.
pub fn detect_target() -> Target {
    // Order matters: a process can be inside tmux *and* a terminal; prefer the
    // multiplexer that owns the input line.
    if let Some(id) = nonempty_env("TMUX_PANE") {
        return Target {
            mux: Mux::Tmux,
            id,
            socket: String::new(),
        };
    }
    if let Some(id) = nonempty_env("ZELLIJ_SESSION_NAME") {
        return Target {
            mux: Mux::Zellij,
            id,
            socket: String::new(),
        };
    }
    if let Some(id) = nonempty_env("WEZTERM_PANE") {
        return Target {
            mux: Mux::Wezterm,
            id,
            socket: String::new(),
        };
    }
    if let Some(id) = nonempty_env("KITTY_WINDOW_ID") {
        return Target {
            mux: Mux::Kitty,
            id,
            // kitty only answers remote-control requests when launched with
            // `--listen-on`; it then exports the address as KITTY_LISTEN_ON.
            // Capture it so we can pass `--to <socket>`; absent it, `kitten @`
            // falls back to kitty's default control path (unchanged behavior).
            socket: nonempty_env("KITTY_LISTEN_ON").unwrap_or_default(),
        };
    }
    if let Some(id) = nonempty_env("STY") {
        return Target {
            mux: Mux::Screen,
            id,
            socket: String::new(),
        };
    }
    Target::none()
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Make `text` safe to type into a recipient's input line as a single submission.
///
/// The live nudge is only a ping — the full message body always arrives via the
/// store on the recipient's next hook drain — so we collapse interior control
/// characters here. Interior CR/LF in particular would otherwise be read by the
/// receiving TUI as Enter, prematurely submitting a partial line and executing
/// the remainder as a second command. Tabs and other control bytes are dropped
/// (a stray tab can trigger TUI completion). The result carries no terminator;
/// each backend appends its own paste-safe Enter.
fn sanitize(text: &str) -> String {
    // Map line terminators to spaces, drop every other control character (tab,
    // ESC, etc.) — they have no place in a one-line ping and can be read as TUI
    // keys — then collapse any resulting whitespace runs so e.g. a "\r\n" pair
    // does not leave a double space.
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        let mapped = if c == '\n' || c == '\r' {
            Some(' ')
        } else if c.is_control() {
            None
        } else {
            Some(c)
        };
        if let Some(ch) = mapped {
            if ch == ' ' {
                if !prev_space {
                    out.push(' ');
                }
                prev_space = true;
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
    }
    // Cap length (by char, never splitting a UTF-8 codepoint) so an oversized
    // body cannot flood the recipient's input line.
    if out.chars().count() > MAX_INJECT_CHARS {
        let mut capped: String = out.chars().take(MAX_INJECT_CHARS - 1).collect();
        capped.push('…');
        capped
    } else {
        out
    }
}

/// The exact argv command(s) that inject `text` (followed by a paste-safe Enter)
/// into `target`. Pure function — unit-tested for every backend without any
/// multiplexer present.
///
/// Returns an empty vec when there is nothing safe to inject: an un-injectable
/// target, or a `text` that is empty/whitespace-only (so we never fire a bare
/// Enter into someone's pane — the message degrades to next-turn delivery).
pub fn commands_for(target: &Target, text: &str) -> Vec<Vec<String>> {
    let id = &target.id;
    let sanitized = sanitize(text);
    if sanitized.trim().is_empty() {
        return vec![];
    }
    let text = sanitized.as_str();
    match target.mux {
        // tmux: type literal text, close bracketed-paste mode with the hex
        // ESC[201~ sequence (so the TUI doesn't treat the following Enter as a
        // key/cancel), then send Enter.
        Mux::Tmux => vec![
            argv(&["tmux", "send-keys", "-t", id, "-l", "--", text]),
            argv(&[
                "tmux",
                "send-keys",
                "-t",
                id,
                "-H",
                "1b",
                "5b",
                "32",
                "30",
                "31",
                "7e",
            ]),
            argv(&["tmux", "send-keys", "-t", id, "Enter"]),
        ],
        // zellij: write the literal chars, then write byte 13 (carriage return).
        // `--` ends option parsing so a body beginning with `-`/`--` is treated as
        // content, not as a flag to `write-chars`.
        Mux::Zellij => vec![
            argv(&[
                "zellij",
                "--session",
                id,
                "action",
                "write-chars",
                "--",
                text,
            ]),
            argv(&["zellij", "--session", id, "action", "write", "13"]),
        ],
        // kitty: requires remote control. Match the target window by id; send the
        // text, then a carriage return as a separate send-text. `--` guards against
        // a body beginning with `-` being parsed as a `send-text` option.
        //
        // When the target carries a socket (from KITTY_LISTEN_ON) we thread it in
        // as `--to <socket>` *before* the `@`, so `kitten @` talks to the kitty that
        // was launched with `--listen-on`. The `--to` global option must precede the
        // `@` subcommand. Empty socket ⇒ omit it entirely (kitty's default path).
        Mux::Kitty => {
            let m = format!("id:{id}");
            let to = &target.socket;
            let build = |payload: &str| -> Vec<String> {
                let mut a: Vec<&str> = vec!["kitten"];
                if !to.is_empty() {
                    a.push("--to");
                    a.push(to);
                }
                a.extend_from_slice(&["@", "send-text", "--match", &m, "--", payload]);
                argv(&a)
            };
            vec![build(text), build(CR)]
        }
        // wezterm: --no-paste avoids bracketed paste entirely; submit with CR.
        // `--` ends option parsing so a body beginning with `-` is content.
        Mux::Wezterm => vec![
            argv(&[
                "wezterm",
                "cli",
                "send-text",
                "--pane-id",
                id,
                "--no-paste",
                "--",
                text,
            ]),
            argv(&[
                "wezterm",
                "cli",
                "send-text",
                "--pane-id",
                id,
                "--no-paste",
                "--",
                CR,
            ]),
        ],
        // screen: `stuff` takes a single positional string. screen does not reparse
        // that argument as options, but we keep the body as one argv element so a
        // leading `-` cannot be misread.
        Mux::Screen => vec![argv(&[
            "screen",
            "-S",
            id,
            "-X",
            "stuff",
            &format!("{text}{CR}"),
        ])],
        Mux::None => vec![],
    }
}

/// Mode-aware variant of [`commands_for`]: builds the injection commands for the
/// chosen [`Nudge`] mode. `Nudge::Full` is exactly `commands_for(target, body)`;
/// `Nudge::Nudge` injects the fixed ping instead of the body. Pure (unit-tested).
pub fn commands_for_mode(target: &Target, body: &str, mode: Nudge) -> Vec<Vec<String>> {
    commands_for(target, mode.payload(body))
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The argv that asks a mux whether `target` still exists (its session/pane is
/// live). Pure + unit-tested. Returns `None` for backends with no cheap probe
/// (`screen`, `None`) — callers treat "no probe" as "don't pre-gate".
///
/// These are read-only listing/has commands; we inspect only their exit status
/// (or, for the panes/cli forms, scan stdout for the id) — see [`target_alive`].
pub fn liveness_probe(target: &Target) -> Option<Vec<String>> {
    if !target.injectable() {
        return None;
    }
    let id = &target.id;
    match target.mux {
        // `tmux has-session -t <pane>` resolves the pane's session and exits 0 iff
        // it exists. (A pane id like `%3` resolves through to its session.)
        Mux::Tmux => Some(argv(&["tmux", "has-session", "-t", id])),
        // zellij has no per-session "exists" verb; `list-sessions` enumerates them
        // and we scan stdout for the name in `target_alive`.
        Mux::Zellij => Some(argv(&["zellij", "list-sessions"])),
        // wezterm: `cli list` prints all panes; we scan stdout for the pane id.
        Mux::Wezterm => Some(argv(&["wezterm", "cli", "list"])),
        // kitty: `kitten @ ls` (honoring --to) reports the window tree as JSON; a
        // present window id means the target is live.
        Mux::Kitty => {
            let mut a: Vec<&str> = vec!["kitten"];
            if !target.socket.is_empty() {
                a.push("--to");
                a.push(&target.socket);
            }
            a.extend_from_slice(&["@", "ls"]);
            Some(argv(&a))
        }
        // screen has no cheap, scriptable existence check we trust here.
        Mux::Screen | Mux::None => None,
    }
}

/// Best-effort liveness pre-check: is `target`'s pane/session still around?
///
/// Used *opportunistically* — a `true` (or "no probe available") means "go
/// ahead and try to inject"; only a confident `false` (the probe ran and the
/// target was demonstrably absent) should steer a caller to skip injection and
/// fall straight to next-turn delivery. Because it is advisory we fail OPEN:
/// a missing mux binary, a probe error, or a timeout all return `true` so we
/// never suppress a delivery just because the probe itself was unavailable.
pub fn target_alive(target: &Target) -> bool {
    let Some(cmd) = liveness_probe(target) else {
        // No probe for this backend — don't gate; let inject try.
        return true;
    };
    let bin = target.mux.binary();
    if !have(bin) {
        // Can't probe ⇒ don't suppress; inject() will surface a real error.
        return true;
    }
    match target.mux {
        // Exit-status probes: 0 ⇒ alive, non-zero ⇒ gone, error/timeout ⇒ assume alive.
        Mux::Tmux => run_bounded(&cmd, INJECT_TIMEOUT).unwrap_or(true),
        // Stdout-scan probes: the id must appear in the listing to count as alive,
        // but any spawn/timeout failure leaves us unsure ⇒ assume alive. We match on
        // a token/field boundary, never a raw substring, so an id like "2" does not
        // spuriously match "12", a column header, or a timestamp digit run.
        Mux::Zellij | Mux::Wezterm | Mux::Kitty => {
            match run_capture(&cmd, INJECT_TIMEOUT) {
                Ok(Some(out)) => id_present(target.mux, &out, target.id.as_str()),
                // Ran but produced nothing usable, or could not be run: don't gate.
                Ok(None) | Err(_) => true,
            }
        }
        // Unreachable (liveness_probe returned None for these), but be explicit.
        Mux::Screen | Mux::None => true,
    }
}

/// Does the target `id` appear in probe `out` as a *whole token / field*, not as a
/// bare substring? `out.contains(id)` was unsafe: an id of "2" substring-matches
/// "12", a pane in another column, an epoch timestamp, etc., wrongly reporting a
/// dead target as alive (or — worse for fail-open intent — masking a real id with a
/// coincidental digit run). Boundary-aware matching per backend:
///   - zellij  `list-sessions` — session names are word-like, one per line possibly
///     with surrounding decoration; whitespace-tokenize and require an exact token.
///   - wezterm `cli list`      — columnar text; the integer pane id is its own
///     whitespace-delimited field, so the same exact-token rule isolates it.
///   - kitty   `kitten @ ls`   — JSON window tree; the window id is a numeric value
///     of an `"id"` field. Parse the JSON and look for that exact integer field
///     rather than scanning text, so "2" can't match inside "12345" or a timestamp.
///
/// Fail-open is preserved by the caller: this only ever runs on a successful probe
/// capture; an empty/garbled capture never reaches here (it returns `true` upstream).
fn id_present(mux: Mux, out: &str, id: &str) -> bool {
    match mux {
        // Exact whitespace-delimited token anywhere in the listing.
        Mux::Zellij | Mux::Wezterm => out.split_whitespace().any(|tok| tok == id),
        // Kitty emits JSON; an integer window id appears as `"id": <n>`. Match that
        // field exactly. Fall back to exact-token matching if the output isn't the
        // JSON we expect (defensive: a future kitty format change shouldn't make us
        // suppress a live target — only a confident absence gates).
        Mux::Kitty => {
            if let Ok(want) = id.parse::<i64>() {
                if let Some(found) = json_has_id(out, want) {
                    return found;
                }
            }
            // Couldn't parse the id as an int or couldn't parse the JSON: don't
            // claim a confident absence — defer to a boundary-safe token scan.
            out.split_whitespace()
                .any(|tok| tok.trim_matches(|c| c == ',' || c == ':') == id)
        }
        // The remaining backends never reach here (target_alive handles them).
        Mux::Tmux | Mux::Screen | Mux::None => out.contains(id),
    }
}

/// Scan kitty `kitten @ ls` JSON for a window whose `"id"` field equals `want`.
/// Returns `Some(true/false)` when the text is recognizably the kitty JSON (we can
/// make a confident judgement) and `None` when it doesn't look like that JSON at all
/// (caller should fall back rather than trust a parse we couldn't perform).
///
/// We avoid pulling in a JSON crate: kitty prints `"id": <int>` (with optional
/// whitespace after the colon) for every os-window / tab / window node. We hunt for
/// the literal key, then read the integer that follows and compare it as a whole
/// number — so id 2 matches `"id": 2` but never `"id": 12345` or a `"last_focused":`
/// timestamp. Presence of at least one `"id"` key is our signal the text really is
/// the expected JSON.
fn json_has_id(out: &str, want: i64) -> Option<bool> {
    let mut saw_any_id = false;
    let mut rest = out;
    while let Some(pos) = rest.find("\"id\"") {
        // Advance past the matched key.
        let after_key = &rest[pos + "\"id\"".len()..];
        // Expect a colon (allowing whitespace before it), then an integer.
        let after_colon = after_key.trim_start();
        if let Some(num_str) = after_colon.strip_prefix(':') {
            let digits: String = num_str
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(val) = digits.parse::<i64>() {
                saw_any_id = true;
                if val == want {
                    return Some(true);
                }
            }
        }
        rest = after_key;
    }
    // Recognized the JSON (had at least one numeric "id") but never matched ⇒ a
    // confident absence. Didn't recognize it at all ⇒ None so the caller falls back.
    if saw_any_id {
        Some(false)
    } else {
        None
    }
}

/// Fixed, trusted directories to resolve a mux binary from. A bare binary name
/// resolved via ambient `$PATH` lets a fake `tmux`/`zellij` placed early on PATH
/// execute inside weave's process (which may hold a libSQL auth token) on a
/// remote-triggered send. We only run a mux found by ABSOLUTE path in one of these
/// system/user-tool dirs.
fn trusted_dirs() -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> =
        ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
    // Explicit opt-in for a mux installed in a nonstandard dir (the user vouches
    // for it by setting this); also how tests point at a fake mux.
    if let Some(extra) = std::env::var_os("WEAVE_MUX_DIR") {
        v.extend(std::env::split_paths(&extra));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let h = std::path::PathBuf::from(home);
        v.push(h.join(".cargo/bin"));
        v.push(h.join(".local/bin"));
        v.push(h.join(".nix-profile/bin"));
    }
    v
}

/// Resolve `bin` to an absolute path inside a trusted dir, or `None`.
pub fn resolve_trusted(bin: &str) -> Option<std::path::PathBuf> {
    if bin.is_empty() {
        return None;
    }
    trusted_dirs()
        .into_iter()
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Is `bin` available as a TRUSTED absolute path (not just anywhere on $PATH)?
pub fn have(bin: &str) -> bool {
    resolve_trusted(bin).is_some()
}

/// Rewrite a command's argv[0] (a bare mux binary name) to its trusted absolute
/// path, so `Command::new` spawns the trusted binary rather than re-resolving via
/// ambient `$PATH`. Returns `None` if the binary isn't in a trusted dir.
fn trusted_argv(cmd: &[String]) -> Option<Vec<String>> {
    let abs = resolve_trusted(&cmd[0])?;
    let mut out = cmd.to_vec();
    out[0] = abs.to_string_lossy().into_owned();
    Some(out)
}

/// Inject `text` into `target`. Returns:
///   Ok(true)  — the literal text was typed into the pane (submission may have
///               been best-effort; see below)
///   Ok(false) — nothing to inject (mux None / empty id / empty-or-whitespace text)
///   Err(..)   — the mux binary is missing, or typing the literal text itself
///               failed (e.g. pane gone) *before* anything landed in the pane;
///               callers fall back to next-turn delivery.
///
/// The command list is non-atomic: the FIRST command types the literal text and
/// the remaining commands submit it (paste-close + Enter). Once the text has been
/// typed we do NOT return `Err` if a later submission step fails — doing so would
/// make callers treat the send as failed and re-deliver via the store *on top of*
/// a half-typed line already sitting in the recipient's prompt (duplicate +
/// dirtied input). Instead a failed submission is logged to stderr and we still
/// return `Ok(true)`: the body is the recipient's to submit, and the authoritative
/// copy still arrives on their next hook drain.
pub fn inject(target: &Target, text: &str) -> Result<bool> {
    inject_mode(target, text, Nudge::Full)
}

/// Mode-aware injection. Identical to [`inject`] but lets the caller pick a quiet
/// [`Nudge::Nudge`] ping instead of typing the full body (`Nudge::Full`, the
/// behavior of plain `inject`). All the `Ok/Err` semantics of [`inject`] hold.
///
/// Transient-failure retry: the FIRST (text-typing) command is the make-or-break
/// step — if it fails *before anything lands*, we retry it exactly once after a
/// short backoff to ride out a momentary mux hiccup (server briefly busy, pane
/// transitioning). The retry is safe precisely because a failed first command
/// means nothing was typed, so re-typing cannot duplicate. We do NOT retry later
/// submission steps: by then text is already in the pane and a re-run could append
/// a stray duplicate / extra Enter. A retry that itself fails falls back exactly as
/// before (propagate the error ⇒ caller does next-turn delivery).
/// Is `id` a structurally valid target for `mux`? Targets are captured from the
/// recipient's environment at register time, so they are attacker-influenceable;
/// we accept only each mux's expected id shape (no whitespace, no option-smuggling
/// characters, bounded length).
pub fn id_valid(mux: Mux, id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    match mux {
        // tmux exports `$TMUX_PANE` as `%<n>`.
        Mux::Tmux => {
            id.starts_with('%') && id.len() > 1 && id[1..].bytes().all(|b| b.is_ascii_digit())
        }
        // zellij session name.
        Mux::Zellij => id.len() <= 64 && id.chars().all(word),
        // kitty window id / wezterm pane id are integers.
        Mux::Kitty | Mux::Wezterm => id.bytes().all(|b| b.is_ascii_digit()),
        // screen `$STY` is `<pid>.<tty>.<host>`.
        Mux::Screen => {
            let mut parts = id.splitn(2, '.');
            let pid = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("");
            !pid.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && !rest.is_empty()
                && rest.chars().all(|c| word(c) || c == '.')
        }
        Mux::None => false,
    }
}

pub fn inject_mode(target: &Target, body: &str, mode: Nudge) -> Result<bool> {
    // Defense against a poisoned peer registration: a target id is an
    // attacker-influenceable string (it comes from the recipient's env at
    // register time). Refuse to drive a mux with an id that doesn't match that
    // mux's expected shape, so a crafted id can't redirect keystrokes to an
    // arbitrary pane or smuggle extra arguments.
    if !id_valid(target.mux, &target.id) {
        bail!(
            "refusing to inject: target id {:?} is not a valid {} target",
            target.id,
            target.mux.as_str()
        );
    }
    let cmds = commands_for_mode(target, body, mode);
    if cmds.is_empty() {
        return Ok(false);
    }
    let bin = target.mux.binary();
    if !have(bin) {
        bail!("{bin} not found on PATH (mux '{}')", target.mux.as_str());
    }
    // Opportunistic liveness pre-check: only a CONFIDENT "absent" skips injection
    // (fails open — an unavailable/timed-out probe still lets us try). Avoids
    // typing into a pane/session that has demonstrably gone away.
    if !target_alive(target) {
        bail!(
            "target '{}' on {} is not alive; falling back to next-turn delivery",
            target.id,
            target.mux.as_str()
        );
    }
    for (i, cmd) in cmds.iter().enumerate() {
        // The first command types the literal text; the rest submit it.
        if i == 0 {
            match run_with_one_retry(cmd, INJECT_TIMEOUT) {
                Ok(true) => {}
                // Retried once and still non-zero ⇒ nothing landed; fall back.
                Ok(false) => bail!("`{}` exited non-zero (after one retry)", cmd.join(" ")),
                // Retried once and still erroring ⇒ propagate; caller next-turns.
                Err(e) => return Err(e),
            }
            continue;
        }
        match run_bounded(cmd, INJECT_TIMEOUT) {
            Ok(true) => {}
            // A later (submission) command failed, but the text is already typed.
            // Don't error — warn and keep going so callers don't double-deliver.
            Ok(false) => {
                eprintln!(
                    "[weave] submission step `{}` failed after text was typed; \
                     leaving it for the recipient (full copy arrives next turn)",
                    cmd.join(" ")
                );
            }
            Err(e) => {
                eprintln!(
                    "[weave] submission step `{}` errored after text was typed: {e}; \
                     leaving it for the recipient (full copy arrives next turn)",
                    cmd.join(" ")
                );
            }
        }
    }
    Ok(true)
}

/// Backoff between the first attempt and its single retry. Short enough not to
/// add perceptible latency, long enough to outlast a momentary mux-server stall.
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(120);

/// Run a command, retrying exactly once on a transient failure (non-zero exit OR
/// spawn/timeout error). Returns the result of whichever attempt last ran. Only
/// safe for idempotent commands — here, exclusively the text-typing first command,
/// which on failure has typed nothing and so can be re-run without duplication.
fn run_with_one_retry(cmd: &[String], dur: std::time::Duration) -> Result<bool> {
    match run_bounded(cmd, dur) {
        Ok(true) => Ok(true),
        first => {
            // Transient hiccup: pause briefly, then make one more attempt whose
            // outcome (success, clean non-zero, or error) we return verbatim.
            if let Err(ref e) = first {
                eprintln!(
                    "[weave] inject step `{}` failed ({e}); retrying once",
                    cmd.join(" ")
                );
            } else {
                eprintln!(
                    "[weave] inject step `{}` exited non-zero; retrying once",
                    cmd.join(" ")
                );
            }
            std::thread::sleep(RETRY_BACKOFF);
            run_bounded(cmd, dur)
        }
    }
}

/// Run one mux command with a wall-clock cap. Returns Ok(true/false) for the exit
/// status, or Err if it cannot be spawned or it exceeds `dur` (the child is killed
/// so a wedged mux server cannot hang weave). Polls rather than pulling in a crate.
fn run_bounded(cmd: &[String], dur: std::time::Duration) -> Result<bool> {
    use std::time::Instant;
    // Spawn the mux binary by TRUSTED absolute path, never via ambient $PATH.
    let cmd = trusted_argv(cmd)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a trusted directory", cmd[0]))?;
    let mut child = Command::new(&cmd[0]).args(&cmd[1..]).spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if start.elapsed() >= dur {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{}` timed out after {:?}", cmd.join(" "), dur);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Run a listing/probe command with a wall-clock cap and capture its stdout. Used
/// by [`target_alive`] for the backends whose liveness is decided by scanning the
/// output for the target id (zellij/wezterm/kitty). Returns:
///   Ok(Some(stdout)) — ran to completion with exit 0; `stdout` is its output
///   Ok(None)         — ran but exited non-zero (output not trustworthy)
///   Err(..)          — could not spawn, or it exceeded `dur` (child killed)
/// Mirrors `run_bounded`'s timeout/kill discipline so a wedged mux can't hang us.
fn run_capture(cmd: &[String], dur: std::time::Duration) -> Result<Option<String>> {
    use std::process::Stdio;
    use std::time::Instant;
    // Spawn the probe binary by TRUSTED absolute path, never via ambient $PATH.
    let cmd = trusted_argv(cmd)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a trusted directory", cmd[0]))?;
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            // Child exited; drain whatever it wrote. wait_with_output is safe now
            // that the process has finished, and reads the piped stdout to EOF.
            let out = child.wait_with_output()?;
            if status.success() {
                return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
            }
            return Ok(None);
        }
        if start.elapsed() >= dur {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{}` timed out after {:?}", cmd.join(" "), dur);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(mux: Mux, id: &str) -> Target {
        Target {
            mux,
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tmux_is_paste_safe() {
        let c = commands_for(&t(Mux::Tmux, "%3"), "hi");
        assert_eq!(c.len(), 3, "type + paste-close + Enter");
        assert_eq!(
            c[0],
            argv(&["tmux", "send-keys", "-t", "%3", "-l", "--", "hi"])
        );
        // ESC[201~ closes bracketed paste before Enter.
        assert_eq!(
            c[1],
            argv(&[
                "tmux",
                "send-keys",
                "-t",
                "%3",
                "-H",
                "1b",
                "5b",
                "32",
                "30",
                "31",
                "7e"
            ])
        );
        assert_eq!(c[2], argv(&["tmux", "send-keys", "-t", "%3", "Enter"]));
    }

    #[test]
    fn zellij_writes_cr() {
        let c = commands_for(&t(Mux::Zellij, "envctl"), "hi");
        assert_eq!(
            c[0],
            argv(&[
                "zellij",
                "--session",
                "envctl",
                "action",
                "write-chars",
                "--",
                "hi"
            ])
        );
        assert_eq!(
            c[1],
            argv(&["zellij", "--session", "envctl", "action", "write", "13"])
        );
    }

    #[test]
    fn kitty_matches_window() {
        let c = commands_for(&t(Mux::Kitty, "7"), "hi");
        assert_eq!(
            c[0],
            argv(&["kitten", "@", "send-text", "--match", "id:7", "--", "hi"])
        );
        assert_eq!(
            c[1],
            argv(&["kitten", "@", "send-text", "--match", "id:7", "--", "\r"])
        );
    }

    #[test]
    fn wezterm_no_paste() {
        let c = commands_for(&t(Mux::Wezterm, "2"), "hi");
        assert_eq!(
            c[0],
            argv(&[
                "wezterm",
                "cli",
                "send-text",
                "--pane-id",
                "2",
                "--no-paste",
                "--",
                "hi"
            ])
        );
    }

    #[test]
    fn screen_stuffs_cr() {
        let c = commands_for(&t(Mux::Screen, "1234.pts-0.host"), "hi");
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0],
            argv(&["screen", "-S", "1234.pts-0.host", "-X", "stuff", "hi\r"])
        );
    }

    #[test]
    fn none_is_not_injectable() {
        assert!(commands_for(&Target::none(), "hi").is_empty());
        assert!(!Target::none().injectable());
    }

    #[test]
    fn binaries_map() {
        assert_eq!(Mux::Kitty.binary(), "kitten");
        assert_eq!(Mux::Tmux.binary(), "tmux");
        assert_eq!(Mux::parse("wezterm"), Mux::Wezterm);
    }

    /// A body beginning with `-`/`--` must land as literal content, never be
    /// parsed as a flag by the backend CLI. The end-of-options `--` separator
    /// guarantees the value is the last positional argument.
    #[test]
    fn leading_dash_body_is_content_not_a_flag() {
        for (mux, id) in [
            (Mux::Tmux, "%3"),
            (Mux::Zellij, "envctl"),
            (Mux::Kitty, "7"),
            (Mux::Wezterm, "2"),
            (Mux::Screen, "1234.pts-0.host"),
        ] {
            for body in ["--help", "-n", "--from-file", "--no-paste", "--pane-id"] {
                let c = commands_for(&t(mux, id), body);
                let first = &c[0];
                // The body is the final argv element of the first (text-typing)
                // command, and the element immediately before it is the `--`
                // end-of-options marker (screen embeds the body+CR as one arg,
                // and screen does not reparse `stuff`'s positional as options).
                if mux == Mux::Screen {
                    assert_eq!(
                        first.last().map(String::as_str),
                        Some(format!("{body}\r").as_str()),
                        "screen body must be a single positional arg: {first:?}"
                    );
                } else {
                    assert_eq!(
                        first.last().map(String::as_str),
                        Some(body),
                        "{}: body must be the last positional: {first:?}",
                        mux.as_str()
                    );
                    assert_eq!(
                        first.get(first.len() - 2).map(String::as_str),
                        Some("--"),
                        "{}: body must be guarded by an end-of-options `--`: {first:?}",
                        mux.as_str()
                    );
                }
            }
        }
    }

    /// Empty or whitespace-only text must NOT inject — otherwise a bare Enter is
    /// fired into the recipient's pane, submitting an empty prompt. It degrades
    /// to next-turn delivery instead.
    #[test]
    fn empty_or_whitespace_text_does_not_inject() {
        for (mux, id) in [
            (Mux::Tmux, "%3"),
            (Mux::Zellij, "envctl"),
            (Mux::Kitty, "7"),
            (Mux::Wezterm, "2"),
            (Mux::Screen, "1234.pts-0.host"),
        ] {
            for body in ["", "   ", "\n", "\r\n", "\t  \n"] {
                assert!(
                    commands_for(&t(mux, id), body).is_empty(),
                    "{}: text {body:?} must yield no commands",
                    mux.as_str()
                );
            }
        }
    }

    /// Interior newlines must be collapsed so a multiline body becomes a single
    /// submission instead of fragmenting at the first `\n`.
    #[test]
    fn multiline_body_is_collapsed_to_one_line() {
        // tmux: the literal-text command is the first one; its last arg is text.
        let c = commands_for(&t(Mux::Tmux, "%3"), "line one\nline two\r\nthree");
        let typed = c[0].last().unwrap();
        assert!(
            !typed.contains('\n') && !typed.contains('\r'),
            "interior CR/LF must be stripped from the typed text: {typed:?}"
        );
        assert_eq!(typed, "line one line two three");

        // screen embeds the body+trailing CR as one arg: only the single trailing
        // CR may remain, no interior newlines.
        let s = commands_for(&t(Mux::Screen, "x"), "a\nb");
        let arg = s[0].last().unwrap();
        assert_eq!(arg, "a b\r");
    }

    /// Control characters (tab, ESC, …) are dropped from the one-line ping so
    /// they can't be read as TUI keys.
    #[test]
    fn control_chars_are_stripped() {
        let c = commands_for(&t(Mux::Tmux, "%3"), "a\tb\x1bc");
        assert_eq!(c[0].last().unwrap(), "abc");
    }

    /// An oversized body is capped (with an ellipsis) so it can't flood the
    /// recipient's input line — the full copy still arrives via the store.
    #[test]
    fn oversized_text_is_capped() {
        let big = "x".repeat(10_000);
        let c = commands_for(&t(Mux::Tmux, "%3"), &big);
        let typed = c[0].last().unwrap();
        let n = typed.chars().count();
        assert_eq!(n, MAX_INJECT_CHARS, "capped to MAX_INJECT_CHARS");
        assert!(typed.ends_with('…'), "truncation marker present");
    }

    /// Multibyte content is capped on a char boundary (never splits a codepoint).
    #[test]
    fn cap_respects_utf8_boundaries() {
        let big = "é".repeat(10_000);
        let c = commands_for(&t(Mux::Tmux, "%3"), &big);
        // Building the String at all proves no panic on a non-char-boundary slice.
        assert!(c[0].last().unwrap().chars().count() <= MAX_INJECT_CHARS);
    }

    /// Build a kitty target carrying an explicit remote-control socket.
    fn kitty_with_socket(id: &str, socket: &str) -> Target {
        Target {
            mux: Mux::Kitty,
            id: id.into(),
            socket: socket.into(),
        }
    }

    /// With KITTY_LISTEN_ON present (socket non-empty) every kitty command must
    /// thread `--to <socket>` in *before* the `@` subcommand, and the body must
    /// still be the final argument guarded by `--`.
    #[test]
    fn kitty_honors_listen_socket() {
        let sock = "unix:/tmp/mykitty";
        let c = commands_for(&kitty_with_socket("7", sock), "hi");
        assert_eq!(
            c[0],
            argv(&[
                "kitten",
                "--to",
                sock,
                "@",
                "send-text",
                "--match",
                "id:7",
                "--",
                "hi"
            ])
        );
        // CR submission carries the same --to.
        assert_eq!(
            c[1],
            argv(&[
                "kitten",
                "--to",
                sock,
                "@",
                "send-text",
                "--match",
                "id:7",
                "--",
                "\r"
            ])
        );
        // `--to <socket>` precedes `@`.
        let at = c[0].iter().position(|s| s == "@").unwrap();
        let to = c[0].iter().position(|s| s == "--to").unwrap();
        assert!(to < at, "--to must come before @: {:?}", c[0]);
    }

    /// Without a socket (default), kitty shaping is byte-for-byte the legacy form:
    /// no `--to` is emitted, preserving backward compatibility.
    #[test]
    fn kitty_without_socket_is_unchanged() {
        let c = commands_for(&kitty_with_socket("7", ""), "hi");
        assert_eq!(
            c[0],
            argv(&["kitten", "@", "send-text", "--match", "id:7", "--", "hi"])
        );
        assert!(
            !c[0].iter().any(|s| s == "--to"),
            "no --to when socket is empty: {:?}",
            c[0]
        );
    }

    /// A leading-dash body must still land as content even with a socket present
    /// (the `--` end-of-options guard is unaffected by `--to`).
    #[test]
    fn kitty_socket_leading_dash_body_is_content() {
        let c = commands_for(&kitty_with_socket("7", "tcp:localhost:9"), "--help");
        let first = &c[0];
        assert_eq!(first.last().map(String::as_str), Some("--help"));
        assert_eq!(
            first.get(first.len() - 2).map(String::as_str),
            Some("--"),
            "body guarded by end-of-options --: {first:?}"
        );
    }

    /// `Target::default()` / `from_peer` leave the socket empty, so plain
    /// constructors keep producing the legacy kitty commands.
    #[test]
    fn target_socket_defaults_empty() {
        assert!(Target::default().socket.is_empty());
        assert!(Target::none().socket.is_empty());
        let p = Peer {
            name: "x".into(),
            mux: "kitty".into(),
            target: "7".into(),
            cwd: None,
            socket: String::new(),
            last_seen: 0,
        };
        assert!(Target::from_peer(&p).socket.is_empty());
    }

    /// `from_peer` copies a peer's stored kitty socket into the Target so a
    /// cross-session inject can reach a kitty launched with `--listen-on`.
    #[test]
    fn from_peer_carries_kitty_socket() {
        let p = Peer {
            name: "k".into(),
            mux: "kitty".into(),
            target: "7".into(),
            cwd: None,
            socket: "unix:/tmp/mykitty".into(),
            last_seen: 0,
        };
        let target = Target::from_peer(&p);
        assert_eq!(target.socket, "unix:/tmp/mykitty");
        assert_eq!(target.mux, Mux::Kitty);
        assert_eq!(target.id, "7");
        // And that socket actually threads through to the shaped commands.
        let c = commands_for(&target, "hi");
        assert_eq!(
            c[0],
            argv(&[
                "kitten",
                "--to",
                "unix:/tmp/mykitty",
                "@",
                "send-text",
                "--match",
                "id:7",
                "--",
                "hi"
            ])
        );
    }

    /// `Nudge::Full` injects the body verbatim; `Nudge::Nudge` injects only the
    /// fixed ping and never the body.
    #[test]
    fn nudge_flag_chooses_payload() {
        let target = t(Mux::Tmux, "%3");
        let full = commands_for_mode(&target, "secret body", Nudge::Full);
        assert_eq!(full[0].last().unwrap(), "secret body");

        let ping = commands_for_mode(&target, "secret body", Nudge::Nudge);
        let typed = ping[0].last().unwrap();
        assert_eq!(typed, NUDGE_PING);
        assert!(
            !typed.contains("secret"),
            "nudge mode must not leak the body: {typed:?}"
        );
    }

    /// `Full` is the default mode, and `commands_for_mode(.., Full)` equals plain
    /// `commands_for`.
    #[test]
    fn nudge_default_is_full_and_matches_commands_for() {
        assert_eq!(Nudge::default(), Nudge::Full);
        let target = t(Mux::Zellij, "envctl");
        assert_eq!(
            commands_for_mode(&target, "hello", Nudge::Full),
            commands_for(&target, "hello")
        );
    }

    /// `Nudge::payload` returns the body for `Full` and the ping for `Nudge`.
    #[test]
    fn nudge_payload_selects_text() {
        assert_eq!(Nudge::Full.payload("body"), "body");
        assert_eq!(Nudge::Nudge.payload("body"), NUDGE_PING);
    }

    /// Liveness probe argv shaping per backend (pure, no mux required).
    #[test]
    fn liveness_probe_shapes_per_backend() {
        assert_eq!(
            liveness_probe(&t(Mux::Tmux, "%3")),
            Some(argv(&["tmux", "has-session", "-t", "%3"]))
        );
        assert_eq!(
            liveness_probe(&t(Mux::Zellij, "envctl")),
            Some(argv(&["zellij", "list-sessions"]))
        );
        assert_eq!(
            liveness_probe(&t(Mux::Wezterm, "2")),
            Some(argv(&["wezterm", "cli", "list"]))
        );
        // kitty without socket: plain `kitten @ ls`.
        assert_eq!(
            liveness_probe(&t(Mux::Kitty, "7")),
            Some(argv(&["kitten", "@", "ls"]))
        );
        // kitty with socket: `--to <socket>` before `@`.
        assert_eq!(
            liveness_probe(&kitty_with_socket("7", "unix:/tmp/k")),
            Some(argv(&["kitten", "--to", "unix:/tmp/k", "@", "ls"]))
        );
        // No cheap probe for screen / none / un-injectable.
        assert_eq!(liveness_probe(&t(Mux::Screen, "x")), None);
        assert_eq!(liveness_probe(&Target::none()), None);
        assert_eq!(
            liveness_probe(&t(Mux::Tmux, "")),
            None,
            "empty id ⇒ no probe"
        );
    }

    /// Backends without a probe are never gated: `target_alive` returns true so
    /// injection is still attempted (advisory, fail-open).
    #[test]
    fn target_alive_is_open_for_unprobed_backends() {
        // screen has no probe → always "alive" (don't suppress).
        assert!(target_alive(&t(Mux::Screen, "1234.pts-0.host")));
        // None / un-injectable → no probe → true (inject() itself no-ops on these).
        assert!(target_alive(&Target::none()));
    }

    #[test]
    fn id_valid_accepts_real_rejects_malicious() {
        assert!(id_valid(Mux::Tmux, "%3"));
        assert!(id_valid(Mux::Zellij, "envctl"));
        assert!(id_valid(Mux::Kitty, "7"));
        assert!(id_valid(Mux::Screen, "1234.pts-0.host"));
        // malicious / malformed ids are refused
        assert!(!id_valid(Mux::Tmux, "%3; rm -rf /"));
        assert!(!id_valid(Mux::Zellij, "a b"));
        assert!(!id_valid(Mux::Zellij, "--listen-on=evil"));
        assert!(!id_valid(Mux::Wezterm, "1 2"));
        assert!(!id_valid(Mux::Tmux, "3"));
        assert!(!id_valid(Mux::None, "x"));
        assert!(!id_valid(Mux::Kitty, ""));
    }

    /// Liveness scanning must match the target id on a token/field boundary, never
    /// as a raw substring: an id of "2" must NOT be satisfied by "12" / a timestamp
    /// / a longer pane id, but MUST match when "2" stands alone as its own field.
    #[test]
    fn id_present_matches_on_token_boundary() {
        // wezterm `cli list` is columnar: the pane id is its own whitespace field.
        let wez = "WINID TABID PANEID TITLE\n0 0 12 vim\n0 0 2 bash\n";
        assert!(
            id_present(Mux::Wezterm, wez, "2"),
            "standalone pane id 2 must match"
        );
        assert!(
            id_present(Mux::Wezterm, wez, "12"),
            "standalone pane id 12 must match"
        );
        // An id present only as a substring of another field must NOT match.
        let only12 = "WINID TABID PANEID TITLE\n0 0 12 vim\n";
        assert!(
            !id_present(Mux::Wezterm, only12, "2"),
            "id 2 must not substring-match 12: {only12:?}"
        );

        // zellij `list-sessions`: session names are word tokens, one per line.
        let zj = "envctl [Created 1h ago]\ndesktop [Created 2m ago]\n";
        assert!(id_present(Mux::Zellij, zj, "envctl"));
        assert!(id_present(Mux::Zellij, zj, "desktop"));
        assert!(
            !id_present(Mux::Zellij, zj, "env"),
            "a prefix of a session name must not match"
        );
    }

    /// kitty `kitten @ ls` is JSON; the window id must match the numeric `"id"`
    /// field exactly — never a substring of a longer id, and never a coincidental
    /// digit run inside a timestamp field like `last_focused`.
    #[test]
    fn id_present_kitty_parses_json_id() {
        let json = r#"[
          {"id": 1, "tabs": [
            {"id": 3, "windows": [
              {"id": 2, "last_focused": 1717000002, "title": "claude"},
              {"id": 12345, "last_focused": 1717000012, "title": "shell"}
            ]}
          ]}
        ]"#;
        // Exact window id present.
        assert!(id_present(Mux::Kitty, json, "2"), "window id 2 is present");
        assert!(
            id_present(Mux::Kitty, json, "12345"),
            "window id 12345 is present"
        );
        // An id that appears ONLY as a digit run inside a timestamp must not match.
        assert!(
            !id_present(Mux::Kitty, json, "1717000002"),
            "a last_focused timestamp must not count as a window id"
        );
        // An absent id, even though its digits substring-match a present id.
        assert!(
            !id_present(Mux::Kitty, json, "234"),
            "234 substrings 12345 but is not an id field"
        );
        assert!(
            !id_present(Mux::Kitty, json, "9"),
            "absent window id must not match"
        );
    }

    /// `json_has_id` distinguishes a confident absence (recognized JSON, no match →
    /// Some(false)) from unrecognized output (no numeric id field → None, so the
    /// caller can fall back rather than wrongly gate).
    #[test]
    fn json_has_id_confidence() {
        let json = r#"{"id": 7, "tabs": []}"#;
        assert_eq!(json_has_id(json, 7), Some(true));
        assert_eq!(json_has_id(json, 8), Some(false), "recognized but absent");
        // Output with no numeric "id" field at all ⇒ None (unrecognized, fall back).
        assert_eq!(json_has_id("not json at all", 7), None);
        assert_eq!(json_has_id("", 7), None);
    }

    /// When kitty output isn't the expected JSON, `id_present` falls back to a
    /// boundary-safe token scan (trimming JSON punctuation) rather than gating.
    #[test]
    fn id_present_kitty_falls_back_on_non_json() {
        // A degenerate/plain line (not the id JSON) carrying the id as a token.
        assert!(id_present(Mux::Kitty, "window 7 active", "7"));
        // The id as a bare substring of a longer token must still not match.
        assert!(!id_present(Mux::Kitty, "window 77 active", "7"));
    }
}
