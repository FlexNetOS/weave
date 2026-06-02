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
        return Target { mux: Mux::Zellij, id };
    }
    if let Some(id) = nonempty_env("WEZTERM_PANE") {
        return Target { mux: Mux::Wezterm, id };
    }
    if let Some(id) = nonempty_env("KITTY_WINDOW_ID") {
        return Target { mux: Mux::Kitty, id };
    }
    if let Some(id) = nonempty_env("STY") {
        return Target { mux: Mux::Screen, id };
    }
    Target::none()
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// The exact argv command(s) that inject `text` (followed by a paste-safe Enter)
/// into `target`. Pure function — unit-tested for every backend without any
/// multiplexer present.
pub fn commands_for(target: &Target, text: &str) -> Vec<Vec<String>> {
    let id = &target.id;
    match target.mux {
        // tmux: type literal text, close bracketed-paste mode with the hex
        // ESC[201~ sequence (so the TUI doesn't treat the following Enter as a
        // key/cancel), then send Enter.
        Mux::Tmux => vec![
            argv(&["tmux", "send-keys", "-t", id, "-l", "--", text]),
            argv(&[
                "tmux", "send-keys", "-t", id, "-H", "1b", "5b", "32", "30", "31", "7e",
            ]),
            argv(&["tmux", "send-keys", "-t", id, "Enter"]),
        ],
        // zellij: write the literal chars, then write byte 13 (carriage return).
        Mux::Zellij => vec![
            argv(&[
                "zellij",
                "--session",
                id,
                "action",
                "write-chars",
                text,
            ]),
            argv(&["zellij", "--session", id, "action", "write", "13"]),
        ],
        // kitty: requires remote control. Match the target window by id; send the
        // text, then a carriage return as a separate send-text.
        Mux::Kitty => {
            let m = format!("id:{id}");
            vec![
                argv(&["kitten", "@", "send-text", "--match", &m, text]),
                argv(&["kitten", "@", "send-text", "--match", &m, CR]),
            ]
        }
        // wezterm: --no-paste avoids bracketed paste entirely; submit with CR.
        Mux::Wezterm => vec![
            argv(&[
                "wezterm", "cli", "send-text", "--pane-id", id, "--no-paste", text,
            ]),
            argv(&[
                "wezterm", "cli", "send-text", "--pane-id", id, "--no-paste", CR,
            ]),
        ],
        // screen: `stuff` injects the string into the window's input; append CR.
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
///   Ok(true)  — injected
///   Ok(false) — target not injectable (mux None / empty id)
///   Err(..)   — the mux binary is missing or a command failed (e.g. pane gone);
///               callers fall back to next-turn delivery.
pub fn inject(target: &Target, text: &str) -> Result<bool> {
    let cmds = commands_for(target, text);
    if cmds.is_empty() {
        return Ok(false);
    }
    let bin = target.mux.binary();
    if !have(bin) {
        bail!("{bin} not found on PATH (mux '{}')", target.mux.as_str());
    }
    for cmd in &cmds {
        let status = Command::new(&cmd[0]).args(&cmd[1..]).status()?;
        if !status.success() {
            bail!("`{}` exited with {}", cmd.join(" "), status);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(mux: Mux, id: &str) -> Target {
        Target {
            mux,
            id: id.into(),
        }
    }

    #[test]
    fn tmux_is_paste_safe() {
        let c = commands_for(&t(Mux::Tmux, "%3"), "hi");
        assert_eq!(c.len(), 3, "type + paste-close + Enter");
        assert_eq!(c[0], argv(&["tmux", "send-keys", "-t", "%3", "-l", "--", "hi"]));
        // ESC[201~ closes bracketed paste before Enter.
        assert_eq!(
            c[1],
            argv(&["tmux", "send-keys", "-t", "%3", "-H", "1b", "5b", "32", "30", "31", "7e"])
        );
        assert_eq!(c[2], argv(&["tmux", "send-keys", "-t", "%3", "Enter"]));
    }

    #[test]
    fn zellij_writes_cr() {
        let c = commands_for(&t(Mux::Zellij, "envctl"), "hi");
        assert_eq!(
            c[0],
            argv(&["zellij", "--session", "envctl", "action", "write-chars", "hi"])
        );
        assert_eq!(c[1], argv(&["zellij", "--session", "envctl", "action", "write", "13"]));
    }

    #[test]
    fn kitty_matches_window() {
        let c = commands_for(&t(Mux::Kitty, "7"), "hi");
        assert_eq!(c[0], argv(&["kitten", "@", "send-text", "--match", "id:7", "hi"]));
        assert_eq!(c[1], argv(&["kitten", "@", "send-text", "--match", "id:7", "\r"]));
    }

    #[test]
    fn wezterm_no_paste() {
        let c = commands_for(&t(Mux::Wezterm, "2"), "hi");
        assert_eq!(
            c[0],
            argv(&["wezterm", "cli", "send-text", "--pane-id", "2", "--no-paste", "hi"])
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
}
