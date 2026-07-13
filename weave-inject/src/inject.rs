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

use anyhow::{bail, Result};
use std::process::Command;
use weave_core::model::Peer;

/// A carriage return — the byte a TUI reads as "Enter".
const CR: &str = "\r";

/// Hard cap on injected characters. A nudge is a short ping; a hostile or huge
/// message body must never flood the recipient's input line. Anything longer is
/// truncated with an ellipsis (the full body still arrives via the store).
pub const MAX_INJECT_CHARS: usize = 240;

/// WL-047 spawn input caps. A spawned child's argv is attacker-influenceable on the
/// MCP/remote surface, so bound both the number of argv elements and each element's
/// length before any of them is handed to a mux. These are generous (a real agent
/// launch is a handful of short args) but finite — an unbounded/huge argv must never
/// reach `Command`.
pub const MAX_SPAWN_ARGS: usize = 64;
/// Hard cap on the length (bytes) of a single child-argv element.
pub const MAX_SPAWN_ARG_LEN: usize = 4096;

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
    /// iTerm2 (macOS). Uses AppleScript `osascript`; injection targets the
    /// current session of the current window. `id` is a placeholder (the
    /// `TERM_SESSION_ID` when available); pane-level targeting is not supported.
    ITerm2,
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
            Mux::ITerm2 => "iterm2",
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
            "iterm2" | "iterm" => Mux::ITerm2,
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
            Mux::ITerm2 => "osascript",
            Mux::None => "",
        }
    }
}

/// Where a session can be injected.
///
/// `socket` is an OPTIONAL backend-specific auxiliary identifier:
///   - kitty: remote-control socket address (`KITTY_LISTEN_ON`, e.g.
///     `unix:/tmp/mykitty` or `tcp:localhost:12345`). Only kitty's `commands_for`
///     arm consults it, passing `--to <socket>` so `kitten @` reaches a kitty
///     launched with `--listen-on`.
///   - zellij: pane id (`ZELLIJ_PANE_ID`, e.g. `13` or `terminal_1`). When
///     present, `commands_for` passes `--pane-id <socket>` so `write-chars`
///     hits the correct pane instead of the currently focused one.
///   - every other backend: empty and ignored.
///
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
            // Carry the peer's stored auxiliary identifier:
            //   - kitty: remote-control socket (`KITTY_LISTEN_ON`)
            //   - zellij: pane id (`ZELLIJ_PANE_ID`)
            //   - every other backend: empty (ignored).
            socket: p.socket.clone(),
        }
    }
}

/// Detect the *current* process's injectable target from environment variables
/// set by the multiplexer/terminal. Probed most- to least-specific.
pub fn detect_target() -> Target {
    detect_target_with_preference(None)
}

