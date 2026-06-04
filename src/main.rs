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
//!   weave attach         adopt this running session into the store (no restart)
//!   weave connect        probe whether a peer can be live-nudged right now
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
use store::{is_alive, Store};

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
    /// Adopt this running session into the shared store WITHOUT restarting:
    /// re-capture the current pane env and upsert your own peer row. The zero-
    /// restart path to becoming visible/injectable to other sessions.
    Attach {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Probe whether a peer can be reached by a live nudge right now, and report
    /// the verdict. A not-alive / non-injectable peer is NOT an error — its
    /// messages are still delivered via the store on its next turn.
    Connect {
        #[arg(long)]
        to: String,
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
    // Tier-1 federation: report the union peer count (local + read-only extra
    // stores). `extra` empty ⇒ exactly the local peers, identical-to-today.
    let extra = cfg.peer_db_paths();
    let views = store::federated_peers(store, &extra)?;
    let total_peers = views.len();
    let online = views.iter().filter(|v| is_alive(&v.peer)).count();
    let (fed_ok, fed_skipped) = store::federation_status(&extra);
    let total = store.total_messages()?;
    let claude = inject::have("claude");
    let db = cfg.db_path();
    // FR6: warn when the resolved store is NOT the well-known XDG default. The most
    // common "why can't I see the other session's peers" cause is each session
    // pointing at a different WEAVE_DB, so surface it as a diagnostic hint.
    let db_default = config::default_db_path();
    let db_is_default = db == db_default;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "backend": store.backend(),
                "db_path": db.to_string_lossy(),
                "db_is_default": db_is_default,
                "config_path": config::config_path().to_string_lossy(),
                "current_mux": target.mux.as_str(),
                "current_target": target.id,
                "injectable_here": target.injectable(),
                "total_messages": total,
                "peers": total_peers,
                "peers_online": online,
                "claude_on_path": claude,
                "federation_stores": extra.len(),
                "federation_stores_ok": fed_ok,
                "federation_stores_skipped": fed_skipped,
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
        println!("  peers:          {total_peers} ({online} online)");
        println!("  claude on PATH: {}", if claude { "yes" } else { "no" });
        if !extra.is_empty() {
            // Federation is configured: surface its health. This replaces the
            // non-default-WEAVE_DB hint below for the federated case (the whole
            // point of federation is to see across stores).
            println!(
                "  federation:     {} extra store(s) ({fed_ok} ok, {fed_skipped} skipped)",
                extra.len()
            );
        } else if !db_is_default {
            println!(
                "  note: using non-default WEAVE_DB — peers on a different store won't be visible (default: {})",
                db_default.display()
            );
        }
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
///
/// Strictly id-ascending high-water paging via [`Store::inbox_since`]: we track
/// the highest id we have printed (`last`) and on each tick fetch *only* messages
/// with `id > last`, oldest-first. When a single tick's backlog exceeds `limit`
/// we page within that tick (loop until a short page) so a burst larger than the
/// page size is never silently dropped — every message is printed exactly once,
/// in id order, and `last` only ever moves forward.
///
/// `all` seeds the starting high-water mark: by default we begin at the current
/// max delivered id (so watch shows only messages that arrive *after* it starts);
/// with `--all` we begin at 0 so the existing backlog is surfaced on the first
/// pass too. Either way subsequent ticks only ever see strictly-newer ids.
fn watch(
    store: &dyn Store,
    _cfg: &Config,
    me: &str,
    explicit: bool,
    interval: u64,
    all: bool,
    limit: i64,
) -> Result<()> {
    eprintln!("[weave] watching inbox for '{me}' every {interval}s (Ctrl-C to stop)");
    // Page size for one inbox_since call. A non-positive --limit would make no
    // forward progress (and clamp_limit maps negatives to a huge cap), so floor
    // it to a sane minimum for paging.
    let page = if limit <= 0 { 50 } else { limit };

    // Seed the high-water mark. Without --all, jump past the existing backlog so
    // watch reports only what lands after it starts; the seed peek is a single
    // bounded fetch from the front (id-asc) advanced to the last row.
    let mut last: i64 = 0;
    if !all {
        // Drain forward once (without printing) to discover the current max id.
        loop {
            let rows = store.inbox_since(me, last, page)?;
            let n = rows.len();
            if let Some(m) = rows.last() {
                last = last.max(m.id);
            }
            if n < page as usize {
                break;
            }
        }
    }

    loop {
        // Page within the tick until a short page: a backlog larger than `page`
        // is fully drained this tick rather than trickled one page per interval.
        loop {
            let rows = store.inbox_since(me, last, page)?;
            let n = rows.len();
            for m in &rows {
                // inbox_since is strictly id>last ascending, but guard anyway so a
                // backend that returns an inclusive/duplicate row can't reprint.
                if m.id <= last {
                    continue;
                }
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
                last = last.max(m.id);
            }
            // Short page ⇒ caught up for now; stop paging until the next tick.
            if n < page as usize {
                break;
            }
        }
        // A1 heartbeat: a long-lived `weave watch` is an actively-attended session,
        // so keep its presence warm each tick even when no messages arrive. Best-
        // effort + explicit-identity-only via refresh_presence; a heartbeat failure
        // must never abort the watch.
        refresh_presence(store, me, explicit);
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
            // Plumb the configured nudge template (if any) into the MCP server so
            // its live-injection nudges honor the same `nudge_template` the CLI
            // uses. `None` ⇒ the server falls back to its built-in default text.
            let nudge_tpl = cfg.nudge_template().map(str::to_owned);
            // Tier-1 federation: pass the validated read-only extra store paths so
            // the MCP peers/sessions/doctor tools aggregate them too.
            let extra_dbs = cfg.peer_db_paths();
            mcp::run(store, def, nudge_tpl.as_deref(), extra_dbs)?;
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
            // A1 heartbeat-on-read: listing peers is a cheap, frequently-hit path,
            // so use it to keep our own `last_seen` warm. Best-effort and explicit-
            // identity-only (refresh_presence guards both): a heartbeat write failure
            // must never abort the read.
            let (me, explicit) = resolve_me_explicit(None, None, &cfg);
            refresh_presence(store, &me, explicit);
            // Tier-1 federation: union the local peers with any configured
            // read-only extra stores, origin-tagged. Default (no WEAVE_PEER_DBS /
            // [federation] peer_dbs) ⇒ `extra` is empty ⇒ output is the local
            // listing tagged `local`, byte-identical to single-store behavior.
            let extra = cfg.peer_db_paths();
            let views = store::federated_peers(store, &extra)?;
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|v| {
                        let p = &v.peer;
                        serde_json::json!({
                            "name": p.name, "mux": p.mux, "target": p.target,
                            "socket": p.socket, "cwd": p.cwd,
                            "last_seen": p.last_seen,
                            "pid": p.pid, "host": p.host,
                            "online": is_alive(p),
                            "alive": is_alive(p),
                            "injectable": inject::Target::from_peer(p).injectable(),
                            "origin": v.origin.label(),
                            "foreign": v.origin.is_foreign(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                if views.is_empty() {
                    println!("no peers registered");
                }
                for v in &views {
                    let p = &v.peer;
                    let inj = if inject::Target::from_peer(p).injectable() {
                        "injectable"
                    } else {
                        "no-inject"
                    };
                    let presence = if is_alive(p) { "online" } else { "offline" };
                    let tgt = if p.target.is_empty() { "-" } else { &p.target };
                    let via = if v.origin.is_foreign() {
                        format!(" (via {})", v.origin.label())
                    } else {
                        String::new()
                    };
                    println!(
                        "{} [{presence}] [{}] {} ({inj}) seen {}{via}",
                        p.name,
                        p.mux,
                        tgt,
                        model::fmt_ts(p.last_seen)
                    );
                }
            }
        }

        Cmd::Sessions { json } => {
            // Tier-1 federation: union local sessions with read-only extra stores,
            // origin-tagged. Foreign sessions are kept distinct (no unread summing —
            // Tier 1 has no cross-store inbox). Default ⇒ identical-to-today.
            let extra = cfg.peer_db_paths();
            let views = store::federated_sessions(store, &extra)?;
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|v| {
                        serde_json::json!({
                            "name": v.name, "unread": v.unread, "last_activity": v.last_activity,
                            "origin": v.origin.label(), "foreign": v.origin.is_foreign(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                if views.is_empty() {
                    println!("no sessions yet");
                }
                for v in &views {
                    let via = if v.origin.is_foreign() {
                        format!(" (via {})", v.origin.label())
                    } else {
                        String::new()
                    };
                    println!(
                        "{}: {} unread (last {}){via}",
                        v.name,
                        v.unread,
                        model::fmt_ts(v.last_activity)
                    );
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
            // Persist the captured kitty control socket (KITTY_LISTEN_ON; empty for
            // every other backend) so a remote sender can reach a `--listen-on`
            // kitty via `kitten --to <socket>` without re-detecting it. Capture
            // this process's PID + host so presence reflects real liveness.
            store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd_val.as_deref(),
                Some(std::process::id() as i64),
                &config::this_host(),
            )?;
            let tgt = if t.id.is_empty() {
                "-".to_string()
            } else {
                t.id.clone()
            };
            println!("registered '{me}' [{}] {}", t.mux.as_str(), tgt);
        }

        Cmd::Attach { name, cwd } => {
            // Bind the row key to OUR OWN resolved identity — attach upserts the
            // caller's own peer row only, never an arg-supplied foreign target.
            let me = resolve_me(name, cwd.as_deref(), &cfg);
            // Validate identity up front (the store also enforces this, but failing
            // here keeps the error close to the input).
            store::check_ident("name", &me)?;
            let t = inject::detect_target();
            // If a mux was detected, the captured pane id must match that mux's
            // expected shape; a structurally invalid injectable target is refused so
            // we never persist a poisoned, un-injectable registration. A legitimate
            // mux=none (no multiplexer) has an empty id and is allowed (store-only).
            if t.injectable() && !inject::id_valid(t.mux, &t.id) {
                anyhow::bail!(
                    "refusing to attach: captured target {:?} is not a valid {} target",
                    t.id,
                    t.mux.as_str()
                );
            }
            let cwd_val = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            // Idempotent upsert (ON CONFLICT(name) DO UPDATE) under our own identity.
            // Capture this process's PID + host so the adopted peer reflects real
            // liveness (the whole point of zero-restart attach).
            store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd_val.as_deref(),
                Some(std::process::id() as i64),
                &config::this_host(),
            )?;
            let tgt = if t.id.is_empty() {
                "-".to_string()
            } else {
                t.id.clone()
            };
            let inj = if t.injectable() {
                "injectable"
            } else {
                "no-inject"
            };
            println!("attached '{me}' [{}] {tgt} ({inj})", t.mux.as_str());
        }

        Cmd::Connect { to } => {
            let peer = store
                .get_peer(&to)?
                .ok_or_else(|| anyhow::anyhow!("no registered peer '{to}'"))?;
            let t = inject::Target::from_peer(&peer);
            match inject::capability(&t) {
                inject::Capability::Live => {
                    println!(
                        "connect '{to}': live [{}] {} — a live nudge can be delivered now",
                        t.mux.as_str(),
                        t.id
                    );
                }
                inject::Capability::RegisteredNotAlive => {
                    println!(
                        "connect '{to}': registered but not alive [{}] {} — \
                         delivery will be queued; recipient drains on next turn",
                        t.mux.as_str(),
                        t.id
                    );
                }
                inject::Capability::NotInjectable => {
                    println!(
                        "connect '{to}': not injectable (mux=none) — \
                         delivery will be queued; recipient drains on next turn"
                    );
                }
            }
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
            // Pass the captured kitty control socket through (empty for non-kitty);
            // see the Register arm. A poisoned/empty socket is harmless — only the
            // kitty injector consults it. Capture PID + host so presence reflects
            // real process-liveness for this hook-registered session.
            store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd,
                Some(std::process::id() as i64),
                &config::this_host(),
            )?;
            eprintln!("[weave] registered peer '{me}' [{}]", t.mux.as_str());
            // S2 — opportunistic retention sweep. Best-effort: a GC failure must
            // never sink the session hook (which also drives presence/registration),
            // so errors are reported and swallowed. A configured retention of 0
            // disables the sweep entirely.
            let retention = cfg.retention();
            if retention > 0 {
                match store.gc(retention) {
                    Ok(n) if n > 0 => {
                        eprintln!("[weave] gc: pruned {n} message(s) older than {retention}s")
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[weave] gc skipped (non-fatal): {e}"),
                }
            }
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
