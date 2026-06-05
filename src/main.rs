//! weave — Rust-native agent-to-agent session mesh with a native injector.
//!
//! Subcommands:
//!   weave mcp            run the MCP stdio server (register with `claude mcp add`)
//!   weave setup          wire weave into Claude Code (MCP + hooks)
//!   weave uninstall      remove weave's Claude Code wiring
//!   weave send           send a message (CLI; --to-store deposits a cross-store intent)
//!   weave outbox         list pending cross-store intents (Tier-2)
//!   weave pull           pull cross-store intents from pull_from sources into your inbox
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
#[cfg(feature = "sign")]
mod sign;
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
        /// Cross-store (Tier-2): deposit the message as an intent in THIS store's
        /// outbox for a recipient that lives in another store. The recipient pulls
        /// and commits it on its next drain (next-drain latency). This NEVER writes
        /// the recipient's store. When omitted, the message is a normal local send.
        #[arg(long)]
        to_store: Option<String>,
        /// Optional host hint stored on a cross-store intent (advisory; only with
        /// --to-store). Disambiguates the same recipient name across machines.
        #[arg(long)]
        to_host: Option<String>,
    },
    /// List pending cross-store intents in your outbox (Tier-2, read-only).
    Outbox {
        #[arg(long, default_value_t = 200)]
        limit: i64,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Pull cross-store intents from configured `pull_from` sources and commit
    /// them into your inbox now (an explicit one-shot of the hook/watch drain).
    Pull {
        #[arg(long)]
        me: Option<String>,
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
    /// Manage Ed25519 signing keys for signed cross-store identity (Tier-2, only
    /// built with `--features sign`). Generate this session's keypair, print/register
    /// its public key, register a peer's public key, or list registered keys.
    #[cfg(feature = "sign")]
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Claude Code lifecycle hook: session|prompt|stop|notification (reads JSON on stdin).
    Hook { event: String },
}

/// `weave key` subcommands (only compiled with `--features sign`, so the default
/// build's `--help` stays free of a subcommand that would do nothing).
#[cfg(feature = "sign")]
#[derive(Subcommand)]
enum KeyCmd {
    /// Generate this session's Ed25519 keypair (private key stored 0600 under the
    /// config dir) and register + print its public key for `identity`.
    Gen {
        /// identity to register the generated public key under (defaults to
        /// config/$WEAVE_SESSION or basename of cwd)
        #[arg(long)]
        me: Option<String>,
    },
    /// Print this session's public key (and its on-disk private-key path), without
    /// revealing the private key itself.
    Show {
        #[arg(long)]
        me: Option<String>,
    },
    /// Register a PEER's public key so signed intents claiming to be from them can
    /// be verified. `pubkey` is the 64-hex-char Ed25519 public key.
    Add {
        /// the peer's identity (sender name as it appears on their intents)
        identity: String,
        /// the peer's hex-encoded Ed25519 public key
        pubkey: String,
    },
    /// List all registered (identity, public key) pairs.
    List {
        #[arg(long)]
        json: bool,
    },
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

/// Sign a cross-store intent's canonical `(from,to,body)` with this session's
/// configured signing key, returning the hex signature for `outbox.sig`. Returns
/// `""` when no key is configured or the `sign` feature is not built — in which case
/// the intent is unsigned and the receiver falls back to the advisory model (or
/// drops it under `strict_verify`). A signing-key load error is non-fatal: it logs
/// to stderr and sends unsigned rather than blocking the send. The private key is
/// never logged here.
#[cfg(feature = "sign")]
fn sign_intent_if_keyed(from: &str, to: &str, body: &str) -> String {
    match sign::load_signing_key() {
        Ok(Some(key)) => sign::sign_intent(&key, from, to, body),
        Ok(None) => String::new(),
        Err(e) => {
            eprintln!("[weave] could not load signing key (sending unsigned): {e}");
            String::new()
        }
    }
}

/// Without the `sign` feature, intents are always unsigned (empty `sig`).
#[cfg(not(feature = "sign"))]
fn sign_intent_if_keyed(_from: &str, _to: &str, _body: &str) -> String {
    String::new()
}

/// Best-effort Tier-2 pull: commit any cross-store intents addressed to `me` from
/// the configured `pull_from` sources into the local inbox. Like the heartbeat, a
/// failure here must NEVER break the inbox drain — errors are reported to stderr
/// and swallowed. A no-op when `pull_from` is unconfigured (no Tier-2). Pulling
/// under a guessed identity is fine: it only commits intents explicitly addressed
/// to that name (no read-marking, so no inbox is consumed).
fn try_pull(store: &dyn Store, cfg: &Config, me: &str) {
    let allow = cfg.pull_from_sources();
    if allow.is_empty() {
        return;
    }
    match store::pull_from_store(store, me, &allow, cfg.strict_verify()) {
        Ok(p) if p.committed > 0 => {
            eprintln!(
                "[weave] pulled {} cross-store message(s) for '{me}'",
                p.committed
            );
            nudge_pulled(store, cfg, me, &p.committed_sources);
        }
        Ok(_) => {}
        Err(e) => eprintln!("[weave] pull skipped (non-fatal): {e}"),
    }
}

/// Tier-2 consent nudge (decision 5, DEFAULT ON): after a pull commits messages,
/// fire the EXISTING paste-safe content-free [`inject::Nudge::Nudge`] into THIS
/// session's OWN registered pane — never a foreign pane, never the body. This is
/// done **caller-side** (here, in `main`/`mcp`, which already depend on both
/// `store` and `inject`) so `store::pull_from_store` never gains a `store → inject`
/// edge.
///
/// Gating (hard): does nothing unless `inject_pulled()` is on (the single
/// off-switch ⇒ pure queue-only) AND at least one committed source passes
/// `inject_allowed_from` (a non-allow-listed source delivers to the inbox but
/// never injects). If this session's pane is not injectable (`mux=none`) or not
/// alive, the inject silently falls back to queue-only (the committed messages are
/// already safely in the inbox). Best-effort: any failure is logged to stderr and
/// NEVER breaks the drain.
fn nudge_pulled(
    store: &dyn Store,
    cfg: &Config,
    me: &str,
    committed_sources: &[config::StoreSource],
) {
    // Master toggle: false ⇒ pure queue-only, no keystroke at all.
    if !cfg.inject_pulled() {
        return;
    }
    // Per-source gate: a source must be inject-eligible. A non-allow-listed source
    // delivered its message to the inbox but must never trigger a keystroke.
    if !committed_sources
        .iter()
        .any(|src| cfg.inject_allowed_from_source(src))
    {
        return;
    }
    // Inject into THIS session's OWN pane only (owner-only: never a foreign pane).
    let peer = match store.get_peer(me) {
        Ok(Some(p)) => p,
        Ok(None) => return, // no registered pane for me ⇒ queue-only.
        Err(err) => {
            eprintln!("[weave] pull-nudge skipped (non-fatal): {err}");
            return;
        }
    };
    let target = inject::Target::from_peer(&peer);
    // Fail open to queue-only when the pane is not injectable or not alive.
    if !target.injectable() || !inject::target_alive(&target) {
        return;
    }
    // Reuse the EXACT paste-safe path; content-free Nudge::Nudge (the body already
    // landed in the inbox on commit). Failure is non-fatal — the message is safe.
    match inject::inject_mode(&target, "", inject::Nudge::Nudge) {
        Ok(_) => {}
        Err(err) => eprintln!("[weave] pull-nudge inject failed (non-fatal): {err}"),
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
    let extra = cfg.peer_db_sources();
    let remote_count = extra.iter().filter(|s| s.is_remote()).count();
    let views = store::federated_peers(store, &extra)?;
    let total_peers = views.len();
    let online = views.iter().filter(|v| is_alive(&v.peer)).count();
    let (fed_ok, fed_skipped) = store::federation_status(&extra);
    // On the default sqlite build a remote source cannot be opened — surface how
    // many were skipped purely for lack of the libsql feature so the user is told.
    let remote_unsupported = if cfg!(feature = "libsql") {
        0
    } else {
        remote_count
    };
    // Token-FREE per-source token-tier observability: how each remote source's auth
    // token resolved (per-source label-env / shared / none). NEVER prints any token
    // byte or the label↔token pairing — only aggregate COUNTS.
    let tiers = cfg.peer_db_remote_token_tiers();
    let token_per_source = tiers
        .iter()
        .filter(|t| **t == config::PullTokenTier::PerSourceLabel)
        .count();
    let token_shared = tiers
        .iter()
        .filter(|t| **t == config::PullTokenTier::Shared)
        .count();
    let token_none = tiers
        .iter()
        .filter(|t| **t == config::PullTokenTier::None)
        .count();
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
                "federation_remote_stores": remote_count,
                "federation_remote_unsupported": remote_unsupported,
                "federation_remote_token_per_source": token_per_source,
                "federation_remote_token_shared": token_shared,
                "federation_remote_token_none": token_none,
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
            if remote_count > 0 {
                println!("  remote sources: {remote_count} configured");
                println!(
                    "  remote tokens:  {token_per_source} per-source, {token_shared} shared, {token_none} none"
                );
                if remote_unsupported > 0 {
                    println!(
                        "  note: {remote_unsupported} remote source(s) skipped — rebuild weave with --features libsql to use them"
                    );
                }
            }
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
    cfg: &Config,
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
        // Tier-2: pull cross-store intents each tick (best-effort) so a federated
        // message surfaces in the same forward-paging walk below.
        try_pull(store, cfg, me);
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
                .or_else(|| cfg.session.clone())
                // MCP stdio mode has no per-call `--from`/`--me` flag, so without this it
                // left the server identity *unset* and every tool errored with
                // "'from' is required". Fall back to the SAME basename(cwd) identity the
                // CLI's resolve_me() uses (mesh peers are already named by cwd basename),
                // so MCP tools work without a hand-passed `from`. Only the degenerate
                // "unknown" cwd is left unset.
                .or_else(|| {
                    let me = resolve_me(None, None, &cfg);
                    (!me.is_empty() && me != "unknown").then_some(me)
                });
            // Plumb the configured nudge template (if any) into the MCP server so
            // its live-injection nudges honor the same `nudge_template` the CLI
            // uses. `None` ⇒ the server falls back to its built-in default text.
            let nudge_tpl = cfg.nudge_template().map(str::to_owned);
            // Tier-1 federation: pass the validated read-only extra store paths so
            // the MCP peers/sessions/doctor tools aggregate them too.
            let extra_dbs = cfg.peer_db_sources();
            // Tier-2: cross-store delivery sources the MCP inbox drain will pull
            // intents from (DISTINCT from extra_dbs, which is read-only
            // visibility), bundled with the decision-5 consent state so the drain
            // can fire the caller-side nudge into this session's OWN pane.
            let pull = mcp::PullConsent {
                from: cfg.pull_from_sources(),
                inject_pulled: cfg.inject_pulled(),
                allow_inject_from: cfg.allow_inject_from_sources(),
                strict_verify: cfg.strict_verify(),
            };
            mcp::run(store, def, nudge_tpl.as_deref(), extra_dbs, pull)?;
        }

        Cmd::Send {
            from,
            to,
            subject,
            body,
            to_store,
            to_host,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            match to_store {
                // Cross-store (Tier-2): the recipient lives in a FOREIGN store, so
                // we deposit an intent into OUR OWN outbox rather than attempt any
                // foreign write (owner-only-writes). A is the writer of A's outbox;
                // authorization is the receiver's (its pull_from allowlist). No
                // local inbox row, no inject — A cannot reach the recipient's pane.
                Some(store_path) => {
                    if model::is_broadcast(&to) {
                        anyhow::bail!(
                            "cross-store broadcast is not supported; send to a named recipient \
                             (Tier-2 is directed-only)."
                        );
                    }
                    let host = to_host.as_deref().unwrap_or("");
                    // Signed identity (2d): if a local signing key is configured,
                    // sign the canonical (from,to,body,created) so the receiver can
                    // verify `from` is unforgeable. `created` is the enqueue time we
                    // bind into the row; we sign the SAME value the store stamps so
                    // verification matches. Without the `sign` feature this is "".
                    let sig = sign_intent_if_keyed(&from, &to, &body);
                    let id =
                        store.enqueue_intent(&to, host, &from, subject.as_deref(), &body, &sig)?;
                    println!("queued intent #{id} for '{to}' @ {store_path} (delivered on their next drain)");
                }
                None => {
                    let mid = store.send(&from, &to, subject.as_deref(), &body)?;
                    println!("sent #{mid}: {from} -> {to}");
                    try_inject(store, &cfg, &from, &to, &body)?;
                }
            }
        }

        Cmd::Outbox { limit, json } => {
            let intents = store.outbox_all(limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "outbox": intents }))?
                );
            } else if intents.is_empty() {
                println!("outbox: empty (no pending cross-store intents)");
            } else {
                println!("outbox: {} pending intent(s)", intents.len());
                for i in &intents {
                    let subj = i
                        .subject
                        .as_ref()
                        .map(|s| format!(" | {s}"))
                        .unwrap_or_default();
                    let host = if i.to_host.is_empty() {
                        String::new()
                    } else {
                        format!("@{}", i.to_host)
                    };
                    println!(
                        "#{} [{}] {} -> {}{}{}\n  {}",
                        i.id,
                        model::fmt_ts(i.ts),
                        i.from,
                        i.to,
                        host,
                        subj,
                        i.body
                    );
                }
            }
        }

        Cmd::Pull { me } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            let allow = cfg.pull_from_sources();
            let pulled = store::pull_from_store(store, &me, &allow, cfg.strict_verify())?;
            println!(
                "pulled {} message(s) into '{me}' from {} source(s){}",
                pulled.committed,
                allow.len(),
                if pulled.sources_skipped > 0 {
                    format!(" ({} skipped)", pulled.sources_skipped)
                } else {
                    String::new()
                }
            );
            // Tier-2 consent nudge (decision 5, default on) into our OWN pane.
            if pulled.committed > 0 {
                nudge_pulled(store, &cfg, &me, &pulled.committed_sources);
            }
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
            let extra = cfg.peer_db_sources();
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
            let extra = cfg.peer_db_sources();
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

        #[cfg(feature = "sign")]
        Cmd::Key { cmd } => handle_key(store, &cfg, cmd)?,

        Cmd::Hook { event } => handle_hook(store, &cfg, &event)?,
    }
    Ok(())
}

