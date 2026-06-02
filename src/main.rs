//! weave — Rust-native agent-to-agent session mesh with a native injector.
//!
//! Subcommands:
//!   weave mcp            run the MCP stdio server (register with `claude mcp add`)
//!   weave setup          wire weave into Claude Code (MCP + hooks)
//!   weave uninstall      remove weave's Claude Code wiring
//!   weave send           send a message (CLI)
//!   weave inbox          read your inbox (CLI)
//!   weave peers          list registered peers (with presence)
//!   weave sessions       list known sessions
//!   weave register       register this session as an injectable peer
//!   weave inject         manually inject text into a peer's pane (test)
//!   weave hook <event>   Claude Code lifecycle hook: session|prompt|stop|notification

mod config;
mod inject;
mod mcp;
mod model;
mod setup;
mod store;
#[cfg(feature = "libsql")]
mod store_libsql;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use std::io::Read;
use std::path::PathBuf;
use store::{is_online, Store};

#[derive(Parser)]
#[command(
    name = "weave",
    version,
    about = "Rust-native agent-to-agent session mesh with a native injector"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP stdio server.
    Mcp {
        #[arg(long)]
        session: Option<String>,
    },
    /// Wire weave into Claude Code (register MCP server + lifecycle hooks).
    Setup,
    /// Remove weave's Claude Code wiring.
    Uninstall,
    /// Send a message to another session.
    Send {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        body: String,
    },
    /// Read your inbox.
    Inbox {
        #[arg(long)]
        me: Option<String>,
        /// include already-read
        #[arg(long)]
        all: bool,
        /// do not mark read
        #[arg(long)]
        peek: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// List registered peers (with presence + injectability).
    Peers,
    /// List known sessions with unread counts.
    Sessions,
    /// Register this session as an injectable peer (captures the current pane).
    Register {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Manually inject text into a peer's pane (test the injector).
    Inject {
        #[arg(long)]
        to: String,
        #[arg(long)]
        text: String,
    },
    /// Claude Code lifecycle hook: session|prompt|stop|notification (reads JSON on stdin).
    Hook { event: String },
}

/// Open the configured storage backend.
fn open_store(cfg: &Config) -> Result<Box<dyn Store>> {
    match cfg.backend().as_str() {
        "libsql" => {
            #[cfg(feature = "libsql")]
            {
                Ok(Box::new(store_libsql::LibsqlStore::open(cfg)?))
            }
            #[cfg(not(feature = "libsql"))]
            {
                anyhow::bail!(
                    "backend 'libsql' requires building weave with `--features libsql`"
                )
            }
        }
        _ => Ok(Box::new(store::SqliteStore::open(&cfg.db_path())?)),
    }
}

/// Resolve this session's name: explicit > config/$WEAVE_SESSION > basename(cwd).
fn resolve_me(opt: Option<String>, cwd: Option<&str>, cfg: &Config) -> String {
    if let Some(s) = opt {
        if !s.trim().is_empty() {
            return s.trim().to_string();
        }
    }
    if let Some(s) = &cfg.session {
        if !s.is_empty() {
            return s.clone();
        }
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    cwd_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn try_inject(store: &dyn Store, cfg: &Config, from: &str, to: &str) -> Result<()> {
    if model::is_broadcast(to) {
        return Ok(());
    }
    if let Some(peer) = store.get_peer(to)? {
        let t = inject::Target::from_peer(&peer);
        if t.injectable() {
            match inject::inject(&t, &cfg.nudge(from)) {
                Ok(true) => println!("injected into {} '{}'", t.mux.as_str(), t.id),
                Ok(false) => {}
                Err(err) => eprintln!("inject failed ({err}); will arrive on next turn"),
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();

    // Commands that don't need the store.
    match &cli.cmd {
        Cmd::Setup => {
            let exe = std::env::current_exe()?
                .to_string_lossy()
                .into_owned();
            return setup::run(&exe);
        }
        Cmd::Uninstall => return setup::uninstall(),
        _ => {}
    }

    let store = open_store(&cfg)?;
    let store = store.as_ref();

    match cli.cmd {
        Cmd::Setup | Cmd::Uninstall => unreachable!("handled above"),

        Cmd::Mcp { session } => {
            let def = session
                .filter(|s| !s.is_empty())
                .or_else(|| cfg.session.clone());
            mcp::run(store, def)?;
        }

        Cmd::Send {
            from,
            to,
            subject,
            body,
        } => {
            let from = resolve_me(from, None, &cfg);
            let mid = store.send(&from, &to, subject.as_deref(), &body)?;
            println!("sent #{mid}: {from} -> {to}");
            try_inject(store, &cfg, &from, &to)?;
        }

        Cmd::Inbox {
            me,
            all,
            peek,
            limit,
        } => {
            let me = resolve_me(me, None, &cfg);
            let (rows, remaining) = store.inbox(&me, all, !peek, limit)?;
            if rows.is_empty() {
                println!("inbox '{me}': empty");
            } else {
                for m in &rows {
                    let subj = m
                        .subject
                        .as_ref()
                        .map(|s| format!(" | {s}"))
                        .unwrap_or_default();
                    println!(
                        "#{} [{}] {} -> {}{}\n  {}",
                        m.id,
                        model::fmt_ts(m.ts),
                        m.sender,
                        m.recipient,
                        subj,
                        m.body
                    );
                }
                if remaining > 0 {
                    println!("({remaining} more unread)");
                }
            }
        }

        Cmd::Peers => {
            let peers = store.list_peers()?;
            if peers.is_empty() {
                println!("no peers registered");
            }
            for p in peers {
                let inj = if inject::Target::from_peer(&p).injectable() {
                    "injectable"
                } else {
                    "no-inject"
                };
                let presence = if is_online(p.last_seen) {
                    "online"
                } else {
                    "offline"
                };
                let tgt = if p.target.is_empty() { "-" } else { &p.target };
                println!(
                    "{} [{presence}] [{}] {} ({inj}) seen {}",
                    p.name,
                    p.mux,
                    tgt,
                    model::fmt_ts(p.last_seen)
                );
            }
        }

        Cmd::Sessions => {
            let info = store.sessions()?;
            if info.is_empty() {
                println!("no sessions yet");
            }
            for (n, unread, last) in info {
                println!("{n}: {unread} unread (last {})", model::fmt_ts(last));
            }
        }

        Cmd::Register { name, cwd } => {
            let me = resolve_me(name, cwd.as_deref(), &cfg);
            let t = inject::detect_target();
            let cwd_val = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            store.register_peer(&me, t.mux.as_str(), &t.id, cwd_val.as_deref())?;
            let tgt = if t.id.is_empty() {
                "-".to_string()
            } else {
                t.id.clone()
            };
            println!("registered '{me}' [{}] {}", t.mux.as_str(), tgt);
        }

        Cmd::Inject { to, text } => {
            let peer = store
                .get_peer(&to)?
                .ok_or_else(|| anyhow::anyhow!("no registered peer '{to}'"))?;
            let t = inject::Target::from_peer(&peer);
            let ok = inject::inject(&t, &text)?;
            println!("{}", if ok { "injected" } else { "peer not injectable" });
        }

        Cmd::Hook { event } => handle_hook(store, &cfg, &event)?,
    }
    Ok(())
}

fn handle_hook(store: &dyn Store, cfg: &Config, event: &str) -> Result<()> {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let v: serde_json::Value = serde_json::from_str(&buf).unwrap_or(serde_json::json!({}));
    let cwd = v.get("cwd").and_then(|x| x.as_str());
    let me = resolve_me(None, cwd, cfg);

    match event {
        "session" => {
            let t = inject::detect_target();
            store.register_peer(&me, t.mux.as_str(), &t.id, cwd)?;
            eprintln!("[weave] registered peer '{me}' [{}]", t.mux.as_str());
        }
        "prompt" | "stop" => {
            let (rows, _) = store.inbox(&me, false, true, 50)?;
            if !rows.is_empty() {
                println!("[weave] {} new message(s) for '{me}':", rows.len());
                for m in &rows {
                    let subj = m
                        .subject
                        .as_ref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default();
                    println!("  #{} from {}{}: {}", m.id, m.sender, subj, m.body);
                }
            }
        }
        "notification" => { /* reserved for future use */ }
        other => eprintln!("[weave] unknown hook event: {other}"),
    }
    Ok(())
}
