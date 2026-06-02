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

// Backends statically link their own SQLite and cannot coexist; guard loudly.
#[cfg(all(feature = "sqlite", feature = "libsql"))]
compile_error!(
    "features `sqlite` and `libsql` are mutually exclusive (both statically link SQLite). \
     Build the libSQL backend with `--no-default-features --features libsql`."
);
#[cfg(not(any(feature = "sqlite", feature = "libsql")))]
compile_error!("no storage backend selected: enable `sqlite` (default) or `libsql`.");

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

/// Open the configured storage backend. Unknown backend names fail loudly rather
/// than silently defaulting to sqlite — a typo'd WEAVE_BACKEND (e.g. "sqlitee",
/// "turso") would otherwise land messages in a different store than intended.
fn open_store(cfg: &Config) -> Result<Box<dyn Store>> {
    let backend = cfg.backend();
    match backend.as_str() {
        "sqlite" => {
            #[cfg(feature = "sqlite")]
            {
                Ok(Box::new(store::SqliteStore::open(&cfg.db_path())?))
            }
            #[cfg(not(feature = "sqlite"))]
            {
                anyhow::bail!(
                    "backend 'sqlite' requires building weave with the `sqlite` feature (default)"
                )
            }
        }
        "libsql" => {
            #[cfg(feature = "libsql")]
            {
                // For LOCAL libsql the db/WEAVE_DB path IS the file. It is only
                // ignored when a remote libsql_url is set — warn just for that case.
                if cfg.libsql_url.is_some() && (cfg.db.is_some() || nonempty_env("WEAVE_DB")) {
                    eprintln!(
                        "[weave] note: libsql_url is set (remote backend), so the \
                         db/WEAVE_DB path override is ignored."
                    );
                }
                Ok(Box::new(store_libsql::LibsqlStore::open(cfg)?))
            }
            #[cfg(not(feature = "libsql"))]
            {
                anyhow::bail!("backend 'libsql' requires building weave with `--features libsql`")
            }
        }
        other => anyhow::bail!(
            "unknown backend '{other}' (set WEAVE_BACKEND / config `backend` to 'sqlite' or 'libsql')"
        ),
    }
}

/// Read a non-empty environment variable, mirroring `config::nonempty`.
#[cfg(feature = "libsql")]
fn nonempty_env(key: &str) -> bool {
    std::env::var(key).ok().filter(|s| !s.is_empty()).is_some()
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

fn try_inject(store: &dyn Store, cfg: &Config, from: &str, to: &str, body: &str) -> Result<()> {
    if model::is_broadcast(to) {
        return Ok(());
    }
    if let Some(peer) = store.get_peer(to)? {
        let t = inject::Target::from_peer(&peer);
        if t.injectable() {
            match inject::inject(&t, &cfg.nudge(from, body)) {
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
            let exe = std::env::current_exe()?.to_string_lossy().into_owned();
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
            try_inject(store, &cfg, &from, &to, &body)?;
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
            println!(
                "{}",
                if ok {
                    "injected"
                } else {
                    "peer not injectable"
                }
            );
        }

        Cmd::Hook { event } => handle_hook(store, &cfg, &event)?,
    }
    Ok(())
}

fn handle_hook(store: &dyn Store, cfg: &Config, event: &str) -> Result<()> {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("[weave] hook stdin read error: {e}");
    }
    // Track whether the payload actually parsed: a garbled/empty payload means we
    // cannot trust `cwd` for identity, and must not guess one for a read-marking
    // drain (which would consume another session's inbox).
    let (v, payload_ok) = if buf.trim().is_empty() {
        (serde_json::json!({}), false)
    } else {
        match serde_json::from_str::<serde_json::Value>(&buf) {
            Ok(v) => (v, true),
            Err(e) => {
                eprintln!("[weave] hook payload is not valid JSON ({e}); ignoring its fields");
                (serde_json::json!({}), false)
            }
        }
    };
    let cwd = v.get("cwd").and_then(|x| x.as_str());
    let me = resolve_me(None, cwd, cfg);

    // An identity is "explicit" (trustworthy) when it comes from config/$WEAVE_SESSION
    // or from a `cwd` the payload actually supplied — NOT from basename(current_dir()),
    // which in a hook is not guaranteed to be the project dir.
    let explicit_identity = cfg.session.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
        || (payload_ok && cwd.is_some());

    match event {
        "session" => {
            let t = inject::detect_target();
            store.register_peer(&me, t.mux.as_str(), &t.id, cwd)?;
            eprintln!("[weave] registered peer '{me}' [{}]", t.mux.as_str());
        }
        // UserPromptSubmit: Claude Code injects this hook's stdout into the model
        // as additionalContext, so the printed messages are actually delivered.
        // Only here do we mark messages read — the drain and the delivery are the
        // same event.
        //
        // Stop: Claude Code does NOT add Stop-hook stdout to the model context on a
        // normal exit, so anything we print here is never seen. We therefore PEEK
        // (mark_read=false) so the messages remain unread and the next
        // UserPromptSubmit drain re-surfaces and marks them. Marking them read here
        // would silently consume them — concrete message loss.
        "prompt" | "stop" => {
            let mut mark_read = event == "prompt";
            // Never mark messages read under a guessed identity: if we had to fall
            // back to basename(current_dir()) (no config session, no payload cwd),
            // peek instead so we cannot permanently consume another session's inbox.
            if mark_read && !explicit_identity {
                eprintln!(
                    "[weave] no explicit session identity (set WEAVE_SESSION or config `session`); \
                     peeking inbox for guessed '{me}' without marking read"
                );
                mark_read = false;
            }
            let (rows, _) = store.inbox(&me, false, mark_read, 50)?;
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