/// Detect the current multiplexer target, with an optional preference override.
///
/// If `preferred` is `Some(mux)`, check ONLY that mux's env var and return the
/// corresponding target (or `Target::none()` if the env var is absent).
/// If `preferred` is `None`, use the normal auto-detection order:
/// tmux → zellij → wezterm → kitty → screen.
pub fn detect_target_with_preference(preferred: Option<Mux>) -> Target {
    // When a preference is set, check only that mux.
    if let Some(mux) = preferred {
        return match mux {
            Mux::Tmux => nonempty_env("TMUX_PANE").map(|id| Target {
                mux: Mux::Tmux,
                id,
                socket: tmux_socket_from_env(),
            }),
            Mux::Zellij => nonempty_env("ZELLIJ_SESSION_NAME").map(|id| Target {
                mux: Mux::Zellij,
                id,
                socket: nonempty_env("ZELLIJ_PANE_ID").unwrap_or_default(),
            }),
            Mux::Wezterm => nonempty_env("WEZTERM_PANE").map(|id| Target {
                mux: Mux::Wezterm,
                id,
                socket: String::new(),
            }),
            Mux::Kitty => nonempty_env("KITTY_WINDOW_ID").map(|id| Target {
                mux: Mux::Kitty,
                id,
                socket: nonempty_env("KITTY_LISTEN_ON").unwrap_or_default(),
            }),
            Mux::Screen => nonempty_env("STY").map(|id| Target {
                mux: Mux::Screen,
                id,
                socket: String::new(),
            }),
            Mux::ITerm2 => {
                // iTerm2 sets TERM_PROGRAM to "iTerm.app". The session id comes from
                // TERM_SESSION_ID (e.g. "w0t0p0:ABC123"); when absent we still register
                // as injectable so the peer can receive next-turn delivery even though
                // we can't target a specific pane.
                let id = nonempty_env("TERM_SESSION_ID").unwrap_or_else(|| "iterm2".to_string());
                Some(Target {
                    mux: Mux::ITerm2,
                    id,
                    socket: String::new(),
                })
            }
            Mux::None => Some(Target::none()),
        }
        .unwrap_or_else(Target::none);
    }

    // Order matters: a process can be inside tmux *and* a terminal; prefer the
    // multiplexer that owns the input line.
    if let Some(id) = nonempty_env("TMUX_PANE") {
        return Target {
            mux: Mux::Tmux,
            id,
            socket: tmux_socket_from_env(),
        };
    }
    if let Some(id) = nonempty_env("ZELLIJ_SESSION_NAME") {
        return Target {
            mux: Mux::Zellij,
            id,
            socket: nonempty_env("ZELLIJ_PANE_ID").unwrap_or_default(),
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
    if std::env::var("TERM_PROGRAM").ok().as_deref() == Some("iTerm.app") {
        let id = nonempty_env("TERM_SESSION_ID").unwrap_or_else(|| "iterm2".to_string());
        return Target {
            mux: Mux::ITerm2,
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
        Mux::Tmux => {
            let s = target.socket.as_str();
            vec![
                tmux_argv(s, &["send-keys", "-t", id, "-l", "--", text]),
                tmux_argv(
                    s,
                    &[
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
                    ],
                ),
                tmux_argv(s, &["send-keys", "-t", id, "Enter"]),
            ]
        }
        // zellij: write the literal chars, then write byte 13 (carriage return).
        // `--` ends option parsing so a body beginning with `-`/`--` is treated as
        // content, not as a flag to `write-chars`.
        // When a pane id was captured at registration, pass `--pane-id` so the
        // text reaches the correct pane instead of whichever pane happens to be
        // focused in the target session.
        Mux::Zellij => {
            let pane = &target.socket;
            let mut wc = vec!["zellij", "--session", id, "action", "write-chars"];
            if !pane.is_empty() {
                wc.push("--pane-id");
                wc.push(pane);
            }
            wc.push("--");
            wc.push(text);
            let mut wr = vec!["zellij", "--session", id, "action", "write"];
            if !pane.is_empty() {
                wr.push("--pane-id");
                wr.push(pane);
            }
            wr.push("13");
            vec![argv(&wc), argv(&wr)]
        }
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
        // iTerm2: AppleScript `write text` sends the literal string followed by Enter.
        // We escape backslashes and double quotes so the AppleScript string is safe.
        // The text is already sanitized (control chars stripped) before reaching here.
        Mux::ITerm2 => {
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            vec![argv(&[
                "osascript",
                "-e",
                &format!(
                    "tell application \"iTerm2\" to tell current session of current window to write text \"{escaped}\""
                ),
            ])]
        }
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

/// Extract the tmux server **socket path** from `$TMUX` (WL-053). tmux exports
/// `$TMUX` as `<socket>,<server-pid>,<session-id>`; the socket is the field before
/// the first comma — a path like `/tmp/tmux-1000/default`, or a non-default one set
/// via `tmux -L <label>` (`…/<label>`) or `tmux -S <path>`. Returns `""` when not
/// inside tmux or `$TMUX` is malformed (⇒ fall back to tmux's default server).
///
/// Capturing this at registration lets a later `inject`/`spawn`/`kill` reach the
/// ORIGINATING server via `tmux -S <socket>` instead of silently hitting whatever
/// `$TMUX` points at in the acting process (the WL-047 `/verify` failure mode).
fn tmux_socket_from_env() -> String {
    nonempty_env("TMUX")
        .map(|v| parse_tmux_socket(&v))
        .unwrap_or_default()
}

/// Pure parse of a `$TMUX` value (`<socket>,<pid>,<session>`) to its socket path.
/// Returns `""` for an empty/malformed value (no socket field).
fn parse_tmux_socket(tmux: &str) -> String {
    tmux.split(',').next().unwrap_or("").to_string()
}

/// Build a tmux argv `tmux [-S <socket>] <rest…>`. When `socket` is non-empty the
/// `-S <socket>` server selector is inserted so the command targets the captured
/// server (WL-053); empty `socket` yields the historical `tmux <rest…>` (default
/// server), so behavior is unchanged for peers registered without a socket.
fn tmux_argv(socket: &str, rest: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(rest.len() + 3);
    v.push("tmux".to_string());
    if !socket.is_empty() {
        v.push("-S".to_string());
        v.push(socket.to_string());
    }
    v.extend(rest.iter().map(|s| s.to_string()));
    v
}

/// Is `s` an acceptable single child-argv element for a spawn (WL-047)?
///
/// Argv is passed to the OS as discrete elements — never concatenated into a shell
/// string — so there is no shell-metacharacter parsing to defend against here. What
/// we *do* enforce, matching the input-cap invariant, is: a length cap
/// ([`MAX_SPAWN_ARG_LEN`]) and a rejection of embedded NUL / other control bytes
/// (a NUL would truncate the arg at the libc boundary; stray control bytes have no
/// place in a program name or option and could confuse a mux that reparses its own
/// args). An empty string IS allowed (a program may legitimately take an empty
/// positional); the argv as a whole is bounded by [`MAX_SPAWN_ARGS`] at the caller.
pub fn spawn_arg_ok(s: &str) -> bool {
    s.len() <= MAX_SPAWN_ARG_LEN && !s.chars().any(|c| c == '\0' || c.is_control())
}

/// The exact argv command(s) that ask `mux` to open a new pane (or, when `window`,
/// a new window/tab) in `cwd` running `argv_child`. Pure function — unit-tested for
/// every backend without any multiplexer present (the whole reason the injector's
/// command tables are free functions).
///
/// `name` is the spawned child's weave identity; some muxes (kitty) let us inject it
/// as `--env` belt-and-suspenders with the runner's `Command::envs`. `cert` is the
/// WL-018 birth certificate threaded the same way for kitty.
///
/// Returns an EMPTY vec for muxes that cannot cleanly spawn (iTerm2, None) — those
/// are not errors, the caller reports "spawn not supported" (the same fail-open
/// posture as [`liveness_probe`] returning `None`). Every attacker-influenceable
/// positional — `cwd` and each child-argv element — sits after an end-of-options
/// `--`, so a child arg beginning with `-` is content, never a flag to the mux.
pub fn spawn_commands(
    mux: Mux,
    socket: &str,
    cwd: &str,
    name: &str,
    cert: &str,
    argv_child: &[String],
    window: bool,
) -> Vec<Vec<String>> {
    if argv_child.is_empty() {
        return vec![];
    }
    // Borrow the child argv as &str for the argv builders.
    let child: Vec<&str> = argv_child.iter().map(String::as_str).collect();
    match mux {
        // tmux: `-P -F '#{pane_id}'` makes tmux PRINT the new pane id on stdout so
        // the runner can capture it. `-c <cwd>` sets the working dir. `--` guards the
        // child argv. window=true ⇒ a new window, else a split pane. `-S <socket>`
        // (WL-053) pins the new pane to the caller's own tmux server, not the default.
        Mux::Tmux => {
            let verb = if window { "new-window" } else { "split-window" };
            let head = tmux_argv(socket, &[verb, "-P", "-F", "#{pane_id}", "-c", cwd, "--"]);
            let mut a: Vec<String> = head;
            a.extend(child.iter().map(|s| s.to_string()));
            vec![a]
        }
        // zellij does NOT echo a usable new-pane id, so we cannot pre-register a
        // target — the child self-registers from its env at SessionStart. We simply
        // open the pane/tab in the named session running the child after `--`.
        Mux::Zellij => {
            let verb = if window { "new-tab" } else { "new-pane" };
            // `new-pane`/`new-tab` take `-- <cmd...>`. `-c` (cwd) is not portable across
            // zellij actions; rely on the runner's spawn cwd instead. (Documented.)
            let mut a: Vec<&str> = vec!["zellij", "action", verb, "--"];
            a.extend_from_slice(&child);
            vec![argv(&a)]
        }
        // kitty: `kitten @ launch` PRINTS the new window id on stdout. `--cwd <cwd>`
        // sets the dir; `--env K=V` injects identity directly (belt-and-suspenders).
        // window=true ⇒ a new OS window, else a tab in the current window. `--` guards
        // the child argv. (No `--to` socket here: a spawn targets the local kitty the
        // child will live in; the child captures its own KITTY_LISTEN_ON at register.)
        Mux::Kitty => {
            let kind = if window { "os-window" } else { "tab" };
            let env_session = format!("WEAVE_SESSION={name}");
            let env_cert = format!("WEAVE_BIRTH_CERT={cert}");
            let mut a: Vec<&str> = vec![
                "kitten",
                "@",
                "launch",
                "--type",
                kind,
                "--cwd",
                cwd,
                "--env",
                &env_session,
            ];
            if !cert.is_empty() {
                a.push("--env");
                a.push(&env_cert);
            }
            a.push("--");
            a.extend_from_slice(&child);
            vec![argv(&a)]
        }
        // wezterm: `cli spawn` PRINTS the new pane id on stdout. `--cwd <cwd>` sets the
        // dir; `--new-window` for a window, else a pane in the current tab. wezterm
        // `spawn` takes no `--env`, so identity rides the runner's `Command::envs`.
        Mux::Wezterm => {
            let mut a: Vec<&str> = vec!["wezterm", "cli", "spawn"];
            if window {
                a.push("--new-window");
            }
            a.extend_from_slice(&["--cwd", cwd, "--"]);
            a.extend_from_slice(&child);
            vec![argv(&a)]
        }
        // screen: open a new window in session `<name-of-existing-session>`... but we
        // do not have the parent's screen session here, and screen does not echo a new
        // target id. Spawn is best-effort: open a NEW detached session named after the
        // child so the child OWNS its session (cleaner kill), running the child argv.
        // screen reparses nothing after the command, but keep each element discrete.
        Mux::Screen => {
            // `screen -dmS <name> <cmd...>` starts a detached session running cmd.
            let mut a: Vec<&str> = vec!["screen", "-dmS", name];
            a.extend_from_slice(&child);
            vec![argv(&a)]
        }
        // iTerm2 has no argv-clean spawn (AppleScript only) and None is not spawnable.
        Mux::ITerm2 | Mux::None => vec![],
    }
}

/// The per-mux kill argv for an existing [`Target`]. Pure + unit-tested. Returns an
/// empty vec for muxes with no clean kill (iTerm2, None). For zellij/screen the kill
/// is COARSE/best-effort (documented): zellij closes the focused pane of the session
/// and screen quits the named session — neither is a precise per-pane guarantee.
pub fn kill_commands(target: &Target) -> Vec<Vec<String>> {
    let id = &target.id;
    match target.mux {
        // tmux: kill the pane by id (`%<n>`).
        Mux::Tmux => vec![tmux_argv(target.socket.as_str(), &["kill-pane", "-t", id])],
        // wezterm: kill the pane by id.
        Mux::Wezterm => vec![argv(&["wezterm", "cli", "kill-pane", "--pane-id", id])],
        // kitty: close the window matched by id. Thread `--to <socket>` before `@`
        // when the peer carries a KITTY_LISTEN_ON socket, exactly as commands_for.
        Mux::Kitty => {
            let m = format!("id:{id}");
            let mut a: Vec<&str> = vec!["kitten"];
            if !target.socket.is_empty() {
                a.push("--to");
                a.push(&target.socket);
            }
            a.extend_from_slice(&["@", "close-window", "--match", &m]);
            vec![argv(&a)]
        }
        // zellij: COARSE. Kill the whole session the agent lives in (`delete-session
        // --force`) — safer than "close-pane" (which closes only the focused pane).
        // The target id is the session name.
        Mux::Zellij => vec![argv(&["zellij", "delete-session", "--force", id])],
        // screen: COARSE. Quit the named session (`-X quit`) — when the spawned agent
        // owns its own session (see spawn_commands) this cleanly tears it down.
        Mux::Screen => vec![argv(&["screen", "-S", id, "-X", "quit"])],
        // No clean kill for iTerm2 (AppleScript) / None.
        Mux::ITerm2 | Mux::None => vec![],
    }
}

/// Outcome of a [`spawn`] call: whether the mux launched the child, and the new
/// target id when the mux echoed one (tmux/kitty/wezterm). An empty `target` means
/// the mux does not report an id (zellij/screen) — the child self-registers its own
/// target at SessionStart from the env we threaded in, so an empty target here is
/// expected, not a failure.
#[derive(Debug, Clone, Default)]
pub struct SpawnOutcome {
    /// `true` iff the launch command ran (exit 0). `false` is unused today (a failed
    /// launch surfaces as `Err`), kept for forward-compatible reporting.
    pub launched: bool,
    /// The captured new pane/window id (`%3`, `7`, …), or empty when the mux does not
    /// echo one. When non-empty the caller MAY pre-register the peer row.
    pub target: String,
}

/// Launch `argv_child` in a new pane/window of `mux`, in `cwd`, threading the child's
/// identity (`WEAVE_SESSION=name`), birth cert (`WEAVE_BIRTH_CERT=cert`) and optional
/// `WEAVE_CIRCLE` into its environment (via the runner's `Command::envs`). Mirrors
/// [`inject_mode`]'s discipline: resolve the mux binary by TRUSTED absolute path,
/// run bounded, and — for muxes that echo a new id — capture it from stdout.
///
/// Returns `Ok(SpawnOutcome{ launched:true, target })` on success (`target` empty for
/// muxes that do not report an id), `Ok(SpawnOutcome::default())` (launched:false) for
/// an unspawnable mux (iTerm2/None), or `Err` when the mux binary is missing, the
/// child argv is invalid/oversized, or the launch command itself fails.
///
/// SECURITY: the child PROGRAM (argv[0]) is constrained to the injector's trusted
/// dirs — a spawn cannot launch an arbitrary binary off `$PATH`. Each child argv
/// element is validated by [`spawn_arg_ok`] and the count bounded by
/// [`MAX_SPAWN_ARGS`]. cwd-allowlisting is the CALLER's job (config layer).
pub fn spawn(
    mux: Mux,
    cwd: &str,
    name: &str,
    cert: &str,
    circle: &str,
    argv_child: &[String],
    window: bool,
) -> Result<SpawnOutcome> {
    if argv_child.is_empty() {
        bail!("spawn: empty child command");
    }
    if argv_child.len() > MAX_SPAWN_ARGS {
        bail!(
            "spawn: child command has {} args (max {MAX_SPAWN_ARGS})",
            argv_child.len()
        );
    }
    for a in argv_child {
        if !spawn_arg_ok(a) {
            bail!("spawn: child argument is too long or contains control/NUL bytes");
        }
    }
    // The child PROGRAM must itself resolve to a trusted absolute path — a remote
    // spawn must not be able to launch an arbitrary binary off ambient $PATH. An
    // absolute path is accepted as-is only if it lives under a trusted dir.
    let prog = &argv_child[0];
    let prog_abs = resolve_trusted_program(prog)
        .ok_or_else(|| anyhow::anyhow!("spawn: program {prog:?} is not in a trusted directory"))?;
    // A spawn creates the new pane in the CALLER's OWN tmux server, so capture this
    // process's `$TMUX` socket and pin the new pane to it (WL-053); other muxes don't
    // use a socket here (the child captures its own at SessionStart).
    let spawn_socket = if mux == Mux::Tmux {
        tmux_socket_from_env()
    } else {
        String::new()
    };
    let cmds = spawn_commands(mux, &spawn_socket, cwd, name, cert, argv_child, window);
    if cmds.is_empty() {
        // Unspawnable mux (iTerm2/None): not an error, just unsupported.
        return Ok(SpawnOutcome::default());
    }
    let bin = mux.binary();
    if !have(bin) {
        bail!(
            "{bin} not found in a trusted directory (mux '{}')",
            mux.as_str()
        );
    }
    // Environment threaded into the child so it self-registers its unguessable
    // identity on its first `weave hook session`.
    let mut env: Vec<(String, String)> = vec![
        ("WEAVE_SESSION".to_string(), name.to_string()),
        ("WEAVE_BIRTH_CERT".to_string(), cert.to_string()),
    ];
    if !circle.is_empty() {
        env.push(("WEAVE_CIRCLE".to_string(), circle.to_string()));
    }
    // Replace argv[0] of the CHILD with its trusted absolute path inside the mux's
    // spawn argv, so the mux launches the trusted binary, not an ambient one. The
    // child program is the first element AFTER the end-of-options `--`.
    let mut cmds = cmds;
    rewrite_child_prog(&mut cmds, &prog_abs);
    // tmux/kitty/wezterm echo the new id on stdout → capture it. zellij/screen don't.
    let captures_id = matches!(mux, Mux::Tmux | Mux::Kitty | Mux::Wezterm);
    let mut outcome = SpawnOutcome {
        launched: false,
        target: String::new(),
    };
    for (i, cmd) in cmds.iter().enumerate() {
        let last = i + 1 == cmds.len();
        if last && captures_id {
            match run_capture_env(cmd, &env, INJECT_TIMEOUT) {
                // Parse the single trimmed stdout line as the new id. Be TOLERANT
                // (WL-008 ANSI lesson): a parse miss ⇒ fail-open to empty target and
                // lean on child self-registration, never a hard error.
                Ok(Some(out)) => {
                    outcome.launched = true;
                    outcome.target = parse_spawn_id(mux, &out);
                }
                Ok(None) => {
                    // Ran but non-zero / no stdout: treat as launched best-effort,
                    // empty target (child self-registers).
                    outcome.launched = true;
                }
                Err(e) => return Err(e),
            }
        } else {
            match run_bounded_env(cmd, &env, INJECT_TIMEOUT) {
                Ok(true) => outcome.launched = true,
                Ok(false) => bail!("`{}` exited non-zero", cmd.join(" ")),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(outcome)
}

/// Issue the per-mux kill for `target`. Returns `Ok(true)` when a kill command ran,
/// `Ok(false)` when the mux has no clean kill (iTerm2/None), or `Err` when the mux
/// binary is missing or the kill command fails. The target id MUST already have been
/// validated by [`id_valid`] at the caller (mcp/main) before reaching here.
pub fn kill(target: &Target) -> Result<bool> {
    let cmds = kill_commands(target);
    if cmds.is_empty() {
        return Ok(false);
    }
    let bin = target.mux.binary();
    if !have(bin) {
        bail!(
            "{bin} not found in a trusted directory (mux '{}')",
            target.mux.as_str()
        );
    }
    for cmd in &cmds {
        match run_bounded(cmd, INJECT_TIMEOUT) {
            // The mux ran but reported failure (non-zero exit) — e.g. the pane /
            // session is already gone, or the mux server is unreachable. Do NOT
            // claim the kill succeeded: surface it so the caller can report honestly
            // instead of a false "killed". (Mirrors `spawn`, which already fails on
            // a non-zero exit rather than swallowing it.)
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// WL-036 — run ONE post-send hook program, argv-only and bounded-synchronously.
///
/// SECURITY (the single most dangerous edit in WL-036, ARCHITECTURE §7): this is a
/// **no-shell** spawn. `argv` is a FIXED operator-authored template from `config.toml`;
/// weave NEVER substitutes message text INTO an argv element here. The program
/// (`argv[0]`) is resolved to a TRUSTED absolute path via [`resolve_trusted_program`]
/// (the same constraint a spawned child program gets), so a hook cannot launch an
/// arbitrary `$PATH` binary. The remaining elements are passed WHOLE via
/// `Command::args` — never concatenated, never parsed by a shell (there is no shell on
/// this path). Message-derived strings reach the child ONLY as `Command::envs` values,
/// delivered as the child's `environ` array with no shell evaluation; a hostile subject
/// `"; rm -rf /"` / `"$(reboot)"` is therefore an inert env value.
///
/// The wait is BOUNDED ([`run_bounded_env`]'s try_wait/kill pattern): a slow hook is
/// killed at `timeout` and reported, so a wedged hook can never hang `send`. Returns
/// `Ok(())` on a clean exit-zero; `Err` for a missing trusted program, an invalid argv,
/// a non-zero exit, a timeout, or a spawn failure — the orchestrator
/// ([`fire_post_send_hooks`]) catches every `Err` and logs it to stderr WITHOUT
/// sinking the (already-persisted) send.
pub fn run_post_send_hook(
    argv: &[String],
    env: &[(String, String)],
    timeout: std::time::Duration,
) -> Result<()> {
    use std::time::Instant;
    if argv.is_empty() {
        bail!("post-send hook: empty argv");
    }
    if argv.len() > MAX_SPAWN_ARGS {
        bail!(
            "post-send hook: {} argv elements (max {MAX_SPAWN_ARGS})",
            argv.len()
        );
    }
    for a in argv {
        if !spawn_arg_ok(a) {
            bail!("post-send hook: argv element is too long or contains control/NUL bytes");
        }
    }
    // argv[0] MUST resolve to a trusted absolute path — a hook cannot run an ambient
    // $PATH binary, exactly as a spawned child program cannot.
    let prog_abs = resolve_trusted_program(&argv[0]).ok_or_else(|| {
        anyhow::anyhow!(
            "post-send hook: program {:?} is not in a trusted directory",
            argv[0]
        )
    })?;
    let mut child = Command::new(&prog_abs)
        .args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("post-send hook {:?} exited non-zero", argv[0]);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("post-send hook {:?} timed out after {:?}", argv[0], timeout);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// WL-036 — fire every post-send hook matching `event`/`recipient`, best-effort.
///
/// The ONE orchestration seam invoked by BOTH send paths (CLI `weave send`/`notify` +
/// MCP `weave_send`/`weave_notify`) and the ack path, so the hook logic has a single
/// source of truth (no fork between `main.rs` and `mcp.rs`). It:
///   1. selects matching, valid hooks via the PURE [`Config::hooks_for`];
///   2. builds the `WEAVE_HOOK_*` env (event/sender/recipient/subject/message-id, plus
///      a small `WEAVE_HOOK_PAYLOAD` JSON object) — the message BODY is NOT exported
///      (it must not leak into the child's env / `ps e`);
///   3. spawns each via the bounded [`run_post_send_hook`];
///   4. catches EVERY failure (missing trusted binary, non-zero exit, timeout, spawn
///      error) and logs it to STDERR via `eprintln!` — NEVER propagated. The send has
///      already succeeded (the message is persisted) before any hook runs.
///
/// STDOUT DISCIPLINE: this writes to stderr only (`eprintln!`). The MCP caller invokes
/// it AFTER its JSON-RPC result is built, and these diagnostics never touch stdout.
pub fn fire_post_send_hooks(
    cfg: &weave_core::config::Config,
    event: weave_core::config::HookEvent,
    sender: &str,
    recipient: &str,
    subject: &str,
    message_id: i64,
) {
    let hooks = cfg.hooks_for(event, recipient);
    if hooks.is_empty() {
        return;
    }
    let env = hook_env(event, sender, recipient, subject, message_id);
    for h in hooks {
        if let Err(err) = run_post_send_hook(&h.argv, &env, h.timeout()) {
            // Fault isolation: a failing/slow/missing hook never breaks send. stderr
            // only (the send already persisted; never touch the JSON-RPC stdout frame).
            eprintln!("[weave] post-send hook failed (non-fatal): {err}");
        }
    }
}

/// Build the `WEAVE_HOOK_*` env vector handed to a post-send hook child. Message
/// fields travel ONLY as env values (never argv). The BODY is deliberately omitted
/// (avoid leaking message bodies into the child's environ / `ps e`). The optional
/// `WEAVE_HOOK_PAYLOAD` mirrors the same fields as a compact JSON object (atm-core
/// `ATM_POST_SEND` parity); fields are JSON-escaped via [`json_escape`] so any
/// metacharacters are inert quoted content, never interpreted. We hand-build the JSON
/// (a 3-field object) to keep `weave-inject` dependency-light — no `serde_json`.
fn hook_env(
    event: weave_core::config::HookEvent,
    sender: &str,
    recipient: &str,
    subject: &str,
    message_id: i64,
) -> Vec<(String, String)> {
    let payload = format!(
        "{{\"event\":\"{}\",\"sender\":\"{}\",\"recipient\":\"{}\",\"subject\":\"{}\",\"message_id\":{}}}",
        json_escape(event.as_str()),
        json_escape(sender),
        json_escape(recipient),
        json_escape(subject),
        message_id,
    );
    vec![
        ("WEAVE_HOOK_EVENT".to_string(), event.as_str().to_string()),
        ("WEAVE_HOOK_SENDER".to_string(), sender.to_string()),
        ("WEAVE_HOOK_RECIPIENT".to_string(), recipient.to_string()),
        ("WEAVE_HOOK_SUBJECT".to_string(), subject.to_string()),
        ("WEAVE_HOOK_MESSAGE_ID".to_string(), message_id.to_string()),
        ("WEAVE_HOOK_PAYLOAD".to_string(), payload),
    ]
}

/// Minimal JSON string-content escaper for [`hook_env`]'s `WEAVE_HOOK_PAYLOAD`.
/// Escapes the characters that MUST be escaped inside a JSON string (`"`, `\`, and
/// the C0 control chars) per RFC 8259, so a field value can never break out of its
/// quotes. Kept tiny on purpose — `weave-inject` has no `serde_json` dep.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Resolve a spawned child's PROGRAM (argv[0]) to a trusted absolute path. Accepts
/// either a bare name resolved against [`trusted_dirs`] (like the mux binaries) OR an
/// absolute path that itself lives under a trusted dir. Returns `None` otherwise, so a
/// remote spawn cannot launch a binary outside the trusted set.
fn resolve_trusted_program(prog: &str) -> Option<std::path::PathBuf> {
    if prog.is_empty() {
        return None;
    }
    let p = std::path::Path::new(prog);
    if p.is_absolute() {
        // An absolute program path is accepted only when it is a file whose PARENT
        // DIRECTORY is one of the trusted dirs. We canonicalize the parent dir (so a
        // `..` escape cannot smuggle the path out of a trusted dir, and a symlinked
        // dir compares on its real path) but we DO NOT follow the binary's OWN
        // symlink target — mirroring `resolve_trusted`, which trusts `dir.join(bin)`
        // by directory, not by the symlink's destination. (On many distros a trusted
        // `/usr/bin/foo` is itself a symlink into `/usr/lib/...`; following it would
        // wrongly reject a legitimately trusted binary.)
        if !is_executable_file(p) {
            return None;
        }
        let parent = std::fs::canonicalize(p.parent()?).ok()?;
        for d in trusted_dirs() {
            if let Ok(rd) = std::fs::canonicalize(&d) {
                if parent == rd {
                    return Some(p.to_path_buf());
                }
            }
        }
        None
    } else {
        // A bare name resolves against the trusted dirs, like a mux binary.
        resolve_trusted(prog)
    }
}

/// Rewrite the CHILD program (the first element after the end-of-options `--`) in each
/// spawn command to its trusted absolute path `prog_abs`. The mux binary itself
/// (argv[0]) is left as a bare name for `trusted_argv` to rewrite at run time.
fn rewrite_child_prog(cmds: &mut [Vec<String>], prog_abs: &std::path::Path) {
    let abs = prog_abs.to_string_lossy().into_owned();
    for cmd in cmds.iter_mut() {
        if let Some(pos) = cmd.iter().position(|s| s == "--") {
            if let Some(child0) = cmd.get_mut(pos + 1) {
                *child0 = abs.clone();
            }
        } else if matches!(cmd.first().map(String::as_str), Some("screen")) {
            // screen's spawn form has no `--`; the child program is the element after
            // `-dmS <name>`, i.e. index 3.
            if let Some(child0) = cmd.get_mut(3) {
                *child0 = abs.clone();
            }
        }
    }
}

/// Parse the new target id a mux echoed on stdout after a spawn. TOLERANT by design
/// (the WL-008 lesson: zellij once emitted ANSI codes that broke naive matching):
/// take the first non-empty trimmed line, strip a known prefix, and accept only an id
/// that passes [`id_valid`] for the mux — anything else ⇒ empty (child self-registers).
fn parse_spawn_id(mux: Mux, stdout: &str) -> String {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty());
    let Some(line) = line else {
        return String::new();
    };
    let candidate = match mux {
        // tmux `-F '#{pane_id}'` prints the pane id verbatim, e.g. `%3`.
        Mux::Tmux => line.to_string(),
        // kitty `@ launch` prints the new window id (an integer) on its own line.
        // wezterm `cli spawn` prints the new pane id (an integer).
        Mux::Kitty | Mux::Wezterm => line
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>(),
        // No other mux captures an id.
        _ => String::new(),
    };
    if !candidate.is_empty() && id_valid(mux, &candidate) {
        candidate
    } else {
        String::new()
    }
}

/// Like [`run_bounded`] but sets extra environment variables on the child process.
/// Used by [`spawn`] to thread the spawned agent's identity/cert/circle into the
/// new pane's environment without ever placing them in argv.
fn run_bounded_env(
    cmd: &[String],
    env: &[(String, String)],
    dur: std::time::Duration,
) -> Result<bool> {
    use std::time::Instant;
    let cmd = trusted_argv(cmd)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a trusted directory", cmd[0]))?;
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .spawn()?;
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

/// Like [`run_capture`] but sets extra environment variables on the child (so a
/// spawn's id-echoing launch command also threads the child's identity env).
fn run_capture_env(
    cmd: &[String],
    env: &[(String, String)],
    dur: std::time::Duration,
) -> Result<Option<String>> {
    use std::process::Stdio;
    use std::time::Instant;
    let cmd = trusted_argv(cmd)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a trusted directory", cmd[0]))?;
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
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
        Mux::Tmux => Some(tmux_argv(
            target.socket.as_str(),
            &["has-session", "-t", id],
        )),
        // zellij has no per-session "exists" verb; `list-sessions` enumerates them
        // and we scan stdout for the name in `target_alive`.
        Mux::Zellij => Some(argv(&["zellij", "list-sessions", "--no-formatting"])),
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
        // screen and iTerm2 have no cheap, scriptable existence check we trust here.
        Mux::Screen | Mux::ITerm2 | Mux::None => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetProbe {
    Alive,
    Absent,
    TransportUnavailable,
}

/// Run the read-only mux probe and retain the distinction that the legacy bool
/// [`target_alive`] intentionally erases: a missing/unlaunchable/timed-out local
/// transport is not evidence that the pane is absent, but it also cannot support a
/// [`Capability::Live`] promise.
fn target_probe(target: &Target) -> TargetProbe {
    if !target.injectable() {
        // `target_alive` has always been advisory/fail-open for targets that cannot
        // be injected. `capability` classifies this case before consulting the probe.
        return TargetProbe::Alive;
    }
    let bin = target.mux.binary();
    if !have(bin) {
        return TargetProbe::TransportUnavailable;
    }
    let Some(cmd) = liveness_probe(target) else {
        // Backends without a pane-existence query still need a real launchability
        // check. `--version` is read-only; a non-zero exit still proves the OS
        // launched the trusted program, while spawn/timeout failure cannot promise
        // live transport.
        let version = argv(&[bin, "--version"]);
        return match run_capture(&version, INJECT_TIMEOUT) {
            Ok(_) => TargetProbe::Alive,
            Err(_) => TargetProbe::TransportUnavailable,
        };
    };
    match target.mux {
        Mux::Tmux => match run_bounded(&cmd, INJECT_TIMEOUT) {
            Ok(true) => TargetProbe::Alive,
            Ok(false) => TargetProbe::Absent,
            Err(_) => TargetProbe::TransportUnavailable,
        },
        Mux::Zellij | Mux::Wezterm | Mux::Kitty => {
            match run_capture(&cmd, INJECT_TIMEOUT) {
                Ok(Some(out)) => {
                    if id_present(target.mux, &out, target.id.as_str()) {
                        TargetProbe::Alive
                    } else {
                        TargetProbe::Absent
                    }
                }
                // The program launched but returned no usable listing: preserve the
                // historical fail-open pane verdict. A launch/timeout error is a
                // transport failure and must not become `Capability::Live`.
                Ok(None) => TargetProbe::Alive,
                Err(_) => TargetProbe::TransportUnavailable,
            }
        }
        Mux::Screen | Mux::ITerm2 | Mux::None => unreachable!("handled above"),
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
    !matches!(target_probe(target), TargetProbe::Absent)
}

/// The delivery capability of a target, as a structured verdict for the `connect`
/// handshake. Composed from [`Target::injectable`], trusted mux resolution, and
/// the same bounded read-only probe used by [`target_alive`] — it adds NO new
/// injector or spawn path.
///
/// The verdict is advisory and degrades gracefully: only [`Capability::Live`]
/// promises a live nudge; the other verdicts are NOT errors. A transport-unavailable,
/// registered-but-not-alive, or non-injectable peer still receives every message
/// via the store on its next hook drain, matching weave's degrade-to-store contract.
/// A missing, unlaunchable, or timed-out trusted mux executable is reported
/// separately: injection cannot be promised in that state, so claiming `Live`
/// would be dishonest. Once the binary launches, inconclusive pane liveness
/// remains fail-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Injectable (real mux + valid-looking id) and the liveness probe did not
    /// report the pane/session as gone ⇒ a live nudge can be pushed now.
    Live,
    /// The target is structurally injectable, but weave cannot resolve or launch
    /// the mux executable from its trusted directories. Live transport cannot be
    /// promised until the operator installs/trusts a working mux (for example via
    /// `WEAVE_MUX_DIR`).
    TransportUnavailable,
    /// Injectable, but the liveness probe confidently reported the target absent
    /// ⇒ skip the live nudge, deliver via the store on next turn.
    RegisteredNotAlive,
    /// No injectable pane (`mux=none` or empty id) ⇒ store-only delivery.
    NotInjectable,
}

impl Capability {
    /// Whether the pane/session is not confidently known absent. Transport
    /// availability is deliberately orthogonal: a missing local mux executable
    /// prevents reachability but says nothing about whether the pane exists, so
    /// `TransportUnavailable` preserves the injector's fail-open pane verdict.
    pub fn pane_not_known_absent(self) -> bool {
        matches!(self, Self::Live | Self::TransportUnavailable)
    }
}

/// Capability verdict for `target`, composed from [`Target::injectable`], trusted
/// mux availability, and the same bounded probe as [`target_alive`]. Safe to call
/// before deciding whether to knock or queue; the only side effects are filesystem
/// presence checks and a read-only, fail-open liveness/launch probe.
pub fn capability(target: &Target) -> Capability {
    let injectable = target.injectable();
    if !injectable {
        return capability_from_facts(false, false, false);
    }
    match target_probe(target) {
        TargetProbe::Alive => capability_from_facts(true, true, true),
        TargetProbe::Absent => capability_from_facts(true, true, false),
        TargetProbe::TransportUnavailable => capability_from_facts(true, false, false),
    }
}

/// Pure truth table beneath [`capability`]. Keeping filesystem/probe facts as
/// parameters makes the honesty invariant exhaustive and platform-independent in
/// unit tests: a missing mux transport can never map to [`Capability::Live`].
fn capability_from_facts(
    injectable: bool,
    transport_available: bool,
    target_is_alive: bool,
) -> Capability {
    if !injectable {
        Capability::NotInjectable
    } else if !transport_available {
        Capability::TransportUnavailable
    } else if target_is_alive {
        Capability::Live
    } else {
        Capability::RegisteredNotAlive
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
        // zellij lists exited sessions too, e.g.
        // `name [Created ...] (EXITED - attach to resurrect)`. Those session
        // names are present in `list-sessions` output, but `write-chars` cannot
        // target them until they are resurrected, so only count non-EXITED rows.
        Mux::Zellij => out
            .lines()
            .any(|line| !line.contains("(EXITED") && line.split_whitespace().any(|tok| tok == id)),
        // Exact whitespace-delimited token anywhere in the listing.
        Mux::Wezterm => out.split_whitespace().any(|tok| tok == id),
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
        Mux::Tmux | Mux::Screen | Mux::ITerm2 | Mux::None => out.contains(id),
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
///
/// Precedence: `WEAVE_MUX_DIR` entries come **first**, ahead of the hardcoded
/// system dirs (`/usr/bin`, …) and the `$HOME/...` tool dirs. The user tool set
/// includes both `.nix-profile/bin` and LifeOS/Nix-style `.nix-profile/toolbin`.
/// `resolve_trusted`
/// returns the first dir that contains the binary, so an explicit opt-in dir wins
/// over an ambient same-named system binary. This is intentional: a user who sets
/// `WEAVE_MUX_DIR` is vouching for that dir and means "use *this* mux", and it is
/// also how tests point weave at a fake mux on a runner that already ships a real
/// `/usr/bin/tmux` (otherwise the system binary would shadow the fake).
fn trusted_dirs() -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = Vec::new();
    // Explicit opt-in for a mux installed in a nonstandard dir (the user vouches
    // for it by setting this); also how tests point at a fake mux. Listed first so
    // it takes precedence over an ambient same-named system binary below.
    if let Some(extra) = std::env::var_os("WEAVE_MUX_DIR") {
        v.extend(std::env::split_paths(&extra));
    }
    v.extend(
        ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(std::path::PathBuf::from),
    );
    if let Some(home) = std::env::var_os("HOME") {
        let h = std::path::PathBuf::from(home);
        v.push(h.join(".cargo/bin"));
        v.push(h.join(".local/bin"));
        v.push(h.join(".nix-profile/bin"));
        v.push(h.join(".nix-profile/toolbin"));
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
        .find(|p| is_executable_file(p))
}

/// A trusted-command candidate must be a regular file with at least one executable
/// mode bit. This is deliberately only a cheap resolution filter: Unix permission
/// classes, ACLs, mount flags, and executable format still affect the current
/// process. [`capability`] therefore performs a real bounded read-only launch/probe
/// before it can promise [`Capability::Live`].
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
        // iTerm2 session ids are of the form "w0t0p0:ABC123" (window, tab, pane).
        Mux::ITerm2 => {
            id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':')
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

/// Abstraction over the native injector (and related environment probes) so that
/// `weave-mcp::serve` can be driven by a real mux detector in production and by a
/// deterministic mock in tests.
///
/// The default implementation for the weave binary simply delegates to the free
/// functions in this module; tests can provide a fake `Injector` that records calls
/// and returns canned `Target`s / `Capability`s.
pub trait Injector {
    /// Detect the injection target for the current process from the environment.
    fn detect_target(&self) -> Target;

    /// Probe whether `target`'s pane/session is currently alive (best-effort).
    fn target_alive(&self, target: &Target) -> bool;

    /// Inject `body` into `target` using the chosen nudge mode.
    fn inject_mode(&self, target: &Target, body: &str, mode: Nudge) -> anyhow::Result<bool>;

    /// Describe the target transport (live / transport-unavailable /
    /// registered-but-not-alive / not-injectable).
    fn capability(&self, target: &Target) -> Capability;

    /// Check whether a named binary exists on PATH (via `resolve_trusted`).
    fn have(&self, name: &str) -> bool;

    /// Validate a mux-specific target id (`tmux` pane id, `zellij` session, etc).
    fn id_valid(&self, mux: Mux, id: &str) -> bool;

    /// Capture git worktree tags for `cwd`.
    fn git_tags(&self, cwd: &std::path::Path) -> anyhow::Result<weave_core::model::WorktreeTags>;

    /// Convenience helper: git tags for the current working directory.
    fn git_tags_here(&self) -> weave_core::model::WorktreeTags {
        match std::env::current_dir() {
            Ok(p) => self.git_tags(&p).unwrap_or_default(),
            Err(_) => weave_core::model::WorktreeTags::default(),
        }
    }

    /// Spawn a child agent into a new `mux` pane/window (WL-047). Delegates to the
    /// free [`spawn`] fn by default; tests provide a recording fake. `circle` is the
    /// optional visibility circle threaded as `WEAVE_CIRCLE` (empty ⇒ omitted).
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        mux: Mux,
        cwd: &str,
        name: &str,
        cert: &str,
        circle: &str,
        argv_child: &[String],
        window: bool,
    ) -> anyhow::Result<SpawnOutcome> {
        spawn(mux, cwd, name, cert, circle, argv_child, window)
    }

    /// Kill a registered peer's pane/session (WL-047). Delegates to the free [`kill`]
    /// fn by default; tests provide a recording fake.
    fn kill(&self, target: &Target) -> anyhow::Result<bool> {
        kill(target)
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
        // No pane-id captured: falls back to the focused pane.
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
    fn zellij_targets_pane_when_socket_set() {
        let mut target = t(Mux::Zellij, "envctl");
        target.socket = "13".into();
        let c = commands_for(&target, "hi");
        assert_eq!(
            c[0],
            argv(&[
                "zellij",
                "--session",
                "envctl",
                "action",
                "write-chars",
                "--pane-id",
                "13",
                "--",
                "hi"
            ])
        );
        assert_eq!(
            c[1],
            argv(&[
                "zellij",
                "--session",
                "envctl",
                "action",
                "write",
                "--pane-id",
                "13",
                "13"
            ])
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
            pid: None,
            host: String::new(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: weave_core::model::DEFAULT_CIRCLE.to_string(),
            role: weave_core::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
            client_session: String::new(),
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
            pid: None,
            host: String::new(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: weave_core::model::DEFAULT_CIRCLE.to_string(),
            role: weave_core::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
            client_session: String::new(),
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

    /// `from_peer` copies a peer's stored zellij pane id (held in `socket`)
    /// into the Target so cross-session injection targets the correct pane.
    #[test]
    fn from_peer_carries_zellij_pane_id() {
        let p = Peer {
            name: "z".into(),
            mux: "zellij".into(),
            target: "wise-tomato".into(),
            cwd: None,
            socket: "terminal_3".into(),
            last_seen: 0,
            pid: None,
            host: String::new(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: weave_core::model::DEFAULT_CIRCLE.to_string(),
            role: weave_core::model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
            client_session: String::new(),
        };
        let target = Target::from_peer(&p);
        assert_eq!(target.socket, "terminal_3");
        assert_eq!(target.mux, Mux::Zellij);
        assert_eq!(target.id, "wise-tomato");
        let c = commands_for(&target, "hi");
        assert!(c[0].contains(&"--pane-id".to_string()));
        assert!(c[0].contains(&"terminal_3".to_string()));
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
            Some(argv(&["zellij", "list-sessions", "--no-formatting"]))
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

    /// Truth table for the pure facts beneath `capability()`. This proves every
    /// combination without depending on which mux programs happen to be installed
    /// on the test runner:
    ///
    /// - `mux=none`  ⇒ NotInjectable (not injectable, regardless of id).
    /// - injectable + empty id ⇒ NotInjectable (empty id is not injectable).
    /// - injectable + unavailable transport ⇒ TransportUnavailable, never Live;
    /// - injectable + available transport + fail-open/alive probe ⇒ Live;
    /// - injectable + available transport + confidently absent probe ⇒
    ///   RegisteredNotAlive.
    #[test]
    fn capability_truth_table() {
        // Non-injectable: mux=none ⇒ NotInjectable.
        assert_eq!(capability(&Target::none()), Capability::NotInjectable);
        // Non-injectable: a real mux but an empty id is not injectable.
        assert_eq!(
            capability(&t(Mux::Tmux, "")),
            Capability::NotInjectable,
            "empty id ⇒ not injectable ⇒ NotInjectable"
        );
        assert_eq!(
            capability_from_facts(true, false, true),
            Capability::TransportUnavailable,
            "a missing trusted mux binary can never promise Live"
        );
        assert_eq!(
            capability_from_facts(true, true, true),
            Capability::Live,
            "available transport + fail-open/alive probe ⇒ Live"
        );
        assert_eq!(
            capability_from_facts(true, true, false),
            Capability::RegisteredNotAlive,
            "available transport + confident absence ⇒ RegisteredNotAlive"
        );
        assert!(Capability::Live.pane_not_known_absent());
        assert!(Capability::TransportUnavailable.pane_not_known_absent());
        assert!(!Capability::RegisteredNotAlive.pane_not_known_absent());
        assert!(!Capability::NotInjectable.pane_not_known_absent());
    }

    /// Trusted resolution rejects a mode-0644 lookalike, while capability goes one
    /// step further and refuses `Live` when metadata passes the candidate filter but
    /// the current process cannot actually launch the program.
    #[cfg(unix)]
    #[test]
    fn trusted_resolution_and_actual_launchability_are_distinct() {
        use std::os::unix::fs::PermissionsExt;

        let _g = weave_core::testenv::lock_env();
        let dir = std::env::temp_dir().join(format!("weave-nonexec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create trusted test dir");
        let program = dir.join("weave-nonexec-mux");
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").expect("write test program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644))
            .expect("chmod non-executable");
        let _mux = weave_core::testenv::EnvVarGuard::set(
            "WEAVE_MUX_DIR",
            dir.to_str().expect("utf8 temp path"),
        );

        assert!(!have("weave-nonexec-mux"));
        assert!(resolve_trusted_program(program.to_str().unwrap()).is_none());

        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
        assert!(have("weave-nonexec-mux"));
        assert_eq!(
            resolve_trusted_program(program.to_str().unwrap()),
            Some(program.clone())
        );

        // Executable metadata alone is insufficient: a missing shebang interpreter
        // makes launch fail for every effective uid, including root-run CI.
        // Capability must report the transport failure, while the legacy
        // pane-existence bool remains fail-open.
        let screen = dir.join("screen");
        let missing_interpreter = dir.join("weave-definitely-missing-interpreter");
        assert!(!missing_interpreter.exists());
        std::fs::write(&screen, format!("#!{}\n", missing_interpreter.display()))
            .expect("write unlaunchable fake screen");
        std::fs::set_permissions(&screen, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
        assert!(have("screen"), "candidate filter sees an execute bit");
        let target = t(Mux::Screen, "1234.pts-0.host");
        assert_eq!(capability(&target), Capability::TransportUnavailable);
        assert!(target_alive(&target), "pane liveness remains fail-open");

        std::fs::write(&screen, b"#!/bin/sh\nexit 0\n").expect("repair fake screen");
        assert_eq!(capability(&target), Capability::Live);

        std::fs::remove_file(program).ok();
        std::fs::remove_file(screen).ok();
        std::fs::remove_dir(dir).ok();
    }

    /// Nix profiles in this workspace expose operator tools in `toolbin`, while
    /// conventional Nix packages use `bin`; both are trusted user-profile roots.
    #[test]
    fn nix_profile_bin_and_toolbin_are_trusted() {
        let _g = weave_core::testenv::lock_env();
        let _mux = weave_core::testenv::EnvVarGuard::remove("WEAVE_MUX_DIR");
        let _home = weave_core::testenv::EnvVarGuard::set("HOME", "/tmp/weave-home");
        let dirs = trusted_dirs();
        assert!(dirs.contains(&std::path::PathBuf::from(
            "/tmp/weave-home/.nix-profile/bin"
        )));
        assert!(dirs.contains(&std::path::PathBuf::from(
            "/tmp/weave-home/.nix-profile/toolbin"
        )));
    }

    /// `WEAVE_MUX_DIR` must take precedence over the hardcoded system dirs so an
    /// explicit opt-in dir wins over an ambient same-named system binary (e.g. a
    /// runner-provided `/usr/bin/tmux` must not shadow a fake mux the test points
    /// at). We assert the *ordering* of `trusted_dirs()` directly — no on-disk
    /// binary, no process-global resolution that could race other parallel tests.
    #[test]
    fn weave_mux_dir_precedes_system_dirs() {
        // Serialize on the ONE canonical env lock so this WEAVE_MUX_DIR mutation
        // mutually excludes with every other WEAVE_*-touching unit test (config's
        // token/timeout tests, the concurrency stress test, and any reader of
        // trusted_dirs()). The EnvVarGuard restores the prior value (or removes it
        // if it was absent) on drop — even on panic — so no state leaks.
        let _g = weave_core::testenv::lock_env();
        let _v = weave_core::testenv::EnvVarGuard::set("WEAVE_MUX_DIR", "/tmp/weave-fake-mux");
        let dirs = trusted_dirs();

        let opt_in = std::path::Path::new("/tmp/weave-fake-mux");
        let usr_bin = std::path::Path::new("/usr/bin");
        let opt_in_idx = dirs
            .iter()
            .position(|d| d == opt_in)
            .expect("WEAVE_MUX_DIR entry present in trusted_dirs()");
        let usr_bin_idx = dirs
            .iter()
            .position(|d| d == usr_bin)
            .expect("/usr/bin present in trusted_dirs()");
        assert!(
            opt_in_idx < usr_bin_idx,
            "WEAVE_MUX_DIR ({opt_in_idx}) must precede /usr/bin ({usr_bin_idx}) in {dirs:?}"
        );
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

    #[test]
    fn zellij_exited_sessions_do_not_count_alive() {
        let sessions = "\
judicious-tiger [Created 9h 45m 38s ago] (EXITED - attach to resurrect)
zippy-brachiosaur [Created 9h 27m 47s ago] (current)
";

        assert!(
            !id_present(Mux::Zellij, sessions, "judicious-tiger"),
            "zellij EXITED sessions are listed by name but cannot receive write-chars"
        );
        assert!(
            id_present(Mux::Zellij, sessions, "zippy-brachiosaur"),
            "current/live zellij sessions should still count as injectable"
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

    /// Proof that the canonical env guard serializes concurrent `WEAVE_MUX_DIR`
    /// mutation against `trusted_dirs()` reads. N threads × K iterations each take
    /// `weave_core::testenv::lock_env()`, set a UNIQUE `WEAVE_MUX_DIR` via `EnvVarGuard`,
    /// then assert the dir they just set is the FIRST entry of `trusted_dirs()`. With
    #[test]
    fn detect_target_with_preference_honors_kitty_over_tmux() {
        let _lock = weave_core::testenv::lock_env();
        let _t = weave_core::testenv::EnvVarGuard::set("TMUX_PANE", "%0");
        let _k = weave_core::testenv::EnvVarGuard::set("KITTY_WINDOW_ID", "42");
        // Without preference, tmux wins (higher priority).
        let auto = detect_target_with_preference(None);
        assert_eq!(auto.mux, Mux::Tmux);
        // With kitty preference, kitty wins.
        let pref = detect_target_with_preference(Some(Mux::Kitty));
        assert_eq!(pref.mux, Mux::Kitty);
        assert_eq!(pref.id, "42");
        // With tmux preference, tmux wins.
        let pref_tmux = detect_target_with_preference(Some(Mux::Tmux));
        assert_eq!(pref_tmux.mux, Mux::Tmux);
        assert_eq!(pref_tmux.id, "%0");
    }

    #[test]
    fn detect_target_with_preference_returns_none_when_missing() {
        let _lock = weave_core::testenv::lock_env();
        std::env::remove_var("WEZTERM_PANE");
        std::env::remove_var("TMUX_PANE");
        std::env::remove_var("ZELLIJ_SESSION_NAME");
        std::env::remove_var("KITTY_WINDOW_ID");
        std::env::remove_var("STY");
        let pref = detect_target_with_preference(Some(Mux::Wezterm));
        assert_eq!(pref.mux, Mux::None);
    }

    #[test]
    fn iterm2_commands_use_osascript() {
        let c = commands_for(&t(Mux::ITerm2, "w0t0p0"), "hello world");
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0],
            argv(&[
                "osascript",
                "-e",
                "tell application \"iTerm2\" to tell current session of current window to write text \"hello world\"",
            ])
        );
    }

    #[test]
    fn iterm2_escapes_quotes_and_backslashes() {
        let c = commands_for(&t(Mux::ITerm2, "x"), "say \"hi\"");
        assert_eq!(c.len(), 1);
        let script = &c[0][2];
        assert!(script.contains("\\\"hi\\\""), "quotes escaped: {script}");
    }

    #[test]
    fn iterm2_detect_target_from_term_program() {
        let _lock = weave_core::testenv::lock_env();
        std::env::remove_var("TMUX_PANE");
        std::env::remove_var("ZELLIJ_SESSION_NAME");
        std::env::remove_var("WEZTERM_PANE");
        std::env::remove_var("KITTY_WINDOW_ID");
        std::env::remove_var("STY");
        let _g = weave_core::testenv::EnvVarGuard::set("TERM_PROGRAM", "iTerm.app");
        let _g2 = weave_core::testenv::EnvVarGuard::set("TERM_SESSION_ID", "w0t0p0:ABC123");
        let target = detect_target();
        assert_eq!(target.mux, Mux::ITerm2);
        assert_eq!(target.id, "w0t0p0:ABC123");
    }

    #[test]
    fn iterm2_no_probe_so_always_alive() {
        let target = t(Mux::ITerm2, "x");
        assert!(liveness_probe(&target).is_none());
        assert!(target_alive(&target));
        assert_eq!(
            capability(&target),
            if have(target.mux.binary()) {
                Capability::Live
            } else {
                Capability::TransportUnavailable
            }
        );
    }

    /// the unified lock every critical section is exclusive, so the read always sees
    /// the writer's own value; without it, another thread's set/remove could
    /// interleave and the assertion would observe the wrong (or no) leading dir —
    /// the exact `set_var`/`getenv` data race #10 removes. Iteration-count bounded
    /// (no wall-clock) per the anti-flake rule.
    #[test]
    fn env_guard_serializes_concurrent_weave_mux_dir() {
        const THREADS: usize = 8;
        const ITERS: usize = 200;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..ITERS {
                        let unique = format!("/tmp/weave-stress-{t}-{i}");
                        let _g = weave_core::testenv::lock_env();
                        let _v = weave_core::testenv::EnvVarGuard::set("WEAVE_MUX_DIR", &unique);
                        let dirs = trusted_dirs();
                        assert_eq!(
                            dirs.first().map(|p| p.as_path()),
                            Some(std::path::Path::new(&unique)),
                            "under the guard, the just-set WEAVE_MUX_DIR must lead trusted_dirs()"
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("stress worker thread panicked");
        }
    }

    // ---- WL-047 spawn/kill exact-argv unit tests ----------------------------

    fn child(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tmux_spawn_argv() {
        // Pane (default): split-window, captures the new pane id via -P -F.
        let c = spawn_commands(
            Mux::Tmux,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["echo", "hi"]),
            false,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0],
            argv(&[
                "tmux",
                "split-window",
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
                "/work",
                "--",
                "echo",
                "hi"
            ])
        );
        // Window opt-in: new-window instead of split-window.
        let w = spawn_commands(
            Mux::Tmux,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["echo", "hi"]),
            true,
        );
        assert_eq!(w[0][1], "new-window");
    }

    // ---- WL-053: tmux server socket (`-S`) is captured + threaded ------------

    /// `$TMUX` (`<socket>,<pid>,<session>`) parses to its socket path; empty/garbage
    /// yields no socket (⇒ default server).
    #[test]
    fn parse_tmux_socket_extracts_path() {
        assert_eq!(
            parse_tmux_socket("/tmp/tmux-1000/default,12345,0"),
            "/tmp/tmux-1000/default"
        );
        assert_eq!(
            parse_tmux_socket("/tmp/tmux-1000/mylabel,9,3"),
            "/tmp/tmux-1000/mylabel"
        );
        assert_eq!(parse_tmux_socket(""), "");
    }

    /// `tmux_argv` inserts `-S <socket>` only when a socket is present; empty socket
    /// reproduces the historical default-server argv byte-for-byte.
    #[test]
    fn tmux_argv_inserts_socket_selector() {
        assert_eq!(
            tmux_argv("", &["kill-pane", "-t", "%3"]),
            argv(&["tmux", "kill-pane", "-t", "%3"])
        );
        assert_eq!(
            tmux_argv("/tmp/tmux-1000/foo", &["kill-pane", "-t", "%3"]),
            argv(&["tmux", "-S", "/tmp/tmux-1000/foo", "kill-pane", "-t", "%3"])
        );
    }

    /// A peer carrying a captured socket threads `-S <socket>` through inject, kill,
    /// liveness, and spawn — so the command reaches the ORIGINATING server, not the
    /// default one (the WL-047 `/verify` failure mode).
    #[test]
    fn tmux_socket_threads_through_all_commands() {
        let sock = "/tmp/tmux-1000/agents";
        let tgt = Target {
            mux: Mux::Tmux,
            id: "%7".into(),
            socket: sock.into(),
        };
        // inject: every send-keys argv is `-S`-pinned.
        for cmd in commands_for(&tgt, "hi") {
            assert_eq!(&cmd[0..3], &["tmux".to_string(), "-S".into(), sock.into()]);
            assert_eq!(cmd[3], "send-keys");
        }
        // kill.
        assert_eq!(
            kill_commands(&tgt),
            vec![argv(&["tmux", "-S", sock, "kill-pane", "-t", "%7"])]
        );
        // liveness probe (has-session).
        assert_eq!(
            liveness_probe(&tgt),
            Some(argv(&["tmux", "-S", sock, "has-session", "-t", "%7"]))
        );
        // spawn: `-S <sock>` precedes the verb.
        let sp = spawn_commands(
            Mux::Tmux,
            sock,
            "/work",
            "bob",
            "cert",
            &child(&["echo"]),
            false,
        );
        assert_eq!(
            &sp[0][0..3],
            &["tmux".to_string(), "-S".into(), sock.into()]
        );
        assert_eq!(sp[0][3], "split-window");
        // A socket-less peer is unchanged (default server).
        let bare = t(Mux::Tmux, "%7");
        assert_eq!(
            kill_commands(&bare),
            vec![argv(&["tmux", "kill-pane", "-t", "%7"])]
        );
    }

    #[test]
    fn zellij_spawn_argv() {
        let c = spawn_commands(
            Mux::Zellij,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent"]),
            false,
        );
        assert_eq!(c[0], argv(&["zellij", "action", "new-pane", "--", "agent"]));
        let w = spawn_commands(
            Mux::Zellij,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent"]),
            true,
        );
        assert_eq!(w[0][2], "new-tab");
    }

    #[test]
    fn kitty_spawn_argv() {
        let c = spawn_commands(
            Mux::Kitty,
            "",
            "/work",
            "bob",
            "deadbeef",
            &child(&["agent"]),
            false,
        );
        assert_eq!(
            c[0],
            argv(&[
                "kitten",
                "@",
                "launch",
                "--type",
                "tab",
                "--cwd",
                "/work",
                "--env",
                "WEAVE_SESSION=bob",
                "--env",
                "WEAVE_BIRTH_CERT=deadbeef",
                "--",
                "agent"
            ])
        );
        // Window opt-in flips the launch type to a new OS window.
        let w = spawn_commands(
            Mux::Kitty,
            "",
            "/work",
            "bob",
            "deadbeef",
            &child(&["agent"]),
            true,
        );
        let ty = w[0].iter().position(|s| s == "--type").unwrap();
        assert_eq!(w[0][ty + 1], "os-window");
        // No cert ⇒ no WEAVE_BIRTH_CERT --env pair emitted.
        let nc = spawn_commands(
            Mux::Kitty,
            "",
            "/work",
            "bob",
            "",
            &child(&["agent"]),
            false,
        );
        assert!(!nc[0].iter().any(|s| s.starts_with("WEAVE_BIRTH_CERT")));
    }

    #[test]
    fn wezterm_spawn_argv() {
        let c = spawn_commands(
            Mux::Wezterm,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent"]),
            false,
        );
        assert_eq!(
            c[0],
            argv(&["wezterm", "cli", "spawn", "--cwd", "/work", "--", "agent"])
        );
        let w = spawn_commands(
            Mux::Wezterm,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent"]),
            true,
        );
        assert!(w[0].iter().any(|s| s == "--new-window"));
    }

    #[test]
    fn screen_spawn_argv() {
        // screen owns its own session named after the child for a cleaner kill.
        let c = spawn_commands(
            Mux::Screen,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent"]),
            false,
        );
        assert_eq!(c[0], argv(&["screen", "-dmS", "bob", "agent"]));
    }

    #[test]
    fn iterm2_and_none_spawn_empty() {
        assert!(spawn_commands(Mux::ITerm2, "", "/w", "b", "c", &child(&["a"]), false).is_empty());
        assert!(spawn_commands(Mux::None, "", "/w", "b", "c", &child(&["a"]), false).is_empty());
        // An empty child argv is never spawnable, regardless of mux.
        assert!(spawn_commands(Mux::Tmux, "", "/w", "b", "c", &[], false).is_empty());
    }

    #[test]
    fn spawn_child_leading_dash_is_content() {
        // A child arg beginning with `-` must land AFTER the `--`, treated as content.
        let c = spawn_commands(
            Mux::Tmux,
            "",
            "/work",
            "bob",
            "cert",
            &child(&["agent", "--help"]),
            false,
        );
        let dd = c[0].iter().position(|s| s == "--").unwrap();
        assert_eq!(c[0][dd + 1], "agent");
        assert_eq!(c[0][dd + 2], "--help");
    }

    #[test]
    fn tmux_kill_argv() {
        assert_eq!(
            kill_commands(&t(Mux::Tmux, "%3")),
            vec![argv(&["tmux", "kill-pane", "-t", "%3"])]
        );
    }

    #[test]
    fn wezterm_kill_argv() {
        assert_eq!(
            kill_commands(&t(Mux::Wezterm, "7")),
            vec![argv(&["wezterm", "cli", "kill-pane", "--pane-id", "7"])]
        );
    }

    #[test]
    fn kitty_kill_argv() {
        // Without a socket: bare close-window.
        assert_eq!(
            kill_commands(&t(Mux::Kitty, "7")),
            vec![argv(&["kitten", "@", "close-window", "--match", "id:7"])]
        );
        // With a socket: --to precedes @.
        let mut tg = t(Mux::Kitty, "7");
        tg.socket = "unix:/tmp/k".into();
        let c = kill_commands(&tg);
        let at = c[0].iter().position(|s| s == "@").unwrap();
        let to = c[0].iter().position(|s| s == "--to").unwrap();
        assert!(to < at, "--to before @: {:?}", c[0]);
    }

    #[test]
    fn zellij_kill_argv() {
        // Coarse: delete the whole session by name.
        assert_eq!(
            kill_commands(&t(Mux::Zellij, "envctl")),
            vec![argv(&["zellij", "delete-session", "--force", "envctl"])]
        );
    }

    #[test]
    fn screen_kill_argv() {
        // Coarse: quit the named session.
        assert_eq!(
            kill_commands(&t(Mux::Screen, "1234.pts-0.host")),
            vec![argv(&["screen", "-S", "1234.pts-0.host", "-X", "quit"])]
        );
    }

    #[test]
    fn iterm2_and_none_kill_empty() {
        assert!(kill_commands(&t(Mux::ITerm2, "w0t0p0:ABC")).is_empty());
        assert!(kill_commands(&Target::none()).is_empty());
    }

    #[test]
    fn spawn_arg_ok_rejects_bad_args() {
        assert!(spawn_arg_ok("agent"));
        assert!(spawn_arg_ok("--flag"));
        assert!(spawn_arg_ok("")); // empty positional is allowed
        assert!(!spawn_arg_ok("a\0b")); // NUL rejected
        assert!(!spawn_arg_ok("a\nb")); // control byte rejected
        assert!(!spawn_arg_ok("\x1b[0m")); // ESC rejected
        let huge = "x".repeat(MAX_SPAWN_ARG_LEN + 1);
        assert!(!spawn_arg_ok(&huge)); // length cap
    }

    #[test]
    fn parse_spawn_id_is_tolerant() {
        // tmux echoes the pane id verbatim.
        assert_eq!(parse_spawn_id(Mux::Tmux, "%5\n"), "%5");
        // kitty/wezterm echo an integer; trailing junk is trimmed at the first
        // non-digit, mirroring the WL-008 ANSI-tolerance lesson.
        assert_eq!(parse_spawn_id(Mux::Kitty, "12\n"), "12");
        assert_eq!(parse_spawn_id(Mux::Wezterm, "7 something"), "7");
        // A line that yields no valid id ⇒ empty (child self-registers).
        assert_eq!(parse_spawn_id(Mux::Tmux, "garbage"), "");
        assert_eq!(parse_spawn_id(Mux::Kitty, "\x1b[0mnope"), "");
        assert_eq!(parse_spawn_id(Mux::Zellij, "anything"), "");
    }
}