/// `weave key` handler (only built with `--features sign`). Generates/shows this
/// session's keypair and registers/lists public keys in the LOCAL `keys` table. The
/// private key is written 0600 under the config dir and is never printed or logged.
#[cfg(feature = "sign")]
fn handle_key(store: &dyn Store, cfg: &Config, cmd: KeyCmd) -> Result<()> {
    match cmd {
        KeyCmd::Gen { me } => {
            let me = resolve_me(me, None, cfg);
            let pubkey = sign::generate_keypair()?;
            store.register_key(&me, &pubkey)?;
            println!("generated signing key for '{me}'");
            println!(
                "private key: {} (0600, keep secret)",
                sign::key_path().display()
            );
            println!("public key:  {pubkey}");
            println!("share the public key with peers so they can `weave key add {me} {pubkey}`");
        }
        KeyCmd::Show { me } => {
            let me = resolve_me(me, None, cfg);
            match sign::local_public_key()? {
                Some(pk) => {
                    println!("identity:   {me}");
                    println!("public key: {pk}");
                    println!(
                        "private key file: {} (not shown)",
                        sign::key_path().display()
                    );
                }
                None => {
                    println!(
                        "no signing key configured for '{me}' — run `weave key gen` to create one"
                    );
                }
            }
        }
        KeyCmd::Add { identity, pubkey } => {
            // Validate the identity and the hex pubkey before it touches the store.
            store::check_ident("identity", &identity)?;
            sign::check_pubkey(&pubkey)?;
            store.register_key(&identity, &pubkey)?;
            println!("registered public key for '{identity}'");
        }
        KeyCmd::List { json } => {
            let keys = store.list_keys()?;
            if json {
                let arr: Vec<_> = keys
                    .iter()
                    .map(|(i, p)| serde_json::json!({ "identity": i, "pubkey": p }))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "keys": arr }))?
                );
            } else if keys.is_empty() {
                println!("no registered keys");
            } else {
                println!("{} registered key(s):", keys.len());
                for (identity, pubkey) in keys {
                    println!("  {identity}  {pubkey}");
                }
            }
        }
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
            // Tier-2: opportunistically pull cross-store intents into the local
            // inbox BEFORE draining, so a freshly-pulled message is delivered in
            // this same turn. Best-effort: a pull failure never sinks the drain.
            try_pull(store, cfg, &me);
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
