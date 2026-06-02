//! weave — Rust-native agent-to-agent session mesh with a native injector.
//!
//! Subcommands:
//!   weave mcp            run the MCP stdio server (register with `claude mcp add`)
//!   weave setup          wire weave into Claude Code (MCP + hooks)
//!   weave uninstall      remove weave's Claude Code wiring
//!   weave send           send a message (CLI)
//!   weave reply          reply to a message, addressed to the original sender
//!   weave inbox          read your inbox (CLI)
//!   weave thread         print a message thread by its root id
//!   weave receipts       show who has read a message, and when
//!   weave watch          tail your inbox, printing new messages until Ctrl-C
//!   weave peers          list registered peers (with presence)
//!   weave sessions       list known sessions
//!   weave register       register this session as an injectable peer
//!   weave inject         manually inject text into a peer's pane (test)
//!   weave config init    scaffold a commented ~/.config/weave/config.toml
//!   weave completions    print a shell completion script (bash|zsh|fish)
//!   weave man            print a roff man page to stdout
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

/// `--version` provenance: the package version plus the storage backend(s)
/// compiled into THIS binary. Because the `sqlite` and `libsql` backends are
/// mutually exclusive (and a `compile_error!` enforces exactly one), this lists
/// the single backend that is actually linked — so `weave --version` tells you at
/// a glance whether you are running the bundled-sqlite or the libSQL build,
/// without having to run `weave doctor` against a live store.
fn long_version() -> &'static str {
    // Computed once; clap wants a &'static str for `long_version`.
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // cfg attributes on array elements drop out the backends not compiled in,
        // so this is exactly the linked set — and avoids a Vec::new()+push pattern.
        let backends: Vec<&str> = [
            #[cfg(feature = "sqlite")]
            "sqlite",
            #[cfg(feature = "libsql")]
            "libsql",
        ]
        .to_vec();
        let backends = if backends.is_empty() {
            // Unreachable in a valid build (a compile_error! guards the no-backend
            // case), but keep the string total rather than panicking.
            "none".to_string()
        } else {
            backends.join(", ")
        };
        format!("{}\nbackends: {}", env!("CARGO_PKG_VERSION"), backends)
    })
}

/// Long `--help` preamble. Documents the exit-code contract so scripts wrapping
/// weave (hooks, watchers, CI) can branch on the process status.
const LONG_ABOUT: &str = "\
Rust-native agent-to-agent session mesh with a native injector.

weave lets independent Claude Code (or shell) sessions message one another and,
where a multiplexer pane is known, injects a live nudge into the recipient.

EXIT CODES
  0   success
  1   runtime error (bad arguments after parsing, store/IO failure, unknown
      backend, missing peer, or any other anyhow error)
  2   command-line usage error (clap: unknown flag/subcommand, missing required
      argument, or a bad value)

Note: a failed live injection is NOT an error — the message is still persisted
and delivered on the recipient's next inbox drain, so weave exits 0.";

