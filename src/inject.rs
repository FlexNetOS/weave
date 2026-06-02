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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mux {
    Tmux,
    Zellij,
    Kitty,
    Wezterm,
    Screen,
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
#[derive(Debug, Clone)]
pub struct Target {
    pub mux: Mux,
    pub id: String,
}

impl Target {
    pub fn none() -> Self {
        Target {
            mux: Mux::None,
            id: String::new(),
        }
    }

    pub fn injectable(&self) -> bool {
        self.mux != Mux::None && !self.id.is_empty()
    }

    pub fn from_peer(p: &Peer) -> Self {
        Target {
            mux: Mux::parse(&p.mux),
            id: p.target.clone(),
        }
    }
}

/// Detect the *current* process's injectable target from environment variables
/// set by the multiplexer/terminal. Probed most- to least-specific.
pub fn detect_target() -> Target {
    // Order matters: a process can be inside tmux *and* a terminal; prefer the
    // multiplexer that owns the input line.
    if let Some(id) = nonempty_env("TMUX_PANE") {
        return Target { mux: Mux::Tmux, id };
    }
    if let Some(id) = nonempty_env("ZELLIJ_SESSION_NAME") {
        return Target {
            mux: Mux::Zellij,
            id,
        };
    }
    if let Some(id) = nonempty_env("WEZTERM_PANE") {
        return Target {
            mux: Mux::Wezterm,
            id,
        };
    }
    if let Some(id) = nonempty_env("KITTY_WINDOW_ID") {
        return Target {
            mux: Mux::Kitty,
            id,
        };
    }
    if let Some(id) = nonempty_env("STY") {
        return Target {
            mux: Mux::Screen,
            id,
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
    out
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
        Mux::Kitty => {
            let m = format!("id:{id}");
            vec![
                argv(&["kitten", "@", "send-text", "--match", &m, "--", text]),
                argv(&["kitten", "@", "send-text", "--match", &m, "--", CR]),
            ]
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

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Is `bin` on PATH?
pub fn have(bin: &str) -> bool {
    if bin.is_empty() {
        return false;
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
        .unwrap_or(false)
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
    let cmds = commands_for(target, text);
    if cmds.is_empty() {
        return Ok(false);
    }
    let bin = target.mux.binary();
    if !have(bin) {
        bail!("{bin} not found on PATH (mux '{}')", target.mux.as_str());
    }
    for (i, cmd) in cmds.iter().enumerate() {
        let run = || -> Result<bool> {
            let status = Command::new(&cmd[0]).args(&cmd[1..]).status()?;
            Ok(status.success())
        };
        match run() {
            Ok(true) => {}
            // The first command types the literal text. If it fails, nothing has
            // landed — propagate so the caller cleanly falls back to next-turn.
            Ok(false) if i == 0 => {
                bail!("`{}` exited non-zero", cmd.join(" "));
            }
            Err(e) if i == 0 => return Err(e),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn t(mux: Mux, id: &str) -> Target {
        Target { mux, id: id.into() }
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
}