#[derive(Parser)]
#[command(
    name = "weave",
    version,
    long_version = long_version(),
    about = "Rust-native agent-to-agent session mesh with a native injector",
    long_about = LONG_ABOUT
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
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    /// Reply to a message; the reply is auto-addressed to the original sender and
    /// linked to the parent via in_reply_to (so it shows up in `weave thread`).
    Reply {
        /// id of the message you are replying to
        #[arg(long = "in-reply-to")]
        in_reply_to: i64,
        /// your identity (defaults to config/$WEAVE_SESSION or basename of cwd)
        #[arg(long)]
        from: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    /// Print a message thread (a root message and everything chained to it).
    Thread {
        /// id of the thread root (the first message in the conversation)
        #[arg(long)]
        root: i64,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show read receipts for a message: who has read it, and when.
    Receipts {
        /// id of the message to inspect
        #[arg(long)]
        id: i64,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Tail your inbox: poll the store and print new messages until interrupted
    /// (Ctrl-C). Messages are peeked (not marked read) so your normal hook drain
    /// still delivers them.
    Watch {
        #[arg(long)]
        me: Option<String>,
        /// poll interval in seconds
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// also surface already-read messages on the first poll
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
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
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// List registered peers (with presence + injectability).
    Peers {
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// List known sessions with unread counts.
    Sessions {
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Delete messages older than the given age (retention / disk guard).
    Gc {
        /// age threshold in seconds (default 30 days)
        #[arg(long, default_value_t = 2_592_000)]
        older_than_secs: i64,
    },
    /// Print diagnostics: backend, db path, detected mux, peers, Claude wiring.
    Doctor {
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
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
        #[arg(long, allow_hyphen_values = true)]
        text: String,
        /// inject a short content-free ping instead of the text itself
        #[arg(long)]
        quiet: bool,
    },
    /// Configuration helpers.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Print a shell completion script to stdout (eval/source it in your shell).
    Completions {
        /// target shell
        shell: CompletionShell,
    },
    /// Print a roff man page for weave to stdout.
    Man,
    /// Claude Code lifecycle hook: session|prompt|stop|notification (reads JSON on stdin).
    Hook { event: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Scaffold a commented ~/.config/weave/config.toml. Never overwrites an
    /// existing file, so it is safe to run repeatedly.
    Init,
}

/// Shells we can emit completion scripts for. Kept as a local enum (rather than
/// re-exporting clap_complete::Shell) so `weave completions <shell>` parses and
/// `--help` lists the choices even in builds where the optional `cli-extras`
/// crates are not compiled in; the actual generation is then feature-gated.
#[derive(Clone, Copy, clap::ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
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
    resolve_me_explicit(opt, cwd, cfg).0
}

/// Like [`resolve_me`], but also reports whether the identity was *explicit*
/// (came from an explicit `--from`/`--me`/`--name` flag or from config/
/// `$WEAVE_SESSION`) versus merely *guessed* from `basename(cwd)`. Presence
/// refresh (`touch_peer`) and read-marking are only safe under an explicit
/// identity — a guess could belong to another session.
fn resolve_me_explicit(opt: Option<String>, cwd: Option<&str>, cfg: &Config) -> (String, bool) {
    if let Some(s) = opt {
        if !s.trim().is_empty() {
            return (s.trim().to_string(), true);
        }
    }
    if let Some(s) = &cfg.session {
        if !s.is_empty() {
            return (s.clone(), true);
        }
    }
    let cwd_path = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let name = cwd_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (name, false)
}

/// Refresh this session's presence (heartbeat) at the start of a command, but
/// ONLY when the identity is explicit. We deliberately do NOT register a new peer
/// here — `touch_peer` updates `last_seen` for a peer that already registered (via
/// `weave register` or the session hook) and is a no-op otherwise, so simply
/// reading your inbox keeps you showing "online" without inventing phantom peers
/// or clobbering a registered pane's mux/target. A heartbeat failure must never
/// sink the actual command, so errors are reported and swallowed.
fn refresh_presence(store: &dyn Store, name: &str, explicit: bool) {
    if !explicit {
        return;
    }
    if let Err(e) = store.touch_peer(name) {
        eprintln!("[weave] presence refresh failed for '{name}': {e}");
    }
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

/// Diagnostics: backend, db, detected multiplexer, peers, Claude wiring.
fn doctor(store: &dyn Store, cfg: &Config, json: bool) -> Result<()> {
    let target = inject::detect_target();
    let peers = store.list_peers()?;
    let online = peers.iter().filter(|p| is_online(p.last_seen)).count();
    let total = store.total_messages()?;
    let claude = inject::have("claude");
    let db = cfg.db_path();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "backend": store.backend(),
                "db_path": db.to_string_lossy(),
                "config_path": config::config_path().to_string_lossy(),
                "current_mux": target.mux.as_str(),
                "current_target": target.id,
                "injectable_here": target.injectable(),
                "total_messages": total,
                "peers": peers.len(),
                "peers_online": online,
                "claude_on_path": claude,
            }))?
        );
    } else {
        let tgt = if target.id.is_empty() {
            "-"
        } else {
            &target.id
        };
        println!("weave doctor");
        println!("  version:        {}", env!("CARGO_PKG_VERSION"));
        println!("  backend:        {}", store.backend());
        println!("  db:             {}", db.display());
        println!("  config:         {}", config::config_path().display());
        println!(
            "  this session:   mux={} target={} injectable={}",
            target.mux.as_str(),
            tgt,
            target.injectable()
        );
        println!("  messages:       {total}");
        println!("  peers:          {} ({online} online)", peers.len());
        println!("  claude on PATH: {}", if claude { "yes" } else { "no" });
    }
    Ok(())
}

/// Upper bound on messages returned for a `thread` view.
const MAX_THREAD: i64 = 1_000;

/// Emit a shell completion script to stdout.
fn print_completions(shell: CompletionShell) -> Result<()> {
    use clap::CommandFactory;
    use clap_complete::Shell;
    let sh = match shell {
        CompletionShell::Bash => Shell::Bash,
        CompletionShell::Zsh => Shell::Zsh,
        CompletionShell::Fish => Shell::Fish,
    };
    let mut cmd = Cli::command();
    clap_complete::generate(sh, &mut cmd, "weave", &mut std::io::stdout());
    Ok(())
}

/// Emit a roff man page for weave to stdout.
fn print_man() -> Result<()> {
    use clap::CommandFactory;
    clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
    Ok(())
}

/// Poll the inbox and print new messages until interrupted (Ctrl-C). Always a
/// PEEK (never marks read) so the normal hook drain still delivers them.
fn watch(
    store: &dyn Store,
    _cfg: &Config,
    me: &str,
    _explicit: bool,
    interval: u64,
    all: bool,
    limit: i64,
) -> Result<()> {
    eprintln!("[weave] watching inbox for '{me}' every {interval}s (Ctrl-C to stop)");
    let mut seen: i64 = 0;
    let mut first = true;
    loop {
        let include_read = all && first;
        let (rows, _) = store.inbox(me, include_read, false, limit)?;
        for m in &rows {
            if m.id > seen {
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
                seen = seen.max(m.id);
            }
        }
        first = false;
        std::thread::sleep(std::time::Duration::from_secs(interval.max(1)));
    }
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
        Cmd::Config {
            cmd: ConfigCmd::Init,
        } => {
            return match config::init_config_file()? {
                config::ConfigInit::Created(p) => {
                    println!("wrote config scaffold: {}", p.display());
                    println!(
                        "edit it to set `session`, `backend`, etc. (all keys start commented)"
                    );
                    Ok(())
                }
                config::ConfigInit::Existed(p) => {
                    // Existing config is sacred (may hold secrets); report and exit 0.
                    println!(
                        "config already exists, leaving it untouched: {}",
                        p.display()
                    );
                    Ok(())
                }
            };
        }
        Cmd::Completions { shell } => return print_completions(*shell),
        Cmd::Man => return print_man(),
        _ => {}
    }

    let store = open_store(&cfg)?;
    let store = store.as_ref();

    match cli.cmd {
        Cmd::Setup | Cmd::Uninstall | Cmd::Config { .. } | Cmd::Completions { .. } | Cmd::Man => {
            unreachable!("handled above")
        }

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
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let mid = store.send(&from, &to, subject.as_deref(), &body)?;
            println!("sent #{mid}: {from} -> {to}");
            try_inject(store, &cfg, &from, &to, &body)?;
        }

        Cmd::Reply {
            in_reply_to,
            from,
            body,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            // The store looks up the parent's sender/recipient to address the reply
            // and stores the in_reply_to link; it returns the new message id.
            let mid = store.reply(&from, in_reply_to, &body)?;
            println!("replied #{mid} (in-reply-to #{in_reply_to}) from {from}");
            // Live-nudge the recipient if we can determine who that is (the parent's
            // other party). The new message row tells us its recipient.
            if let Some(parent) = store
                .thread(in_reply_to, MAX_THREAD)
                .ok()
                .and_then(|t| t.into_iter().find(|m| m.id == mid))
            {
                try_inject(store, &cfg, &from, &parent.recipient, &body)?;
            }
        }

        Cmd::Thread { root, limit, json } => {
            let rows = store.thread(root, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "root": root, "messages": rows
                    }))?
                );
            } else if rows.is_empty() {
                println!("thread #{root}: empty (no such root, or no messages)");
            } else {
                for m in &rows {
                    let subj = m
                        .subject
                        .as_ref()
                        .map(|s| format!(" | {s}"))
                        .unwrap_or_default();
                    // Show the reply linkage so the conversation structure is visible
                    // in plain text without needing --json.
                    let reply = m
                        .in_reply_to
                        .map(|r| format!(" (re #{r})"))
                        .unwrap_or_default();
                    println!(
                        "#{}{} [{}] {} -> {}{}\n  {}",
                        m.id,
                        reply,
                        model::fmt_ts(m.ts),
                        m.sender,
                        m.recipient,
                        subj,
                        m.body
                    );
                }
            }
        }

        Cmd::Receipts { id, json } => {
            let receipts = store.receipts(id)?;
            if json {
                let arr: Vec<_> = receipts
                    .iter()
                    .map(|(reader, ts)| serde_json::json!({"reader": reader, "ts": ts}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "message_id": id, "receipts": arr
                    }))?
                );
            } else if receipts.is_empty() {
                println!("#{id}: no reads yet");
            } else {
                println!("#{id} read by:");
                for (reader, ts) in &receipts {
                    println!("  {reader} at {}", model::fmt_ts(*ts));
                }
            }
        }

        Cmd::Watch {
            me,
            interval,
            all,
            limit,
        } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            watch(store, &cfg, &me, explicit, interval, all, limit)?;
        }

        Cmd::Inbox {
            me,
            all,
            peek,
            limit,
            json,
        } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            let (rows, remaining) = store.inbox(&me, all, !peek, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "me": me, "messages": rows, "remaining_unread": remaining
                    }))?
                );
            } else if rows.is_empty() {
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

        Cmd::Peers { json } => {
            let peers = store.list_peers()?;
            if json {
                let arr: Vec<_> = peers
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name, "mux": p.mux, "target": p.target, "cwd": p.cwd,
                            "last_seen": p.last_seen,
                            "online": is_online(p.last_seen),
                            "injectable": inject::Target::from_peer(p).injectable(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
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
        }

        Cmd::Sessions { json } => {
            let info = store.sessions()?;
            if json {
                let arr: Vec<_> = info
                    .iter()
                    .map(|(n, unread, last)| {
                        serde_json::json!({"name": n, "unread": unread, "last_activity": last})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                if info.is_empty() {
                    println!("no sessions yet");
                }
                for (n, unread, last) in info {
                    println!("{n}: {unread} unread (last {})", model::fmt_ts(last));
                }
            }
        }

        Cmd::Gc { older_than_secs } => {
            let n = store.gc(older_than_secs)?;
            println!("gc: deleted {n} message(s) older than {older_than_secs}s");
        }

        Cmd::Doctor { json } => doctor(store, &cfg, json)?,

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

        Cmd::Inject { to, text, quiet } => {
            let peer = store
                .get_peer(&to)?
                .ok_or_else(|| anyhow::anyhow!("no registered peer '{to}'"))?;
            let t = inject::Target::from_peer(&peer);
            let mode = if quiet {
                inject::Nudge::Nudge
            } else {
                inject::Nudge::Full
            };
            let ok = inject::inject_mode(&t, &text, mode)?;
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
