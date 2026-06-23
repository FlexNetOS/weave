//! weave — Rust-native agent-to-agent session mesh with a native injector.
//!
//! Subcommands:
//!   weave mcp            run the MCP stdio server (register with `claude mcp add`)
//!   weave setup          wire weave into a coding-agent host (MCP + hooks);
//!                        --provider <claude|codex|gemini|aider> (default claude);
//!                        --pretooluse also installs the enforcing PreToolUse gate (Claude)
//!   weave uninstall      remove weave's host wiring (--provider <…>, default claude)
//!   weave provider-switch list|current|switch  bridge CC Switch providers into host configs
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
//!   weave hook <event>   Claude Code lifecycle hook: session|prompt|stop|wake|notification|pretooluse
//!   weave harness        dry-run/run autonomous orchestration harnesses (Codex 7-layer)

// The MCP `tools()` registry is a single large `json!([...])` literal; each added
// tool deepens the `serde_json::json!` macro recursion. Raising the crate recursion
// limit (a compile-time attribute, NO dependency) keeps that one literal expanding
// as the tool table grows — the rustc-recommended fix.
#![recursion_limit = "256"]

// Backends statically link their own SQLite and cannot coexist; guard loudly.
#[cfg(all(feature = "sqlite", feature = "libsql"))]
compile_error!(
    "features `sqlite` and `libsql` are mutually exclusive (both statically link SQLite). \
     Build the libSQL backend with `--no-default-features --features libsql`."
);
#[cfg(not(any(feature = "sqlite", feature = "libsql")))]
compile_error!("no storage backend selected: enable `sqlite` (default) or `libsql`.");

mod backup;
mod git;
mod harness;
#[cfg(feature = "sqlite")]
mod provider_switch;
mod session;
mod setup;
#[cfg(feature = "surfaces")]
mod slack;
#[cfg(feature = "surfaces")]
mod telegram;
#[cfg(feature = "sign")]
use weave_core::sign;
#[cfg(test)]
mod testenv;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use weave_core::config::{Config, HookEvent};
use weave_core::memory::{self, MemoryScope};
use weave_core::store::{is_alive, Store};
use weave_core::{config, model, store};
use weave_inject::{self as inject, inject_text, Injector};
use weave_mcp::{self as mcp};

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

/// Parse a mux-preference config string into a real `Mux` variant, filtering
/// out the catch-all `Mux::None` so an unrecognized value falls back to
/// auto-detection rather than forcing "no mux".
fn parse_mux_preference(cfg: &Config) -> Option<weave_inject::Mux> {
    cfg.mux_preference().and_then(|s| {
        let m = inject::Mux::parse(s);
        if m == inject::Mux::None {
            None
        } else {
            Some(m)
        }
    })
}

/// Production injector implementation passed to `weave_mcp::serve`.
struct RealInjector {
    preferred_mux: Option<weave_inject::Mux>,
}

impl Injector for RealInjector {
    fn detect_target(&self) -> weave_inject::Target {
        weave_inject::detect_target_with_preference(self.preferred_mux)
    }
    fn target_alive(&self, target: &weave_inject::Target) -> bool {
        weave_inject::target_alive(target)
    }
    fn inject_mode(
        &self,
        target: &weave_inject::Target,
        body: &str,
        mode: weave_inject::Nudge,
    ) -> anyhow::Result<bool> {
        weave_inject::inject_mode(target, body, mode)
    }
    fn capability(&self, target: &weave_inject::Target) -> weave_inject::Capability {
        weave_inject::capability(target)
    }
    fn have(&self, name: &str) -> bool {
        weave_inject::have(name)
    }
    fn id_valid(&self, mux: weave_inject::Mux, id: &str) -> bool {
        weave_inject::id_valid(mux, id)
    }
    fn git_tags(&self, cwd: &std::path::Path) -> anyhow::Result<weave_core::model::WorktreeTags> {
        Ok(git::capture_worktree_tags(cwd))
    }
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
    /// Wire weave into a coding-agent host (register MCP server + lifecycle
    /// hooks). `--provider` selects the host (default `claude`); each provider
    /// writes into its own config file with the same never-clobber-foreign,
    /// idempotent, read-back-verified merge. Optionally also install a git
    /// pre-commit hook that guards against committing files reserved by peers.
    Setup {
        /// Which coding-agent host to wire (default: claude).
        #[arg(long, value_enum, default_value_t = SetupProvider::Claude)]
        provider: SetupProvider,
        /// Also install the git pre-commit hook in the current repo.
        #[arg(long)]
        git_hooks: bool,
        /// WL-055: ALSO install the enforcing PreToolUse approval gate (Claude only;
        /// matcher `Bash|Edit|Write`). Default OFF so it never surprise-blocks. With
        /// it on, set `pretooluse_approver` (or WEAVE_PRETOOLUSE_APPROVER) — without an
        /// approver every dangerous tool is DENIED (deny-by-default).
        #[arg(long)]
        pretooluse: bool,
        /// WL-057: pin the exact binary path written into the host config (MCP +
        /// hook commands). Use this when the running `weave` is an ephemeral build
        /// or worktree binary (`…/target/{debug,release}/weave`, `…/.worktrees/…`)
        /// whose path would dangle once that build is gone. The override has highest
        /// precedence; if omitted, weave persists the running binary when it is
        /// stable, else falls back to `~/.cargo/bin/weave` or a `weave` on `$PATH`.
        #[arg(long)]
        exe: Option<String>,
    },
    /// Remove weave's host wiring (`--provider` selects the host; default claude).
    Uninstall {
        /// Which coding-agent host to unwire (default: claude).
        #[arg(long, value_enum, default_value_t = SetupProvider::Claude)]
        provider: SetupProvider,
    },
    /// Bridge CC Switch providers into Claude/Codex/Gemini live configs while preserving weave hooks.
    #[cfg(feature = "sqlite")]
    ProviderSwitch {
        #[command(subcommand)]
        cmd: ProviderSwitchCmd,
    },
    /// Autonomous orchestration harnesses (Codex seven-layer ide-merge-ide).
    Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    },
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
        /// Skip automatic memory context prefixing for this message.
        #[arg(long)]
        no_memory: bool,
        /// Idempotency key: if a message with this key already exists, the existing
        /// message id is returned instead of creating a new row.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Message priority: low, normal, high, urgent (default normal).
        #[arg(long)]
        priority: Option<String>,
        /// WL-037: id of a prior message of YOURS this one replaces. The
        /// predecessor is marked superseded and hidden from the recipient's unread
        /// inbox (kept, flagged, in history). You may only supersede your own
        /// messages.
        #[arg(long)]
        supersedes: Option<i64>,
        /// WL-038: ephemeral TTL in seconds (1..=86400). The message is
        /// auto-deleted after this many seconds (delete-on-sweep) and excluded
        /// from every read surface; omit for a permanent message.
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Fire-and-forget notification to a peer (no reply expected). Persists +
    /// pushes a live nudge if injectable, then prints the HONEST delivery verdict
    /// (transport_delivered / queued_next_turn / recipient_not_injectable).
    /// Point-to-point only; use `weave send` for broadcast.
    Notify {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Idempotency key: if a message with this key already exists, the existing
        /// message id is returned instead of creating a new row.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Message priority: low, normal, high, urgent (default normal).
        #[arg(long)]
        priority: Option<String>,
        /// WL-039: idle-notification dedup. Mark this ping as an idle "still
        /// waiting" notification and auto-supersede this sender's prior *unread*
        /// idle pings to the same recipient, so they collapse to just the latest.
        /// Never touches a real `weave send` message or another sender's pings.
        #[arg(long)]
        dedup_idle: bool,
    },
    /// Broadcast a notification to all online peers in your circle. Fan-out:
    /// one message per online peer, plus a live nudge for each injectable peer.
    /// Returns an aggregated delivery verdict per peer.
    BroadcastNotify {
        #[arg(long)]
        from: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Scope to this circle; omit for your own configured circle.
        #[arg(long)]
        circle: Option<String>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Message priority: low, normal, high, urgent (default normal).
        #[arg(long)]
        priority: Option<String>,
    },
    /// Broadcast an ask to all online peers in your circle. Fan-out via
    /// ask-many: one tracked question per online peer. Returns a parent id
    /// and per-child delivery verdicts.
    BroadcastAsk {
        #[arg(long)]
        from: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Scope to this circle; omit for your own configured circle.
        #[arg(long)]
        circle: Option<String>,
        /// Optional message id this broadcast ask replies to (threads the conversation).
        #[arg(long = "reply-to")]
        reply_to: Option<i64>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
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
        /// Skip automatic memory context prefixing for this reply.
        #[arg(long)]
        no_memory: bool,
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
        /// Generate or retrieve a cached LLM summary of the thread.
        #[arg(long)]
        summarize: bool,
        /// Force a fresh LLM summary even if a cached one exists.
        #[arg(long)]
        refresh: bool,
    },
    /// Summarize arbitrary text via the configured LLM endpoint.
    Summarize {
        /// Text to summarize (inline). If omitted, reads from stdin.
        #[arg(long, allow_hyphen_values = true)]
        text: Option<String>,
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
    /// Show the DELIVERY/transport trace for a message (queued -> injected /
    /// inject_failed / not_injectable -> drained). The transport-side complement to
    /// `weave receipts` (read receipts). Read-only and metadata-only.
    Delivery {
        /// id of the message to trace
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
    /// Full-text search over messages (FTS5 on sqlite, LIKE fallback on libsql).
    Search {
        /// FTS5 query string (sqlite) or substring (libsql)
        #[arg(long, allow_hyphen_values = true)]
        query: String,
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
        /// only show peers in this circle (default: your own circle; an
        /// orchestrator defaults to mesh-wide)
        #[arg(long)]
        circle: Option<String>,
        /// show peers in every circle (mesh-wide)
        #[arg(long)]
        all_circles: bool,
    },
    /// List known sessions with unread counts. With `--watch`, render a read-only
    /// presence dashboard (grouped by repo then branch) that re-renders every
    /// `--interval` seconds until interrupted (Ctrl-C); `--iterations N` renders
    /// exactly N frames then exits (bounded test mode).
    Sessions {
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// re-render a read-only presence dashboard until interrupted (Ctrl-C)
        #[arg(long)]
        watch: bool,
        /// dashboard poll interval in seconds (clamped to a sane range)
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// render exactly N frames then exit (0 ⇒ loop forever; bounded test mode)
        #[arg(long, default_value_t = 0)]
        iterations: u64,
        /// only show sessions whose repo tag equals this value
        #[arg(long)]
        repo: Option<String>,
        /// only show sessions whose branch tag equals this value
        #[arg(long)]
        branch: Option<String>,
        /// only show sessions in this circle (default: your own circle; an
        /// orchestrator defaults to mesh-wide)
        #[arg(long)]
        circle: Option<String>,
        /// show sessions in every circle (mesh-wide)
        #[arg(long)]
        all_circles: bool,
    },
    /// Scan, identify, and tag running sessions: refresh your own row's git tags,
    /// then list every (federated) peer joined with liveness and its
    /// repo/branch/worktree tags. Optional `--repo`/`--branch` narrow the set.
    Scan {
        /// only show peers whose repo tag equals this value
        #[arg(long)]
        repo: Option<String>,
        /// only show peers whose branch tag equals this value
        #[arg(long)]
        branch: Option<String>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// only show peers in this circle (default: your own circle; an
        /// orchestrator defaults to mesh-wide)
        #[arg(long)]
        circle: Option<String>,
        /// show peers in every circle (mesh-wide)
        #[arg(long)]
        all_circles: bool,
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
        /// Existing birth certificate for re-registering a previously-registered peer.
        #[arg(long)]
        cert: Option<String>,
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
    /// Spawn a NEW agent/command into a fresh mux pane (or a new window with
    /// --window) and register it as a peer (WL-047). Argv-only — no shell is ever
    /// invoked; the child command after --cmd is passed as discrete argv elements.
    /// An unguessable birth certificate is threaded into the child's env so it
    /// self-registers a tamper-proof identity on its first `weave hook session`.
    Spawn {
        /// The spawned agent's session identity (the peer row key). Must not exist.
        #[arg(long)]
        name: String,
        /// The child command as argv: everything after --cmd is the program + args.
        #[arg(long = "cmd", allow_hyphen_values = true, num_args = 1.., required = true)]
        cmd: Vec<String>,
        /// Working directory to launch in (default: the current directory).
        #[arg(long)]
        cwd: Option<String>,
        /// Override the multiplexer (tmux|zellij|kitty|wezterm|screen).
        #[arg(long)]
        mux: Option<String>,
        /// Open a new window/tab instead of a split pane.
        #[arg(long)]
        window: bool,
    },
    /// Kill a registered peer's pane/session (WL-047). zellij/screen kills are
    /// coarse (session-level); iterm2/none are unsupported.
    Kill {
        /// The registered peer to kill.
        #[arg(long)]
        name: String,
    },
    /// Open a correlation-tracked request to a peer (P1 ask/answer/ack). Returns a
    /// correlation id immediately (non-blocking); the question is delivered like a
    /// normal message and the honest delivery verdict is printed. Point-to-point.
    Ask {
        #[arg(long)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        /// structured kind: free_text (default), choice, tool_permission
        #[arg(long)]
        kind: Option<String>,
        /// kind-specific payload: newline-separated choices, or tool_name\ntool_args
        #[arg(long, allow_hyphen_values = true)]
        options: Option<String>,
        /// prior correlation id this ask chains/closes
        #[arg(long = "reply-to")]
        reply_to: Option<String>,
        #[arg(long)]
        from: Option<String>,
        /// Skip automatic memory context prefixing for this ask.
        #[arg(long)]
        no_memory: bool,
    },
    /// Answer a tracked ask, replying back to whoever opened it (open -> answered).
    /// Reference the thread by --id (correlation id) OR --in-reply-to (a message id).
    Answer {
        /// the ask's correlation id
        #[arg(long)]
        id: Option<String>,
        /// alternatively, a message id belonging to the ask
        #[arg(long = "in-reply-to")]
        in_reply_to: Option<i64>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        #[arg(long)]
        from: Option<String>,
    },
    /// Close a tracked ask (-> acked). Pure state transition; an optional --message
    /// is recorded as the closing note (not delivered).
    Ack {
        /// the ask's correlation id
        #[arg(long)]
        id: String,
        #[arg(long, allow_hyphen_values = true)]
        message: Option<String>,
        #[arg(long)]
        from: Option<String>,
    },
    /// List tracked asks where you are the asker, askee, or either.
    Asks {
        #[arg(long)]
        me: Option<String>,
        /// asker | askee | any (default any)
        #[arg(long, default_value = "any")]
        role: String,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Fetch a single tracked ask by correlation id.
    AskGet {
        #[arg(long)]
        id: String,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Fan ONE question to N peers (P2 ask-many). Opens a parent group + one tracked
    /// child ask per --to peer, fires each child's live nudge, and prints the parent
    /// id + per-child verdicts immediately (non-blocking, best-effort).
    AskMany {
        /// a target peer (repeat --to for each peer; 1..=64, de-duplicated)
        #[arg(long)]
        to: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long)]
        from: Option<String>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Aggregate an ask-many group at read time: per-child state/answer, pending peers,
    /// rollup counts, and the derived state (complete|partial|pending).
    AskManyResult {
        /// the ask-many parent id
        #[arg(long = "parent-id")]
        parent_id: String,
        /// optional age (seconds): a still-pending group older than this reads 'partial'
        #[arg(long)]
        age: Option<i64>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Durable poll-only job board (P3): create/list/show/status/claim/update/
    /// result/cancel. No autonomous dispatch/runner — nothing nudges or spawns.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Per-circle orchestrator role (P4): claim the single coordinator slot for a
    /// circle, or report who currently holds it.
    Orchestrator {
        #[command(subcommand)]
        cmd: OrchestratorCmd,
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
    /// Inspect signed-identity audit logs (Tier-2, only built with `--features sign`).
    /// Currently surfaces the observed-revocation log: enforced rejections (a signed
    /// intent was dropped because its key is revoked) and operator `declared` revokes.
    #[cfg(feature = "sign")]
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Set this session's free-form, self-reported task description (P5 rich
    /// presence). Surfaces compactly in `peers`/`sessions`/`scan` and ages out after
    /// the description TTL (900s). Self-only: targets your OWN peer row.
    Describe {
        /// description text (a one-line task summary; control-stripped + capped)
        #[arg(allow_hyphen_values = true)]
        text: String,
        #[arg(long)]
        me: Option<String>,
    },
    /// Explicitly set this session's turn-state (P5). Normally hook-auto via
    /// `weave hook session|prompt|stop|wake`; this is the manual override.
    /// Self-only. Valid states: pending_first_turn|working|awaiting_input|idle.
    Status {
        /// turn-state label (pending_first_turn|working|awaiting_input|idle)
        state: String,
        #[arg(long)]
        me: Option<String>,
    },
    /// Set or get a peer's contact policy (WL-032). open (default), auto,
    /// contacts_only, block_all. Omit --policy to read the current value.
    PeerPolicy {
        #[arg(long)]
        name: String,
        #[arg(long)]
        policy: Option<String>,
    },
    /// Schedule a future message delivery (one-shot or recurring).
    Schedule {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// One-shot: absolute UNIX timestamp.
        #[arg(long)]
        at: Option<i64>,
        /// Recurring: cron preset or expression.
        #[arg(long)]
        every: Option<String>,
    },
    /// List your scheduled messages.
    Schedules {
        #[arg(long)]
        me: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a scheduled message.
    CancelSchedule {
        #[arg(long)]
        id: i64,
    },
    /// Execute any due scheduled messages now (explicit tick).
    Tick {
        #[arg(long)]
        me: Option<String>,
        /// also evaluate schedules for other senders (admin/debug)
        #[arg(long)]
        all: bool,
    },
    /// Claude Code lifecycle hook: session|prompt|stop|wake|notification|pretooluse
    /// (reads JSON on stdin). `pretooluse` is the WL-055 enforcing approval gate: it
    /// emits a `permissionDecision` (allow|deny|defer) on stdout — deny-by-default for
    /// dangerous tools unless an approver answers `approve` within the internal timeout.
    Hook {
        event: String,
        /// Enable blocking wake on stop: drain inbox, mark read, and emit a
        /// structured JSON block if unread messages exist. Overrides the default
        /// peek-only stop behaviour. Also enabled by WEAVE_STOP_WAKE=1.
        #[arg(long)]
        wake: bool,
    },
    /// Filesystem-backed scoped memory (global, project, persona, orchestrator).
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Presence daemon (v0.2): start, stop, status, or run the heartbeat loop.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// PR review queue (WL-020): track, list, and mark GitHub PRs as reviewed.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Permission approval status (WL-021): check verdict of ToolPermission asks.
    Permission {
        #[command(subcommand)]
        cmd: PermissionCmd,
    },
    /// Reservation leases (WL-024): lightweight advisory file locks between agents.
    Lease {
        #[command(subcommand)]
        cmd: LeaseCmd,
    },
    /// HTTP MCP server (WL-022): localhost-only JSON-RPC endpoint for remote agents.
    Serve {
        /// Port to listen on (default 8787).
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Address to bind (default 127.0.0.1 — loopback only). WL-056 / ADR-0005:
        /// expose cross-machine push by binding a routable address (e.g. 0.0.0.0 or a
        /// Tailscale address); a non-loopback bind REQUIRES a bearer token (fail-closed).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Bearer token for authentication. If omitted, a random token is generated
        /// and printed to stderr.
        #[arg(long)]
        token: Option<String>,
        /// Enable dangerous/mutating tools (disabled by default for safety).
        #[arg(long)]
        dangerous: bool,
    },
    /// Build a communication graph from the message store and run graph analytics
    /// (connected components, centrality). Powered by FrankenNetworkX.
    Graph {
        #[arg(long)]
        me: Option<String>,
        /// Scope to this circle; omit for mesh-wide.
        #[arg(long)]
        circle: Option<String>,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// WL-048: serve the human web dashboard (sessions, messages, jobs, leases,
    /// schedules) over HTTP + SSE. Localhost-bound, bearer-gated. Read-only by default;
    /// `--write` (WL-052a) enables a `POST /api` action surface that routes through the
    /// SAME handler as MCP/CLI.
    #[cfg(feature = "surfaces")]
    Dashboard {
        /// Port to listen on (default 8788).
        #[arg(long, default_value_t = 8788)]
        port: u16,
        /// Address to bind (default 127.0.0.1 — loopback only). A non-loopback bind
        /// REQUIRES a bearer token (fail-closed), same posture as `weave serve`.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Bearer token. If omitted, a random token is generated and printed to stderr.
        #[arg(long)]
        token: Option<String>,
        /// WL-052a: enable the bearer-gated `POST /api` write surface (mutating ops).
        /// Off by default — the dashboard is read-only unless this is set.
        #[arg(long)]
        write: bool,
    },
    /// WL-056 / ADR-0005: PUSH a message to a recipient on ANOTHER machine. The
    /// A-initiated dual of a Tier-2 pull: sign the canonical (from,to,body) if keyed,
    /// then POST the Intent to B's bearer-gated `weave serve --write` endpoint, where
    /// B commits it into its OWN inbox and lights its OWN pane WITHOUT polling.
    /// `--host` is EXPLICIT-ONLY (never auto-resolved from message content — SSRF
    /// avoidance). Body from `--body` or stdin.
    #[cfg(feature = "surfaces")]
    Push {
        /// Recipient session name on the remote machine.
        #[arg(long)]
        to: String,
        /// Remote endpoint, `host:port` (or a full `http://host:port`). EXPLICIT-ONLY.
        #[arg(long)]
        host: String,
        /// Bearer token for B's endpoint. Falls back to $WEAVE_PUSH_TOKEN, then config.
        #[arg(long)]
        token: Option<String>,
        /// Message body. If omitted, the body is read from stdin.
        #[arg(long)]
        body: Option<String>,
        /// Optional subject line.
        #[arg(long)]
        subject: Option<String>,
        /// Optional host hint disambiguating the recipient name (advisory).
        #[arg(long = "to-host")]
        to_host: Option<String>,
        /// Optional message priority: low, normal, high, urgent.
        #[arg(long)]
        priority: Option<String>,
        /// Optional ephemeral TTL in seconds (1..=86400).
        #[arg(long)]
        ttl: Option<i64>,
        /// Optional idempotency key (a retried push with the same key never double-delivers).
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
        /// Your session name (overrides the resolved identity).
        #[arg(long)]
        me: Option<String>,
    },
    /// WL-048: run the Telegram bridge (poll-only): relay between a Telegram chat
    /// and the weave mesh. Token from config/`WEAVE_TELEGRAM_TOKEN`.
    #[cfg(feature = "surfaces")]
    Telegram,
    /// WL-048: run the Slack bridge (poll-only): relay between a Slack channel and
    /// the weave mesh. Token from config/`WEAVE_SLACK_TOKEN`.
    #[cfg(feature = "surfaces")]
    Slack,
    /// WL-034: export a mailbox to a single self-contained, offline-openable HTML
    /// file with client-side search (no external assets, no CDN). Exports the
    /// caller's mailbox (sender / recipient / broadcast scope) via `history`.
    Export {
        /// Output path for the `.html` bundle.
        #[arg(long)]
        out: PathBuf,
        /// Identity whose mailbox to export. Falls back to `resolve_me()`
        /// (`$WEAVE_SESSION` > basename(cwd)) when omitted.
        #[arg(long = "for")]
        for_id: Option<String>,
        /// Max messages to include (clamped to the store's MAX_LIMIT).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// WL-035: write a portable, dependency-free archive of this mailbox — a
    /// consistent SQLite snapshot (`VACUUM INTO`) plus `config.toml` and weave's
    /// installed Claude `settings.json` hooks. Read-back-verified before success.
    Backup {
        /// Output path for the archive (uncompressed USTAR tar).
        #[arg(long)]
        out: PathBuf,
        /// Overwrite `--out` if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// WL-035: restore a mailbox from a `weave backup` archive. Restores the DB and
    /// config by default; `settings.json` is overwritten only with `--force` (a
    /// `.bak` of the current file is written first). Traversal-guarded extraction.
    Restore {
        /// Path to the archive produced by `weave backup`.
        #[arg(long = "in")]
        in_path: PathBuf,
        /// Overwrite existing DB/config/settings (writes a `.bak` of settings first).
        #[arg(long)]
        force: bool,
    },
    /// WL-040: portable, canonical, schema-versioned JSON interchange for resuming a
    /// session (its messages + mesh memory) across distinct weave instances — casr
    /// parity. Logical (not byte-exact like `backup`) and presentation-free (not HTML
    /// like `export`). Idempotent re-import.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// WL-049 / ADR-0002: governed stealth web access via obscura (deny-by-default).
    /// `weave web <op> [--url …] [--arg k=v …]` runs a single browser op through the
    /// same policy/lease/job gate as the `weave_web` MCP tool. `weave web --list`
    /// enumerates the ops; `weave web --stop` reaps the cached obscura child.
    #[cfg(feature = "obscura")]
    Web {
        /// The browser op to run (e.g. `navigate`, `snapshot`, `extract`). Omit with
        /// `--list`/`--stop`.
        op: Option<String>,
        /// URL for a nav-class op (`--url https://example.com`). SSRF-guarded.
        #[arg(long)]
        url: Option<String>,
        /// Extra op argument(s) as `key=value` (repeatable). Values are forwarded
        /// opaquely to obscura as a JSON string field.
        #[arg(long = "arg", value_name = "KEY=VALUE")]
        args: Vec<String>,
        /// List the available web ops and exit (no spawn).
        #[arg(long)]
        list: bool,
        /// Stop and reap the cached obscura child, then exit.
        #[arg(long)]
        stop: bool,
        /// Optional: reserve a per-host lease for N seconds (rate / mutual-exclusion).
        #[arg(long)]
        lease_ttl: Option<i64>,
        /// Optional: record a durable job auditing this web op.
        #[arg(long)]
        audit: bool,
    },
}

/// `weave daemon` subcommands (v0.2).  The optional presence daemon writes
/// periodic heartbeats to the `presence` table so peers show live status.
/// OFF by default; degrades transparently to the TTL heuristic when stopped.
#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon in the background.  It writes a heartbeat every 15 s
    /// and evicts stale rows every 60 s.  Idempotent: a second start is a no-op
    /// if the daemon is already running.
    Start,
    /// Stop the daemon.  Sends SIGTERM to the recorded PID and cleans up.
    Stop,
    /// Show daemon status: running (PID) or stopped.
    Status,
    /// Internal: run the daemon loop (called by Start after spawn).
    /// Not intended for direct use.
    Run {
        #[arg(long)]
        me: Option<String>,
    },
}

/// `weave session` subcommands — WL-040 canonical JSON interchange.
#[derive(Subcommand)]
enum SessionCmd {
    /// Export an identity's messages + mesh memory to a canonical JSON file.
    Export {
        /// Output path for the `.json` interchange document.
        #[arg(long)]
        out: PathBuf,
        /// Identity whose session to export. Falls back to `resolve_me()`
        /// (`$WEAVE_SESSION` > basename(cwd)) when omitted.
        #[arg(long = "for")]
        for_id: Option<String>,
        /// Max messages/asks to include (clamped to the store's MAX_LIMIT).
        #[arg(long)]
        limit: Option<usize>,
        /// Overwrite `--out` if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Import a canonical JSON file into THIS instance (idempotent re-import).
    Import {
        /// Path to a document produced by `weave session export`.
        #[arg(long = "in")]
        in_path: PathBuf,
        /// Identity to resume the session under. Occurrences of the source
        /// identity are remapped to this; third-party names are preserved.
        /// Falls back to `resolve_me()` when omitted.
        #[arg(long = "as")]
        as_id: Option<String>,
        /// Report counts without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

/// `weave memory` subcommands — filesystem-backed scoped memory.
#[derive(Subcommand)]
enum MemoryCmd {
    /// Write a memory entry.
    Write {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
        #[arg(long, allow_hyphen_values = true)]
        title: String,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    /// Read a memory entry.
    Read {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
    },
    /// Search memory entries.
    Search {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// List memory entries in a scope.
    List {
        #[arg(long)]
        scope: String,
    },
    /// Delete a memory entry.
    Delete {
        #[arg(long)]
        scope: String,
        #[arg(long)]
        key: String,
    },
    /// Show available scopes and their resolved paths.
    Scopes,
}

/// `weave review` subcommands (WL-020) — PR review queue.
#[derive(Subcommand)]
enum ReviewCmd {
    /// List review items.
    Queue {
        #[arg(long, default_value = "all")]
        filter: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Add a PR to the review queue.
    Add {
        #[arg(long)]
        pr_url: String,
        #[arg(long, allow_hyphen_values = true)]
        title: Option<String>,
        #[arg(long)]
        author: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Mark a review item as reviewed.
    Mark {
        #[arg(long)]
        id: String,
    },
    /// Remove a review item.
    Remove {
        #[arg(long)]
        id: String,
    },
}

/// `weave permission` subcommands (WL-021) — ToolPermission ask verdicts.
#[derive(Subcommand)]
enum PermissionCmd {
    /// Check the permission status of a ToolPermission ask.
    Status {
        #[arg(long)]
        id: String,
        /// timeout in seconds (default 300)
        #[arg(long)]
        timeout: Option<i64>,
    },
    /// List ToolPermission asks you created.
    List {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

/// `weave lease` subcommands (WL-024) — advisory file reservations.
#[derive(Subcommand)]
enum LeaseCmd {
    /// Reserve a lease on a resource. Succeeds only if no active lease exists.
    Reserve {
        /// Resource identifier (path, glob, or freeform tag).
        #[arg(long)]
        resource: String,
        /// TTL in seconds (1..86400).
        #[arg(long)]
        ttl: i64,
        /// Optional note.
        #[arg(long, allow_hyphen_values = true)]
        note: Option<String>,
    },
    /// Release a lease you hold.
    Release {
        /// Resource identifier.
        #[arg(long)]
        resource: String,
    },
    /// List active (non-expired) leases.
    List {
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Remove all expired leases. Returns the count swept.
    Sweep,
    /// Check staged git files against active leases. Exits non-zero if any
    /// staged file conflicts with a lease held by another peer.
    Guard,
}

/// `weave audit` subcommands (only compiled with `--features sign`). Read-only,
/// secret-free views over the local audit logs.
#[cfg(feature = "sign")]
#[derive(Subcommand)]
enum AuditCmd {
    /// List recorded revocation events, most-recent-first. `enforced` rows are R1
    /// rejections actually observed; `declared` rows record an operator running
    /// `weave key revoke`. Secret-free: fingerprints + public identities/labels only.
    Revocations {
        #[arg(long)]
        json: bool,
        /// maximum rows to show (clamped to a sane cap)
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
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
    /// Print this session's public key, its FINGERPRINT, and its on-disk
    /// private-key path, without revealing the private key itself.
    Show {
        #[arg(long)]
        me: Option<String>,
    },
    /// Print this session's public-key FINGERPRINT (`SHA256:<16-hex>`) — the short,
    /// stable, secret-free hash peers add to their `WEAVE_TRUST`. Add `--json` for a
    /// machine-readable form (identity, pubkey, fingerprint).
    Fingerprint {
        #[arg(long)]
        me: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Rotate this session's signing key: ARCHIVE the existing private key (0600
    /// backup), generate a NEW keypair, register + print BOTH fingerprints, and
    /// explain the config-based overlap (trust BOTH in WEAVE_TRUST; keep the old
    /// pubkey registered via `weave key add`) so in-flight signed messages from the
    /// old key still verify during the window.
    Rotate {
        #[arg(long)]
        me: Option<String>,
    },
    /// Mark a fingerprint REVOKED: print the value to add to WEAVE_REVOKED (or the
    /// `revoked = [...]` config line). A signature verifying against a revoked key is
    /// rejected unconditionally. `fp` is a `SHA256:<full-64-hex>` fingerprint or a
    /// full pubkey hex (single positional argument; no shell involved).
    Revoke {
        /// the fingerprint (or full pubkey hex) to revoke
        fp: String,
    },
    /// Register a PEER's public key so signed intents claiming to be from them can
    /// be verified. APPENDS to the registry: a peer may have MULTIPLE keys
    /// registered at once (rotation overlap — old + new both verify during a
    /// window). Re-adding the same key is a no-op. `pubkey` is the 64-hex-char
    /// Ed25519 public key.
    Add {
        /// the peer's identity (sender name as it appears on their intents)
        identity: String,
        /// the peer's hex-encoded Ed25519 public key
        pubkey: String,
    },
    /// Remove (prune) a registered key from a peer's identity — e.g. retiring an
    /// OLD key after a rotation window has closed. The key may be given as a full
    /// hex pubkey or as a `SHA256:<full-64-hex>` fingerprint (resolved against the
    /// registered set). Single positional `key`; no shell involved.
    Remove {
        /// the peer's identity
        identity: String,
        /// the key to remove: a full hex pubkey OR a SHA256:<full-64-hex> fingerprint
        key: String,
    },
    /// List all registered (identity, public key) pairs. With multi-key
    /// registration an identity may appear several times; each row shows its
    /// fingerprint and a [trusted]/[REVOKED] tag where applicable.
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

/// `weave orchestrator` subcommands (P4). The per-circle coordinator slot is a
/// pure DB role bit; `claim` promotes the caller (refused if a LIVE holder
/// exists unless `--force` steals it), `status` reports the live holder.
#[derive(Subcommand)]
enum OrchestratorCmd {
    /// Claim the orchestrator role for a circle. Refused if a DIFFERENT live
    /// orchestrator already holds it, unless `--force` steals the role (a
    /// non-destructive role-bit flip; the demoted peer can re-claim).
    Claim {
        /// circle to claim (defaults to your own circle)
        #[arg(long)]
        circle: Option<String>,
        /// steal the role even from a live orchestrator
        #[arg(long)]
        force: bool,
        /// claim under this identity (defaults to config/$WEAVE_SESSION/cwd)
        #[arg(long)]
        from: Option<String>,
    },
    /// Report the live orchestrator of a circle (or that none is present).
    Status {
        /// circle to query (defaults to your own circle)
        #[arg(long)]
        circle: Option<String>,
    },
}

/// `weave job` subcommands — the P3 poll-only board: create/list/show/status/claim/
/// update/result/cancel. NO autonomous dispatch/runner (that is P10/P11): nothing
/// nudges or spawns. attempt_id fencing + the state machine are enforced in the
/// store, so both CLI and MCP inherit them.
#[derive(Subcommand)]
enum JobCmd {
    /// Create a durable board job (state 'queued') and print its minted job_id.
    Create {
        #[arg(long)]
        title: String,
        #[arg(long, allow_hyphen_values = true)]
        desc: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        circle: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        prompt: Option<String>,
        /// optional deadline (epoch seconds)
        #[arg(long)]
        deadline: Option<i64>,
        /// your session name (creator); defaults to config/$WEAVE_SESSION/cwd
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List board jobs filtered by state/owner/creator/assignee/circle.
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        creator: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        circle: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// Show a single job's status by id.
    Show {
        job_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Show a single job's status by id (alias of show).
    Status {
        job_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Claim a job to work it: mints an attempt_id, sets you as assignee, -> running.
    Claim {
        job_id: String,
        /// the assignee (defaults to you)
        #[arg(long = "as")]
        as_who: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update a job's lifecycle/result. Pass --attempt to fence a claimed job.
    Update {
        job_id: String,
        #[arg(long)]
        attempt: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long = "state-reason", allow_hyphen_values = true)]
        state_reason: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        phase: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        note: Option<String>,
        #[arg(long = "result-summary", allow_hyphen_values = true)]
        result_summary: Option<String>,
        /// result payload as a JSON string
        #[arg(long, allow_hyphen_values = true)]
        result: Option<String>,
        /// error payload as a JSON string
        #[arg(long, allow_hyphen_values = true)]
        error: Option<String>,
        /// artifacts payload as a JSON string
        #[arg(long, allow_hyphen_values = true)]
        artifacts: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read a job's result (terminal payload, or 'not_ready').
    Result {
        job_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Cooperatively cancel a job (never a hard delete).
    Cancel {
        job_id: String,
        #[arg(long, allow_hyphen_values = true)]
        reason: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[cfg(feature = "sqlite")]
#[derive(Subcommand)]
enum ProviderSwitchCmd {
    /// List providers from the CC Switch SQLite store.
    List {
        /// App namespace to list.
        #[arg(long, value_enum)]
        app: provider_switch::ProviderSwitchApp,
        /// Override CC Switch DB path (default: ~/.cc-switch/cc-switch.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Print the current provider from the CC Switch SQLite store.
    Current {
        /// App namespace to inspect.
        #[arg(long, value_enum)]
        app: provider_switch::ProviderSwitchApp,
        /// Override CC Switch DB path (default: ~/.cc-switch/cc-switch.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Switch to a CC Switch provider and apply its live host config.
    Switch {
        /// App namespace to switch.
        #[arg(long, value_enum)]
        app: provider_switch::ProviderSwitchApp,
        /// Provider id from CC Switch's providers table.
        provider_id: String,
        /// Override CC Switch DB path (default: ~/.cc-switch/cc-switch.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Validate and show the target without writing host config or DB state.
        #[arg(long)]
        dry_run: bool,
    },
    /// List models discovered from CC Switch providers and local Ollama.
    Models {
        /// App namespace to inspect.
        #[arg(long, value_enum)]
        app: provider_switch::ProviderSwitchApp,
        /// Override CC Switch DB path (default: ~/.cc-switch/cc-switch.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Skip probing local Ollama (`OLLAMA_HOST` or localhost:11434).
        #[arg(long)]
        no_ollama: bool,
    },
    /// Change a provider's selected model and refresh live config if it is current.
    SwitchModel {
        /// App namespace to update.
        #[arg(long, value_enum)]
        app: provider_switch::ProviderSwitchApp,
        /// Provider id from CC Switch's providers table.
        provider_id: String,
        /// Model id/name to select.
        model: String,
        /// Override CC Switch DB path (default: ~/.cc-switch/cc-switch.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Validate and show the target without writing host config or DB state.
        #[arg(long)]
        dry_run: bool,
    },
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

/// The coding-agent host `weave setup`/`weave uninstall` targets (WL-042). A
/// closed `ValueEnum` (no free text → no injection); default `claude` preserves
/// today's behavior. Maps onto [`setup::Provider`].
#[derive(Clone, Copy, clap::ValueEnum)]
enum SetupProvider {
    Claude,
    Codex,
    Gemini,
    Aider,
}

impl From<SetupProvider> for setup::Provider {
    fn from(p: SetupProvider) -> Self {
        match p {
            SetupProvider::Claude => setup::Provider::Claude,
            SetupProvider::Codex => setup::Provider::Codex,
            SetupProvider::Gemini => setup::Provider::Gemini,
            SetupProvider::Aider => setup::Provider::Aider,
        }
    }
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
                Ok(Box::new(weave_core::store_libsql::LibsqlStore::open(cfg)?))
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

/// Parse a CLI `--scope` string into a `MemoryScope`.
fn parse_memory_scope(
    scope: &str,
    cfg: &Config,
    from: Option<&str>,
) -> anyhow::Result<MemoryScope> {
    match scope {
        "global" => Ok(MemoryScope::Global),
        "project" => {
            let cwd = std::env::current_dir()?;
            let tags = git::capture_worktree_tags(&cwd);
            if tags.repo.is_empty() {
                anyhow::bail!("not in a git repo; specify 'global' or run inside a git repo");
            }
            Ok(MemoryScope::Project(tags.repo))
        }
        "persona" => {
            let me = resolve_me(from.map(|s| s.to_string()), None, cfg);
            Ok(MemoryScope::Persona(me))
        }
        "orchestrator" => Ok(MemoryScope::Orchestrator(cfg.circle())),
        other => anyhow::bail!(
            "scope must be one of: global, project, persona, orchestrator (got '{other}')"
        ),
    }
}

/// Optionally prepend memory context to a body. Non-fatal: any problem returns the original body.
fn maybe_prefix_body(cfg: &Config, from: &str, body: &str, no_memory: bool) -> String {
    if no_memory {
        return body.to_string();
    }
    let prefix = memory::build_context_prefix(from, &cfg.circle(), body, 3);
    if prefix.is_empty() {
        body.to_string()
    } else {
        format!("{prefix}{body}")
    }
}

/// Resolve the effective circle for a `peers`/`sessions`/`scan` listing (P4),
/// returning `None` for "no filter" (mesh-wide). Precedence (repowire's
/// list_peers scoping, ported daemon-free):
///
/// 1. an explicit `--circle <name>` (`"*"` ⇒ mesh-wide) wins;
/// 2. `--all-circles` ⇒ mesh-wide;
/// 3. else if the caller's OWN peer row is `role='orchestrator'` ⇒ mesh-wide
///    (an orchestrator has cross-circle visibility);
/// 4. else the caller's own circle (`cfg.circle()`).
///
/// Backward-compat: with everyone in `"default"`, no flag, and a non-orchestrator
/// caller, this returns `Some("default")` which keeps every default-circle row ⇒
/// byte-identical to pre-P4.
fn resolve_list_circle(
    store: &dyn Store,
    cfg: &Config,
    me: &str,
    circle: Option<&str>,
    all_circles: bool,
) -> Option<String> {
    if let Some(c) = circle.filter(|s| !s.trim().is_empty()) {
        let c = c.trim();
        if c == "*" {
            return None;
        }
        return Some(model::circle_or_default(c).to_string());
    }
    if all_circles {
        return None;
    }
    // An orchestrator caller defaults to mesh-wide. One indexed lookup on the hot
    // path (comparable to refresh_presence's touch); a read failure falls back to
    // the caller's own circle (never widens visibility on error).
    if let Ok(Some(p)) = store.get_peer(me) {
        if model::PeerRole::from_str(&p.role) == Ok(model::PeerRole::Orchestrator) {
            return None;
        }
    }
    Some(cfg.circle())
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

/// WL-056 / ADR-0005: arguments for `weave push` (the cross-machine SEND verb).
/// Grouped into one struct so the dispatch arm threads a single value rather than
/// ten positional params (the `clippy::too_many_arguments` precedent).
#[cfg(feature = "surfaces")]
struct PushArgs {
    to: String,
    host: String,
    token: Option<String>,
    body: Option<String>,
    subject: Option<String>,
    to_host: Option<String>,
    priority: Option<String>,
    ttl: Option<i64>,
    idempotency_key: Option<String>,
    me: Option<String>,
}

/// WL-056 / ADR-0005: PUSH a message to a recipient on ANOTHER machine — the
/// A-initiated dual of a Tier-2 pull. Resolve `from`, read the body from `--body` or
/// stdin, sign the canonical `(from,to,body)` if a key is configured (the SAME
/// `sign_intent_if_keyed` the cross-store send uses), build a JSON-RPC
/// `tools/call {name:"weave_push",...}`, and POST it to B's `http://<host>/api`
/// endpoint with `Authorization: Bearer <token>`. B verifies + commits into its OWN
/// inbox and lights its OWN pane (owner-only-writes). A non-200 / B-side error is
/// surfaced as a CLI error.
///
/// SSRF avoidance: `--host` is EXPLICIT-ONLY — never auto-resolved from message
/// content. The token comes from `--token` > `$WEAVE_PUSH_TOKEN` > config (the
/// telegram/slack token-resolution precedent). Reuses the EXISTING blocking+rustls
/// `reqwest` client (no new HTTP dep) with a bounded timeout.
#[cfg(feature = "surfaces")]
fn push_to_remote(_store: &dyn Store, cfg: &Config, args: PushArgs) -> anyhow::Result<()> {
    use std::io::Read as _;
    use std::time::Duration;

    let from = resolve_me(args.me, None, cfg);

    // Body from --body, else stdin (the `summarize`/`hook` stdin precedent).
    let body = match args.body {
        Some(b) => b,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| anyhow::anyhow!("reading push body from stdin: {e}"))?;
            buf
        }
    };
    let body = body.trim_end_matches(['\n', '\r']).to_string();
    if body.is_empty() {
        anyhow::bail!("push body is empty (pass --body or pipe it on stdin)");
    }

    // Token: --token > $WEAVE_PUSH_TOKEN. (The serve/dashboard bearer token is
    // CLI/random, not a config key, so there is no config fallback here.)
    let token = args.token.filter(|t| !t.is_empty()).or_else(|| {
        std::env::var("WEAVE_PUSH_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    });

    // Normalize the explicit --host into a full http URL ending in /api. We accept
    // `host:port`, `http://host:port`, or a trailing /api and converge them. NO
    // auto-resolution from message content (SSRF avoidance) — `--host` is the only
    // source of the endpoint.
    let base = args.host.trim().trim_end_matches('/');
    let url = if base.starts_with("http://") || base.starts_with("https://") {
        if base.ends_with("/api") {
            base.to_string()
        } else {
            format!("{base}/api")
        }
    } else if base.ends_with("/api") {
        format!("http://{base}")
    } else {
        format!("http://{base}/api")
    };

    // Sign the canonical (from,to,body) if keyed (reuse the cross-store signer).
    let sig = sign_intent_if_keyed(&from, &args.to, &body);

    // Always populate the idempotency key (so a retried POST never double-commits on
    // B). If the caller didn't supply one, synthesize a stable key from (from,body)
    // via an FNV-1a digest — no rand/hash crate (weave is dependency-light).
    let idempotency_key = args
        .idempotency_key
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| synth_push_key(&from, &body));

    // Build the weave_push arguments object (the Intent wire form).
    let mut push_args = serde_json::Map::new();
    push_args.insert("from".into(), serde_json::Value::from(from.clone()));
    push_args.insert("to".into(), serde_json::Value::from(args.to.clone()));
    push_args.insert("body".into(), serde_json::Value::from(body));
    push_args.insert(
        "idempotency_key".into(),
        serde_json::Value::from(idempotency_key),
    );
    if !sig.is_empty() {
        push_args.insert("sig".into(), serde_json::Value::from(sig));
    }
    if let Some(s) = args.subject.filter(|s| !s.is_empty()) {
        push_args.insert("subject".into(), serde_json::Value::from(s));
    }
    if let Some(h) = args.to_host.filter(|s| !s.is_empty()) {
        push_args.insert("to_host".into(), serde_json::Value::from(h));
    }
    if let Some(p) = args.priority.filter(|s| !s.is_empty()) {
        push_args.insert("priority".into(), serde_json::Value::from(p));
    }
    if let Some(t) = args.ttl {
        push_args.insert("ttl".into(), serde_json::Value::from(t));
    }

    let rpc = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "weave_push", "arguments": serde_json::Value::Object(push_args)},
    });

    // Reuse the EXISTING blocking+rustls reqwest client (shared with telegram/slack/
    // llm) — NO new HTTP dep. Bounded timeout so a dead endpoint cannot hang the CLI.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow::anyhow!("building reqwest client: {e}"))?;
    let mut req = client.post(&url).json(&rpc);
    if let Some(t) = &token {
        req = req.bearer_auth(t);
    }
    let resp = req
        .send()
        .map_err(|e| anyhow::anyhow!("POST {url} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("push rejected by '{url}': HTTP {status}: {text}");
    }
    // The body is the JSON-RPC reply; surface B's tool result. A tool-level error
    // (isError:true) is reported as a CLI error so the operator sees a rejected push.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(err) = v.get("error") {
            anyhow::bail!("push rejected by '{url}': {err}");
        }
        let result = &v["result"];
        let is_error = result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let msg = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or(&text);
        if is_error {
            anyhow::bail!("push rejected by '{url}': {msg}");
        }
        println!("{msg}");
    } else {
        println!("{text}");
    }
    Ok(())
}

/// FNV-1a digest helper for a synthetic push idempotency key — mirrors the
/// receive-side `synth_push_idempotency_key` so a keyless push from the CLI is keyed
/// identically (no rand/hash crate).
#[cfg(feature = "surfaces")]
fn synth_push_key(from: &str, body: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in body.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("push:{from}:{h:x}")
}

/// Best-effort Tier-2 pull: commit any cross-store intents addressed to `me` from
/// the configured `pull_from` sources into the local inbox. Like the heartbeat, a
/// failure here must NEVER break the inbox drain — errors are reported to stderr
/// and swallowed. A no-op when `pull_from` is unconfigured (no Tier-2). Pulling
/// under a guessed identity is fine: it only commits intents explicitly addressed
/// to that name (no read-marking, so no inbox is consumed).
/// Build the signed-identity [`VerifyPolicy`](store::VerifyPolicy) from config: the
/// tri-state strict override plus the validated trust/revocation sets. Inert without
/// the `sign` feature (the fields are carried but never consulted), so a single
/// builder serves every backend/feature cross-product.
fn verify_policy(cfg: &Config) -> store::VerifyPolicy {
    store::VerifyPolicy {
        strict_override: cfg.strict_verify_override(),
        trust: cfg.trust_set(),
        revoked: cfg.revoked_set(),
    }
}

fn try_pull(store: &dyn Store, cfg: &Config, me: &str) {
    let allow = cfg.pull_from_sources();
    if allow.is_empty() {
        return;
    }
    match store::pull_from_store(store, me, &allow, &verify_policy(cfg)) {
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

/// WL-014: after a prompt hook drain, if `me` has open asks where they are the
/// askee, fire a content-free reminder nudge into THIS session's OWN registered
/// pane. Best-effort: any failure is logged to stderr and never blocks the drain.
fn nudge_open_asks(store: &dyn Store, me: &str) {
    let has = match store.has_open_asks(me) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weave] open-ask check skipped (non-fatal): {e}");
            return;
        }
    };
    if !has {
        return;
    }
    let Ok(Some(peer)) = store.get_peer(me) else {
        return;
    };
    let target = inject::Target::from_peer(&peer);
    if !target.injectable() || !inject::target_alive(&target) {
        return;
    }
    match inject::inject_text(&target, "[weave] you have open ask(s) — run weave_asks") {
        Ok(_) => {}
        Err(err) => eprintln!("[weave] open-ask nudge failed (non-fatal): {err}"),
    }
}

/// WL-015: render open asks as actionable prompts in the prompt hook stdout.
/// Printed AFTER the message drain so the recipient sees every unanswered ask
/// with instructions on how to reply. Best-effort: never blocks the drain.
fn render_open_asks(store: &dyn Store, me: &str) {
    let asks = match store.list_asks(me, model::AskRole::Askee, 10) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weave] open-ask render skipped (non-fatal): {e}");
            return;
        }
    };
    let open: Vec<_> = asks
        .into_iter()
        .filter(|a| a.state == model::AskState::Open)
        .collect();
    if open.is_empty() {
        return;
    }
    for ask in &open {
        let subj = ask.subject.as_deref().unwrap_or("(no subject)");
        match ask.kind {
            model::AskKind::Choice => {
                println!("[weave] open ask from {}: {}", ask.asker, subj);
                if let Some(ref opts) = ask.options {
                    for (i, line) in opts.lines().enumerate() {
                        println!("  {}. {}", i + 1, line);
                    }
                }
                println!(
                    "  Reply with: weave_answer --id {} --body \"<number>\"",
                    ask.id
                );
            }
            model::AskKind::ToolPermission => {
                println!("[weave] open ask from {}: {}", ask.asker, subj);
                if let Some(ref opts) = ask.options {
                    let mut lines = opts.lines();
                    if let Some(tool) = lines.next() {
                        println!("  Tool: {tool}");
                    }
                    if let Some(args) = lines.next() {
                        println!("  Args: {args}");
                    }
                }
                println!(
                    "  Reply with: weave_answer --id {} --body \"yes\" to approve",
                    ask.id
                );
            }
            model::AskKind::FreeText => {
                println!("[weave] open ask from {}: {}", ask.asker, subj);
                println!(
                    "  Reply with: weave_answer --id {} --body \"<answer>\"",
                    ask.id
                );
            }
        }
    }
}

fn try_inject(store: &dyn Store, cfg: &Config, from: &str, to: &str, body: &str) -> Result<()> {
    if model::is_broadcast(to) {
        return Ok(());
    }
    if let Some(peer) = store.get_peer(to)? {
        let t = inject::Target::from_peer(&peer);
        if t.injectable() {
            match inject_text(&t, &cfg.nudge(from, body)) {
                Ok(true) => println!("injected into {} '{}'", t.mux.as_str(), t.id),
                Ok(false) => {}
                Err(err) => eprintln!("inject failed ({err}); will arrive on next turn"),
            }
        }
    }
    Ok(())
}

/// Fire the caller-side live nudge for an ask/answer and return the HONEST delivery
/// verdict string, reusing the EXISTING injector return (no new spawn path, no
/// `store → inject` edge — exactly the `try_inject` seam). A broadcast/queued/
/// not-injectable recipient is never an error; the message is safely in the store.
///   * `inject` returned `Ok(true)` ⇒ `transport_delivered`;
///   * registered-but-not-alive / `Ok(false)` / `Err` ⇒ `queued_next_turn`;
///   * `mux=none` / no peer row ⇒ `recipient_not_injectable`.
fn ask_inject_verdict(
    store: &dyn Store,
    cfg: &Config,
    from: &str,
    to: &str,
    body: &str,
) -> &'static str {
    let Ok(Some(peer)) = store.get_peer(to) else {
        return "recipient_not_injectable";
    };
    let t = inject::Target::from_peer(&peer);
    match inject::capability(&t) {
        inject::Capability::NotInjectable => "recipient_not_injectable",
        _ => match inject_text(&t, &cfg.nudge(from, body)) {
            Ok(true) => "transport_delivered",
            Ok(false) => "queued_next_turn",
            Err(err) => {
                eprintln!("inject failed ({err}); will arrive on next turn");
                "queued_next_turn"
            }
        },
    }
}

/// Best-effort delivery-trace write (CLI side): append one metadata-only stage row,
/// swallowing (and logging to STDERR) any store error so a trace failure can NEVER
/// sink the delivery path. Mirrors the gc/git-tag best-effort precedent. The store
/// records the OUTCOME passed here AFTER the inject — NO `store → inject` edge.
fn record_delivery_best_effort(
    store: &dyn Store,
    ref_id: i64,
    kind: model::DeliveryRefKind,
    to: &str,
    stage: model::DeliveryStage,
    outcome: model::DeliveryOutcome,
) {
    if let Err(err) =
        store.record_delivery(ref_id, kind.as_str(), to, stage.as_str(), outcome.as_str())
    {
        eprintln!("delivery-trace write failed (non-fatal): {err}");
    }
}

/// Inject a freshly-persisted point-to-point message AND record its delivery trace
/// (queued + the post-inject stage), returning the normalized HONEST verdict token so
/// the caller can print it WITHOUT a second inject. Caller-side, best-effort trace —
/// never sinks the send. Skips broadcast (not injected, not traced in P6 — returns
/// `recipient_not_injectable`). The recorded stage and the returned verdict are
/// derived from the SAME inject result, so they can never disagree.
fn inject_and_trace(
    store: &dyn Store,
    cfg: &Config,
    ref_id: i64,
    kind: model::DeliveryRefKind,
    from: &str,
    to: &str,
    body: &str,
) -> Result<&'static str> {
    if model::is_broadcast(to) {
        return Ok("recipient_not_injectable");
    }
    record_delivery_best_effort(
        store,
        ref_id,
        kind,
        to,
        model::DeliveryStage::Queued,
        model::DeliveryOutcome::Ok,
    );
    let (stage, outcome, verdict) = if let Some(peer) = store.get_peer(to)? {
        let t = inject::Target::from_peer(&peer);
        if t.injectable() {
            match inject_text(&t, &cfg.nudge(from, body)) {
                Ok(true) => {
                    println!("injected into {} '{}'", t.mux.as_str(), t.id);
                    (
                        model::DeliveryStage::Injected,
                        model::DeliveryOutcome::Ok,
                        "transport_delivered",
                    )
                }
                Ok(false) => (
                    model::DeliveryStage::Queued,
                    model::DeliveryOutcome::Ok,
                    "queued_next_turn",
                ),
                Err(err) => {
                    eprintln!("inject failed ({err}); will arrive on next turn");
                    (
                        model::DeliveryStage::InjectFailed,
                        model::DeliveryOutcome::Fail,
                        "queued_next_turn",
                    )
                }
            }
        } else {
            (
                model::DeliveryStage::NotInjectable,
                model::DeliveryOutcome::Ok,
                "recipient_not_injectable",
            )
        }
    } else {
        (
            model::DeliveryStage::NotInjectable,
            model::DeliveryOutcome::Ok,
            "recipient_not_injectable",
        )
    };
    record_delivery_best_effort(store, ref_id, kind, to, stage, outcome);
    Ok(verdict)
}

/// Diagnostics: backend, db, detected multiplexer, peers, Claude wiring.
fn doctor(store: &dyn Store, cfg: &Config, json: bool) -> Result<()> {
    let target = inject::detect_target_with_preference(parse_mux_preference(cfg));
    // Tier-1 federation: report the union peer count (local + read-only extra
    // stores). `extra` empty ⇒ exactly the local peers, identical-to-today.
    let extra = cfg.peer_db_sources();
    let remote_count = extra.iter().filter(|s| s.is_remote()).count();
    let views = store::federated_peers(store, &extra)?;
    let total_peers = views.len();
    let online = views.iter().filter(|v| is_alive(&v.peer)).count();
    // Session-scan observability: how many peers carry a git repo/worktree tag (a
    // self-describing, scan-able session). A 0 here on a populated mesh hints the
    // sessions predate the scan feature or run in non-git cwds.
    let tagged = views
        .iter()
        .filter(|v| !v.peer.repo.is_empty() || !v.peer.worktree_id.is_empty())
        .count();
    // Host-aware liveness breakdown over the peer set (A2 vocabulary, display-only):
    // one pass classifying each already-pulled row; deterministic given this_host/now,
    // secret-free (host/liveness only). Never a cross-machine probe.
    let this_host = config::this_host();
    let now_ts = model::now();
    let mut peers_alive_local = 0usize;
    let mut peers_alive_remote = 0usize;
    let mut peers_stale = 0usize;
    for v in &views {
        match store::liveness_for(&v.peer, &this_host, now_ts) {
            store::Liveness::AliveLocal => peers_alive_local += 1,
            store::Liveness::AliveRemote => peers_alive_remote += 1,
            store::Liveness::Stale => peers_stale += 1,
        }
    }
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
    // Token-FREE per-source TIMEOUT-tier observability: where each remote source's
    // effective remote-call timeout came from (per-source label / global / default)
    // plus the effective ms range over the remotes. NEVER prints a token byte or the
    // label↔timeout↔token pairing — only aggregate tier COUNTS + a plain ms range.
    let timeout_tiers = cfg.peer_db_remote_timeout_tiers();
    let timeout_per_source = timeout_tiers
        .iter()
        .filter(|(_, t)| *t == config::PullTimeoutTier::PerSourceLabel)
        .count();
    let timeout_global = timeout_tiers
        .iter()
        .filter(|(_, t)| *t == config::PullTimeoutTier::Global)
        .count();
    let timeout_default = timeout_tiers
        .iter()
        .filter(|(_, t)| *t == config::PullTimeoutTier::Default)
        .count();
    let timeout_ms_min = timeout_tiers.iter().map(|(ms, _)| *ms).min();
    let timeout_ms_max = timeout_tiers.iter().map(|(ms, _)| *ms).max();
    // Secret-free federation-health rollup over BOTH source kinds (peer_db AND the
    // previously-unsurfaced pull_from delivery set). Counts/tiers only — never a
    // token. Reads config/env only; no new network probe (reachability for the
    // peer_db set is the already-computed fed_ok/fed_skipped above).
    let fed_health = cfg.federation_health();
    let total = store.total_messages()?;
    let claude = inject::have("claude");
    let db = cfg.db_path();
    // FR6: warn when the resolved store is NOT the well-known XDG default. The most
    // common "why can't I see the other session's peers" cause is each session
    // pointing at a different WEAVE_DB, so surface it as a diagnostic hint.
    let db_default = config::default_db_path();
    let db_is_default = db == db_default;
    if json {
        let mut report = serde_json::json!({
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
            "peers_tagged": tagged,
            "peers_alive_local": peers_alive_local,
            "peers_alive_remote": peers_alive_remote,
            "peers_stale": peers_stale,
            "claude_on_path": claude,
            "federation_stores": extra.len(),
            "federation_stores_ok": fed_ok,
            "federation_stores_skipped": fed_skipped,
            "federation_remote_stores": remote_count,
            "federation_remote_unsupported": remote_unsupported,
            "federation_remote_token_per_source": token_per_source,
            "federation_remote_token_shared": token_shared,
            "federation_remote_token_none": token_none,
        });
        // Signed-identity summary (counts + local fingerprint only, secret-free):
        // surfaces the trust/revocation policy and this session's own fingerprint so
        // a misconfigured trust set is diagnosable. Sign-gated; never any secret.
        #[cfg(feature = "sign")]
        if let Some(obj) = report.as_object_mut() {
            let trust = cfg.trust_set();
            let revoked = cfg.revoked_set();
            let local_fp = sign::local_public_key()
                .ok()
                .flatten()
                .and_then(|pk| sign::fingerprint(&pk));
            obj.insert("sign_trust_set".into(), trust.len().into());
            obj.insert("sign_revoked_set".into(), revoked.len().into());
            // Multi-key registry health (#7), secret-free: how many identities have a
            // registered key, the total registered keys, and any identity holding
            // more than one key (rotation overlap in progress). Pubkeys are NOT
            // surfaced here (use `weave key list`); only counts.
            if let Ok(pairs) = store.list_keys() {
                use std::collections::BTreeMap;
                let mut per_ident: BTreeMap<String, usize> = BTreeMap::new();
                for (ident, _pk) in &pairs {
                    *per_ident.entry(ident.clone()).or_insert(0) += 1;
                }
                obj.insert("sign_key_identities".into(), per_ident.len().into());
                obj.insert("sign_registered_keys".into(), pairs.len().into());
                let multi = per_ident.values().filter(|&&c| c > 1).count();
                obj.insert("sign_identities_multi_key".into(), multi.into());
                // How many REGISTERED keys' fingerprints are currently in the
                // revoked set (per-registered-key revoked breakdown). Reuses the same
                // FULL-digest `fingerprint_matches` idiom as `key list`. Secret-free.
                let hit = pairs
                    .iter()
                    .filter(|(_, pk)| revoked.iter().any(|e| sign::fingerprint_matches(e, pk)))
                    .count();
                obj.insert("sign_registered_keys_revoked".into(), hit.into());
            }
            // Observed-revocation audit count (#11): how many enforcement/declared
            // events are recorded. Read-only rollup; never feeds the decision.
            if let Ok(n) = store.count_revocations() {
                obj.insert("sign_revocation_events".into(), n.into());
            }
            obj.insert(
                "sign_strict_verify".into(),
                match cfg.strict_verify_override() {
                    Some(true) => "forced".into(),
                    Some(false) => "disabled".into(),
                    None => "default".into(),
                },
            );
            obj.insert(
                "sign_local_fingerprint".into(),
                match local_fp {
                    Some(fp) => fp.into(),
                    None => serde_json::Value::Null,
                },
            );
        }
        // Additive (token-free) per-source timeout observability — only when there is
        // at least one remote source, so the surface is unchanged for local-only
        // configs. min/max are the effective-ms bound over the remotes.
        if remote_count > 0 {
            if let Some(obj) = report.as_object_mut() {
                obj.insert(
                    "federation_remote_timeout_per_source".into(),
                    timeout_per_source.into(),
                );
                obj.insert(
                    "federation_remote_timeout_global".into(),
                    timeout_global.into(),
                );
                obj.insert(
                    "federation_remote_timeout_default".into(),
                    timeout_default.into(),
                );
                if let (Some(min), Some(max)) = (timeout_ms_min, timeout_ms_max) {
                    obj.insert("federation_remote_timeout_ms_min".into(), min.into());
                    obj.insert("federation_remote_timeout_ms_max".into(), max.into());
                }
            }
        }
        // Additive (token-free) federation-health rollup for the `pull_from` delivery
        // set — the side `doctor` never surfaced before (parity with the peer_db keys
        // above). Emitted ONLY when `pull_from` is configured, so local-only configs
        // are byte-unchanged. Counts/tiers only; never a token. ms range only when a
        // remote pull source exists (no misleading 0-0 over zero remotes).
        let ph = &fed_health.pull_from;
        if ph.total > 0 {
            if let Some(obj) = report.as_object_mut() {
                obj.insert("federation_pull_sources".into(), ph.total.into());
                obj.insert("federation_pull_local".into(), ph.local.into());
                obj.insert("federation_pull_remote".into(), ph.remote.into());
                obj.insert(
                    "federation_pull_token_per_source".into(),
                    ph.token_per_source.into(),
                );
                obj.insert(
                    "federation_pull_token_shared".into(),
                    ph.token_shared.into(),
                );
                obj.insert("federation_pull_token_none".into(), ph.token_none.into());
                obj.insert(
                    "federation_pull_timeout_per_source".into(),
                    ph.timeout_per_source.into(),
                );
                obj.insert(
                    "federation_pull_timeout_global".into(),
                    ph.timeout_global.into(),
                );
                obj.insert(
                    "federation_pull_timeout_default".into(),
                    ph.timeout_default.into(),
                );
                if let (Some(min), Some(max)) = (ph.ms_min, ph.ms_max) {
                    obj.insert("federation_pull_timeout_ms_min".into(), min.into());
                    obj.insert("federation_pull_timeout_ms_max".into(), max.into());
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&report)?);
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
        println!("  peers:          {total_peers} ({online} online, {tagged} tagged)");
        println!(
            "  liveness:       {peers_alive_local} local-alive, {peers_alive_remote} remote-alive, {peers_stale} stale"
        );
        println!("  claude on PATH: {}", if claude { "yes" } else { "no" });
        // Signed-identity summary (secret-free): trust/revocation counts, the
        // strict-verify mode, and this session's OWN fingerprint (never the secret).
        #[cfg(feature = "sign")]
        {
            let trust = cfg.trust_set();
            let revoked = cfg.revoked_set();
            let mode = match cfg.strict_verify_override() {
                Some(true) => "forced",
                Some(false) => "disabled",
                None => "default (trust-set aware)",
            };
            let local_fp = sign::local_public_key()
                .ok()
                .flatten()
                .and_then(|pk| sign::fingerprint(&pk))
                .unwrap_or_else(|| "none".to_string());
            // Per-registered-key revoked breakdown + observed-revocation event count
            // (#11), both secret-free. `hit` is how many REGISTERED keys are currently
            // in the revoked set; `events` is the audit-log size.
            let hit = store
                .list_keys()
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter(|(_, pk)| revoked.iter().any(|e| sign::fingerprint_matches(e, pk)))
                        .count()
                })
                .unwrap_or(0);
            let events = store.count_revocations().unwrap_or(0);
            println!(
                "  signed id:      strict={mode}, trusted={}, revoked={}",
                trust.len(),
                revoked.len()
            );
            println!(
                "  revoked keys:   {} in policy, {hit} registered key(s) hit, {events} event(s) logged",
                revoked.len()
            );
            println!("  my fingerprint: {local_fp}");
        }
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
                let (tmin, tmax) = (timeout_ms_min.unwrap_or(0), timeout_ms_max.unwrap_or(0));
                println!(
                    "  remote timeout: {timeout_per_source} per-source, {timeout_global} global, {timeout_default} default (effective {tmin}-{tmax} ms)"
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
        // Additive secret-free "federation health" block for the `pull_from` delivery
        // set — the side `doctor` never surfaced before (now at parity with peer_db).
        // Printed ONLY when `pull_from` is configured, so local-only output is
        // byte-unchanged. Counts/tiers only; never a token. No reachability is shown
        // for pull_from (would require a new probe — forbidden): resolved view only.
        let ph = &fed_health.pull_from;
        if ph.total > 0 {
            println!(
                "  pull sources:   {} configured ({} local, {} remote)",
                ph.total, ph.local, ph.remote
            );
            if ph.remote > 0 {
                println!(
                    "  pull tokens:    {} per-source, {} shared, {} none",
                    ph.token_per_source, ph.token_shared, ph.token_none
                );
                let (pmin, pmax) = (ph.ms_min.unwrap_or(0), ph.ms_max.unwrap_or(0));
                println!(
                    "  pull timeout:   {} per-source, {} global, {} default (effective {pmin}-{pmax} ms)",
                    ph.timeout_per_source, ph.timeout_global, ph.timeout_default
                );
            }
        }
    }
    Ok(())
}

/// Capture the git session tags (repo / branch / worktree id) for an optional cwd
/// string, falling back to the process `current_dir()` when `cwd` is `None`. Pure
/// glue around [`git::capture_worktree_tags`]; best-effort and total — a non-git
/// cwd or any failure yields empty tags (the store sanitizes + bounds them on
/// write). Returns `WorktreeTags::default()` (all empty) when no cwd can be
/// resolved at all.
fn git_tags_for(cwd: Option<&str>) -> git::WorktreeTags {
    let path = match cwd {
        Some(c) => std::path::PathBuf::from(c),
        None => match std::env::current_dir() {
            Ok(p) => p,
            Err(_) => return git::WorktreeTags::default(),
        },
    };
    git::capture_worktree_tags(&path)
}

/// Human reason string for a peer's host-aware liveness verdict, used by `scan`.
/// Pure formatting over the already-computed [`store::Liveness`]; surfaces the
/// pid-confirmed-vs-TTL-presumed nuance that the 3-variant enum deliberately
/// folds away (a same-host alive row is "pid" iff its PID is known, else "ttl").
fn scan_liveness_reason(p: &model::Peer, liveness: store::Liveness) -> String {
    match liveness {
        store::Liveness::AliveLocal => {
            if p.pid.is_some() {
                "alive (local, pid)".to_string()
            } else {
                "alive (local, ttl)".to_string()
            }
        }
        store::Liveness::AliveRemote => "alive (remote, ttl)".to_string(),
        store::Liveness::Stale => "stale".to_string(),
    }
}

/// Build a `name -> Peer` map of the LOCAL store's peers for a display-layer tag
/// join (e.g. attaching repo/branch/worktree to `sessions`, which is message-derived
/// and carries no peer/git data). Best-effort: a read failure yields an empty map
/// (sessions simply render without tags), never an error. Only the local store is
/// consulted — never foreign/federated rows — so the join stays owner/secret-free.
fn local_peer_tag_map(store: &dyn Store) -> std::collections::HashMap<String, model::Peer> {
    store
        .list_peers()
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect()
}

/// Render a peer's git session tags for a human listing, e.g.
/// ` {weave@feat/x #my-wt}`, omitting any empty field and the whole group when all
/// three are empty (a non-git session prints nothing extra). Pure formatting.
fn fmt_peer_tags(p: &model::Peer) -> String {
    if p.repo.is_empty() && p.branch.is_empty() && p.worktree_id.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    match (p.repo.is_empty(), p.branch.is_empty()) {
        (false, false) => parts.push(format!("{}@{}", p.repo, p.branch)),
        (false, true) => parts.push(p.repo.clone()),
        (true, false) => parts.push(format!("@{}", p.branch)),
        (true, true) => {}
    }
    if !p.worktree_id.is_empty() {
        parts.push(format!("#{}", p.worktree_id));
    }
    format!(" {{{}}}", parts.join(" "))
}

/// Compact turn_state marker for a human listing (P5), e.g. ` [working]`. NON-NOISY
/// by design (the #8/#19 liveness-reason pattern): an `idle` or `Unknown` peer
/// renders NOTHING, so a legacy/pre-P5 peer's line is byte-identical to before; only
/// a `working`/`awaiting_input`/`pending_first_turn` state adds a marker. Pure
/// formatting. The Peer is expected to carry the read-time-TTL'd view already.
fn fmt_turn_state(p: &model::Peer) -> String {
    match model::TurnState::from_str(&p.turn_state) {
        Ok(model::TurnState::Working) => " [working]".to_string(),
        Ok(model::TurnState::AwaitingInput) => " [awaiting-input]".to_string(),
        Ok(model::TurnState::PendingFirstTurn) => " [pending]".to_string(),
        // Idle / Unknown / an unparseable legacy value ⇒ nothing (default unchanged).
        _ => String::new(),
    }
}

/// Compact description suffix for a human listing (P5), e.g. ` "reviewing PR #23"`.
/// An empty (unset/TTL-expired) description renders NOTHING, so the default line is
/// unchanged. The text is already bounded + control-stripped at the store seam. Pure.
fn fmt_description(p: &model::Peer) -> String {
    if p.description.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", p.description)
    }
}

/// Upper bound on messages returned for a `thread` view.
const MAX_THREAD: i64 = 1_000;

// ---------------------------------------------------------------------------
// `weave sessions --watch` presence dashboard (read-only, dependency-light).
// ---------------------------------------------------------------------------

/// Per-(repo, branch) section row budget for the dashboard. A group with more than
/// this many rows is truncated to the budget plus a `+N more` line, so an enormous
/// federated mesh can never blow up one frame's height. Not user-tunable in v1.
const DASHBOARD_GROUP_ROW_BUDGET: usize = 20;

/// ANSI clear-screen + cursor-home: clears the scrollback-visible screen and parks
/// the cursor at the top-left so each watch frame overwrites the last in place. A
/// plain literal (never built from runtime input); emitted ONLY when the clear gate
/// (`clear_enabled`) is on — never for `--json`, a non-TTY, or `NO_COLOR`/`WEAVE_NO_CLEAR`.
const ANSI_CLEAR_HOME: &str = "\x1b[2J\x1b[H";

/// A single session row flattened from a [`store::PeerView`] for the presence
/// dashboard. Pure data (no store/`Peer` reference) so [`render_sessions_dashboard`]
/// is unit-testable from hand-built snapshots with no store. Carries ONLY display
/// tags — name/repo/branch/worktree/mux/host plus the raw `pid`/`last_seen` presence
/// fields the render classifies via [`store::liveness_from_fields`] — never a token
/// or URL. Liveness is NOT precomputed here: the render computes the host-aware
/// [`store::Liveness`] itself from `(pid, last_seen, host)` + the passed-in
/// `this_host`/`now`, keeping the dashboard deterministic from those two inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRow {
    name: String,
    repo: String,
    branch: String,
    worktree: String,
    mux: String,
    host: String,
    /// Same-host PID when known — feeds the local-arm pid probe in liveness.
    pid: Option<i64>,
    /// Last-seen epoch seconds — feeds the TTL recency guard in liveness.
    last_seen: i64,
    /// foreign-origin label (e.g. `messages.db`) for `(via …)`, empty when local.
    via: String,
    /// Live turn_state label (P5); `""` (Unknown) for a legacy/pre-P5 row. Rendered
    /// compactly + non-noisily (only working/awaiting-input/pending add a marker).
    turn_state: String,
    /// Free-form description (P5), already read-time-TTL'd by the store; `""` ⇒
    /// nothing rendered.
    description: String,
}

/// Pure-render options for the presence dashboard. `clear` toggles the ANSI
/// clear-home prefix (resolved by the caller from the TTY/`NO_COLOR`/`WEAVE_NO_CLEAR`
/// gate, off for `--json`); `repo`/`branch` are the active exact-match filters,
/// echoed into the header for context. No I/O lives here — the render is total.
#[derive(Debug, Clone, Default)]
struct DashboardOpts {
    clear: bool,
    repo: Option<String>,
    branch: Option<String>,
}

/// Decide whether the ANSI clear-home prefix should be emitted: only when stdout is
/// a real TTY AND neither `NO_COLOR` nor `WEAVE_NO_CLEAR` is set (either, even empty,
/// disables it). For `--json` the caller forces this off. Reads env + the terminal —
/// the impure companion to the pure [`render_sessions_dashboard`].
fn clear_enabled() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() || std::env::var_os("WEAVE_NO_CLEAR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Flatten + sort federated peer views into deterministic dashboard rows, grouped
/// downstream by (repo, branch). Sorting here (repo, branch, name) makes the rendered
/// frame stable regardless of store row order — important for hermetic tests. Pure.
fn dashboard_rows(views: &[store::PeerView]) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = views
        .iter()
        .map(|v| {
            let p = &v.peer;
            SessionRow {
                name: p.name.clone(),
                repo: p.repo.clone(),
                branch: p.branch.clone(),
                worktree: p.worktree_id.clone(),
                mux: p.mux.clone(),
                host: p.host.clone(),
                pid: p.pid,
                last_seen: p.last_seen,
                via: if v.origin.is_foreign() {
                    v.origin.label().to_string()
                } else {
                    String::new()
                },
                turn_state: p.turn_state.clone(),
                description: p.description.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.repo, &a.branch, &a.name).cmp(&(&b.repo, &b.branch, &b.name)));
    rows
}

/// Host-aware liveness for a dashboard row, computed from its raw presence fields
/// via [`store::liveness_from_fields`] (the same classifier `scan`/`peers`/`doctor`
/// use). Deterministic given `this_host`/`now`; the only env-dependence is the
/// same-host PID probe gated to the local arm — exactly as `scan` accepts.
fn row_liveness(r: &SessionRow, this_host: &str, now: i64) -> store::Liveness {
    store::liveness_from_fields(&r.host, r.pid, r.last_seen, this_host, now)
}

/// Human reason string for a dashboard row's host-aware verdict — mirrors
/// [`scan_liveness_reason`] over the row's fields (a same-host alive row is "pid"
/// iff its PID is known, else "ttl"). Reuses the exact A2 vocabulary.
fn row_liveness_reason(r: &SessionRow, liveness: store::Liveness) -> &'static str {
    match liveness {
        store::Liveness::AliveLocal => {
            if r.pid.is_some() {
                "alive (local, pid)"
            } else {
                "alive (local, ttl)"
            }
        }
        store::Liveness::AliveRemote => "alive (remote, ttl)",
        store::Liveness::Stale => "stale",
    }
}

/// Render one presence-dashboard frame as a `String` (the testable seam). PURE: no
/// I/O, no clock, no sleep — `now` AND `this_host` are passed in so frame contents
/// (including the host-aware liveness via [`store::liveness_from_fields`]) are
/// deterministic from `(now, this_host)`. The only env-dependence is liveness's
/// same-host PID probe (gated to the local arm), mirroring `scan`.
///
/// Layout: an optional ANSI clear-home prefix (when `opts.clear`), a header summary
/// (`now`, total sessions, the `N local-alive, M remote-alive, K stale` breakdown,
/// #repos, #branches, active filters), then one section per (repo, branch) group —
/// in the sorted order of [`dashboard_rows`] — each listing
/// `name·[reason]·worktree·mux·host` rows with a ` <remote>` marker for off-host
/// rows (the same idiom as `scan`). A group exceeding [`DASHBOARD_GROUP_ROW_BUDGET`]
/// renders the budgeted rows plus a `+N more` line. The empty snapshot renders a
/// stable `no sessions` body. Output is secret-free (tags + host/liveness only; the
/// `via` label is the redacted store basename, never a token/URL).
fn render_sessions_dashboard(
    rows: &[SessionRow],
    opts: &DashboardOpts,
    this_host: &str,
    now: i64,
) -> String {
    let mut out = String::new();
    if opts.clear {
        out.push_str(ANSI_CLEAR_HOME);
    }

    let total = rows.len();
    // Host-aware liveness breakdown over the whole snapshot (A2 vocabulary).
    let mut local_alive = 0usize;
    let mut remote_alive = 0usize;
    let mut stale = 0usize;
    for r in rows {
        match row_liveness(r, this_host, now) {
            store::Liveness::AliveLocal => local_alive += 1,
            store::Liveness::AliveRemote => remote_alive += 1,
            store::Liveness::Stale => stale += 1,
        }
    }
    // Distinct repos / branches in first-seen (already sorted) order.
    let mut repos: Vec<&str> = Vec::new();
    let mut branches: Vec<(&str, &str)> = Vec::new();
    for r in rows {
        if !repos.contains(&r.repo.as_str()) {
            repos.push(&r.repo);
        }
        let key = (r.repo.as_str(), r.branch.as_str());
        if !branches.contains(&key) {
            branches.push(key);
        }
    }

    let mut filt = String::new();
    if let Some(r) = opts.repo.as_deref() {
        filt.push_str(&format!(" repo={r}"));
    }
    if let Some(b) = opts.branch.as_deref() {
        filt.push_str(&format!(" branch={b}"));
    }
    out.push_str(&format!(
        "weave sessions [{}] — {total} session(s), {local_alive} local-alive, {remote_alive} remote-alive, {stale} stale, {} repo(s), {} branch(es){filt}\n",
        model::fmt_ts(now),
        repos.len(),
        branches.len(),
    ));

    if rows.is_empty() {
        out.push_str("no sessions\n");
        return out;
    }

    // Emit one section per (repo, branch) group, in sorted row order. `rows` is
    // already sorted by (repo, branch, name), so equal-key rows are contiguous.
    fn dash(s: &str) -> &str {
        if s.is_empty() {
            "-"
        } else {
            s
        }
    }
    let mut i = 0;
    while i < rows.len() {
        let repo = &rows[i].repo;
        let branch = &rows[i].branch;
        let mut j = i;
        while j < rows.len() && &rows[j].repo == repo && &rows[j].branch == branch {
            j += 1;
        }
        let group = &rows[i..j];
        let g_alive = group
            .iter()
            .filter(|r| !matches!(row_liveness(r, this_host, now), store::Liveness::Stale))
            .count();
        out.push_str(&format!(
            "\n[{} / {}] {} session(s), {g_alive} alive\n",
            dash(repo),
            dash(branch),
            group.len(),
        ));
        let shown = group.len().min(DASHBOARD_GROUP_ROW_BUDGET);
        for r in &group[..shown] {
            let liveness = row_liveness(r, this_host, now);
            let reason = row_liveness_reason(r, liveness);
            let remote_marker = if r.host != this_host { " <remote>" } else { "" };
            let via = if r.via.is_empty() {
                String::new()
            } else {
                format!(" (via {})", r.via)
            };
            // P5 non-noisy presence markers: an idle/unknown turn_state and an empty
            // description render NOTHING, so a pre-P5 row's line is byte-identical.
            let ts_marker = match model::TurnState::from_str(&r.turn_state) {
                Ok(model::TurnState::Working) => " [working]",
                Ok(model::TurnState::AwaitingInput) => " [awaiting-input]",
                Ok(model::TurnState::PendingFirstTurn) => " [pending]",
                _ => "",
            };
            let desc = if r.description.is_empty() {
                String::new()
            } else {
                format!(" \"{}\"", r.description)
            };
            out.push_str(&format!(
                "  {}{remote_marker} [{reason}]{ts_marker} worktree={} mux={} host={}{desc}{via}\n",
                r.name,
                dash(&r.worktree),
                dash(&r.mux),
                dash(&r.host),
            ));
        }
        if group.len() > shown {
            out.push_str(&format!("  +{} more\n", group.len() - shown));
        }
        i = j;
    }
    out
}

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

/// Autonomous orchestration harnesses. Wraps the checked-in Ralph loop script;
/// the Rust CLI owns discovery, defaults, dry-run output, and process execution.
#[derive(Subcommand)]
enum HarnessCmd {
    /// Seven-layer Codex IDE-merge-IDE loop: Kimi Code plans/reviews, Ollama
    /// launches Claude MiniMax for implementation, durable workspace sentinels
    /// control resume/handoff. Defaults to dry-run; pass --execute to run.
    IdeMergeIde {
        /// Worktree that contains Cargo.toml and the checked-in weave-loop script.
        #[arg(long)]
        worktree: Option<std::path::PathBuf>,
        /// Number of backlog cycles a fresh MiniMax pass may close before handoff.
        #[arg(long, default_value_t = 3)]
        budget: u32,
        /// External runner hard stop.
        #[arg(long, default_value_t = 50)]
        max_iters: u32,
        /// Seconds between fresh MiniMax sessions.
        #[arg(long, default_value_t = 5)]
        sleep: u64,
        /// Execute the harness. Without this flag the command only prints the plan.
        #[arg(long)]
        execute: bool,
        /// Keep destructive applies disabled inside the loop.
        #[arg(long)]
        safe: bool,
        /// Print machine-readable JSON plan in dry-run mode.
        #[arg(long)]
        json: bool,
        /// Command prefix used for the implementation agent.
        #[arg(
            long,
            default_value = "ollama launch claude --model minimax-m3:cloud --"
        )]
        agent_cmd: String,
        /// Label passed through to the existing runner for logs.
        #[arg(long, default_value = "minimax-m3:cloud")]
        model: String,
        /// Extra args appended after the implementation prompt.
        #[arg(long, default_value = "")]
        agent_model_args: String,
        /// Disable Kimi Code preflight.
        #[arg(long)]
        no_kimi_plan: bool,
        /// Disable Kimi Code review.
        #[arg(long)]
        no_kimi_review: bool,
        /// Kimi Code command.
        #[arg(long, default_value = "kimi-legacy")]
        kimi_cmd: String,
        /// Kimi Code model label.
        #[arg(long, default_value = "kimi-code/kimi-for-coding")]
        kimi_model: String,
        /// Reused logged-in Kimi Code session id.
        #[arg(long, default_value = "3c6e42cf-090d-4553-a84b-e63fb9c511c1")]
        kimi_session: String,
        /// Kimi session-resume flag (`-r` for kimi-legacy).
        #[arg(long, default_value = "-r")]
        kimi_session_flag: String,
        /// Extra Kimi args.
        #[arg(long, default_value = "--quiet")]
        kimi_extra_args: String,
    },
}

fn run_harness(cmd: &HarnessCmd) -> Result<()> {
    match cmd {
        HarnessCmd::IdeMergeIde {
            worktree,
            budget,
            max_iters,
            sleep,
            execute,
            safe,
            json,
            agent_cmd,
            model,
            agent_model_args,
            no_kimi_plan,
            no_kimi_review,
            kimi_cmd,
            kimi_model,
            kimi_session,
            kimi_session_flag,
            kimi_extra_args,
        } => {
            let mut opts = harness::IdeMergeIde::with_defaults(worktree.clone());
            opts.budget = *budget;
            opts.max_iters = *max_iters;
            opts.sleep_secs = *sleep;
            opts.execute = *execute;
            opts.apply = !*safe;
            opts.json = *json;
            opts.agent_cmd = agent_cmd.clone();
            opts.model = model.clone();
            opts.agent_model_args = agent_model_args.clone();
            opts.no_kimi_plan = *no_kimi_plan;
            opts.no_kimi_review = *no_kimi_review;
            opts.kimi_cmd = kimi_cmd.clone();
            opts.kimi_model = kimi_model.clone();
            opts.kimi_session = kimi_session.clone();
            opts.kimi_session_flag = kimi_session_flag.clone();
            opts.kimi_extra_args = kimi_extra_args.clone();
            harness::run_ide_merge_ide(opts)
        }
    }
}

/// WL-057: `Some(path)` iff `<home>/.cargo/bin/weave` exists as a file — the
/// canonical stable install location preferred when the running binary is an
/// ephemeral build/worktree path. Uses `$HOME` (the same resolution the setup
/// module uses); `None` if `$HOME` is unset or the file is absent.
fn setup_cargo_bin_weave() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home)
        .join(".cargo")
        .join("bin")
        .join("weave");
    if path.is_file() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// WL-057: resolve a `weave` binary on `$PATH` (a dependency-free which-style scan;
/// no `which` crate). Returns the first `<dir>/weave` that exists as a file whose
/// path is NOT ephemeral (a `target/{debug,release}` or `.worktrees` entry on PATH
/// would just re-introduce the dangle we're avoiding). `None` if none is found.
fn setup_path_weave() -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("weave");
        if candidate.is_file() {
            let s = candidate.to_string_lossy().into_owned();
            if !setup::is_ephemeral_exe(&s) {
                return Some(s);
            }
        }
    }
    None
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load();

    // Commands that don't need the store.
    match &cli.cmd {
        Cmd::Setup {
            provider,
            git_hooks,
            pretooluse,
            exe,
        } => {
            let current_exe = std::env::current_exe()?.to_string_lossy().into_owned();

            // WL-057 (#107): never persist an EPHEMERAL build/worktree path into a
            // global host config — it dangles once that build is gone. Validate an
            // explicit --exe override, then resolve the stable path to persist. The
            // resolver is PURE; we gather the filesystem facts (cargo-bin, $PATH weave)
            // here and pass them in.
            if let Some(p) = exe {
                if !std::path::Path::new(p).is_file() {
                    anyhow::bail!("--exe path does not exist: {p}");
                }
            }
            let cargo_bin = setup_cargo_bin_weave();
            let path_weave = setup_path_weave();
            let choice = setup::resolve_setup_exe(
                &current_exe,
                exe.as_deref(),
                cargo_bin.as_deref(),
                path_weave.as_deref(),
            );
            if let Some(warning) = &choice.warning {
                eprintln!("warning: {warning}");
            }
            let resolved = &choice.path;

            setup::run_provider(resolved, (*provider).into(), *pretooluse)?;
            if *git_hooks {
                setup::install_git_precommit_hook(resolved)?;
            }
            return Ok(());
        }
        Cmd::Uninstall { provider } => return setup::uninstall_provider((*provider).into()),
        #[cfg(feature = "sqlite")]
        Cmd::ProviderSwitch { cmd } => match cmd {
            ProviderSwitchCmd::List { app, db } => {
                let rows = provider_switch::list(db.clone(), *app)?;
                for row in rows {
                    println!(
                        "{}	{}	{}	{}",
                        if row.is_current { "*" } else { " " },
                        row.id,
                        row.name,
                        row.category.as_deref().unwrap_or("")
                    );
                }
                return Ok(());
            }
            ProviderSwitchCmd::Current { app, db } => {
                match provider_switch::current(db.clone(), *app)? {
                    Some(row) => println!("{}	{}", row.id, row.name),
                    None => println!("no current {} provider", app.as_cc_switch()),
                }
                return Ok(());
            }
            ProviderSwitchCmd::Switch {
                app,
                provider_id,
                db,
                dry_run,
            } => {
                let row = provider_switch::switch(db.clone(), *app, provider_id, *dry_run)?;
                if *dry_run {
                    println!(
                        "dry-run: {} provider '{}' ({}) is valid",
                        app.as_cc_switch(),
                        row.id,
                        row.name
                    );
                } else {
                    println!(
                        "switched {} provider to '{}' ({})",
                        app.as_cc_switch(),
                        row.id,
                        row.name
                    );
                    println!("weave lifecycle hooks were preserved where present");
                }
                return Ok(());
            }
            ProviderSwitchCmd::Models { app, db, no_ollama } => {
                let rows = provider_switch::models(db.clone(), *app, !*no_ollama)?;
                for row in rows {
                    println!(
                        "{}	{}	{}	{}	{}",
                        if row.current { "*" } else { " " },
                        row.provider_id,
                        row.provider_name,
                        row.source,
                        row.model
                    );
                }
                return Ok(());
            }
            ProviderSwitchCmd::SwitchModel {
                app,
                provider_id,
                model,
                db,
                dry_run,
            } => {
                let row =
                    provider_switch::switch_model(db.clone(), *app, provider_id, model, *dry_run)?;
                if *dry_run {
                    println!(
                        "dry-run: {} provider '{}' ({}) can switch to model '{}'",
                        app.as_cc_switch(),
                        row.id,
                        row.name,
                        model
                    );
                } else {
                    println!(
                        "switched {} provider '{}' ({}) to model '{}'",
                        app.as_cc_switch(),
                        row.id,
                        row.name,
                        model
                    );
                    if row.is_current {
                        println!("live config refreshed for current provider");
                    }
                }
                return Ok(());
            }
        },
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
        Cmd::Harness { cmd } => return run_harness(cmd),
        // Restore replaces the live store/config — it must NOT open the store first
        // (the on-disk DB may be absent or about to be overwritten). It opens only a
        // verified snapshot of its own.
        Cmd::Restore { in_path, force } => return backup::run_restore(&cfg, in_path, *force),
        _ => {}
    }

    let store = open_store(&cfg)?;
    let store = store.as_ref();

    match cli.cmd {
        #[cfg(feature = "sqlite")]
        Cmd::ProviderSwitch { .. } => {
            unreachable!("handled above")
        }

        Cmd::Setup { .. }
        | Cmd::Uninstall { .. }
        | Cmd::Config { .. }
        | Cmd::Completions { .. }
        | Cmd::Man
        | Cmd::Harness { .. }
        | Cmd::Restore { .. } => {
            unreachable!("handled above")
        }

        Cmd::Backup { out, force } => {
            backup::run_backup(&cfg, store, &out, force)?;
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
                policy: verify_policy(&cfg),
            };
            mcp::serve(
                store,
                def,
                nudge_tpl.as_deref(),
                extra_dbs,
                pull,
                &RealInjector {
                    preferred_mux: parse_mux_preference(&cfg),
                },
            )?;
        }

        Cmd::Send {
            from,
            to,
            subject,
            body,
            to_store,
            to_host,
            no_memory,
            idempotency_key,
            priority,
            supersedes,
            ttl,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let body = maybe_prefix_body(&cfg, &from, &body, no_memory);
            let trace_id = model::mint_trace_id();
            // WL-038: validate the ephemeral TTL once at the CLI seam (the
            // --priority/lease-ttl precedent) before any store write.
            if let Some(t) = ttl {
                if !model::ttl_valid(t) {
                    anyhow::bail!(
                        "--ttl must be between 1 and {} seconds.",
                        model::MAX_MSG_TTL_SECS
                    );
                }
            }
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
                    // WL-037: supersede is a local-inbox concept (the predecessor
                    // lives in THIS store); it has no meaning for a cross-store
                    // intent, so reject rather than silently ignore.
                    if supersedes.is_some() {
                        anyhow::bail!(
                            "--supersedes is not supported with --to-store \
                             (supersede targets a message in this store, not a cross-store intent)."
                        );
                    }
                    let host = to_host.as_deref().unwrap_or("");
                    // Signed identity (2d): if a local signing key is configured,
                    // sign the canonical (from,to,body,created) so the receiver can
                    // verify `from` is unforgeable. `created` is the enqueue time we
                    // bind into the row; we sign the SAME value the store stamps so
                    // verification matches. Without the `sign` feature this is "".
                    let sig = sign_intent_if_keyed(&from, &to, &body);
                    let id = store.enqueue_intent(
                        &to,
                        host,
                        &from,
                        subject.as_deref(),
                        &body,
                        &sig,
                        idempotency_key.as_deref(),
                        Some(&trace_id),
                        priority.as_deref(),
                        ttl.unwrap_or(0),
                    )?;
                    println!("queued intent #{id} for '{to}' @ {store_path} (delivered on their next drain)");
                }
                None => {
                    let mid = store.send(
                        &from,
                        &to,
                        subject.as_deref(),
                        &body,
                        idempotency_key.as_deref(),
                        Some(&trace_id),
                    )?;
                    if let Some(p) = priority {
                        let _ = store.set_message_priority(mid, &p);
                    }
                    // WL-038: post-stamp the ephemeral expiry (the priority post-stamp
                    // precedent). `ttl` is already cap-validated above.
                    if let Some(t) = ttl {
                        let _ =
                            store.set_message_expiry(mid, model::expiry_from_ttl(model::now(), t));
                    }
                    // WL-037: post-stamp the supersede link (the priority post-stamp
                    // precedent). Authorization + id existence are enforced in
                    // `Store::supersede`; a bad id bails with a clear message.
                    if let Some(old) = supersedes {
                        if old <= 0 {
                            anyhow::bail!("--supersedes must be a positive message id.");
                        }
                        store.supersede(&from, old, mid)?;
                        println!("sent #{mid}: {from} -> {to} (supersedes #{old})");
                    } else {
                        println!("sent #{mid}: {from} -> {to}");
                    }
                    let _ = inject_and_trace(
                        store,
                        &cfg,
                        mid,
                        model::DeliveryRefKind::Message,
                        &from,
                        &to,
                        &body,
                    )?;
                    // WL-036: best-effort post-send hooks. Fires AFTER the message is
                    // persisted + injected; a failing/slow hook never sinks the send.
                    inject::fire_post_send_hooks(
                        &cfg,
                        HookEvent::Send,
                        &from,
                        &to,
                        subject.as_deref().unwrap_or(""),
                        mid,
                    );
                }
            }
        }

        Cmd::Notify {
            from,
            to,
            subject,
            body,
            idempotency_key,
            priority,
            dedup_idle,
        } => {
            // Fire-and-forget point-to-point notification. Persist via the normal send
            // path (no fork), fire the SAME caller-side nudge + trace, and print the
            // honest verdict. Broadcast is rejected (point-to-point only).
            if model::is_broadcast(&to) {
                anyhow::bail!(
                    "notify is point-to-point (no reply expected); use `weave send` for broadcast \
                     (broadcast notify is deferred)."
                );
            }
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let trace_id = model::mint_trace_id();
            let mid = store.send(
                &from,
                &to,
                subject.as_deref(),
                &body,
                idempotency_key.as_deref(),
                Some(&trace_id),
            )?;
            if let Some(p) = priority {
                let _ = store.set_message_priority(mid, &p);
            }
            // WL-039: opt-in idle-notification dedup. Stamp this ping idle and
            // auto-supersede this sender's prior UNREAD idle pings to `to` so a
            // pile of "still waiting" pings collapses to just the latest. Post-send
            // (mirrors the WL-037 `--supersedes` post-stamp); best-effort — a dedup
            // failure must not sink the notify the recipient already received.
            if dedup_idle {
                let _ = store.supersede_prior_idle(&from, &to, mid);
            }
            // Trace + nudge (best-effort trace, no store→inject edge). The honest
            // verdict is derived from the SAME inject result that drove the trace, so
            // the printed token and the recorded stage can never disagree.
            let verdict = inject_and_trace(
                store,
                &cfg,
                mid,
                model::DeliveryRefKind::Notify,
                &from,
                &to,
                &body,
            )?;
            println!("notified '{to}' (#{mid}, no reply expected) [{verdict}]");
            // WL-036: notify is a point-to-point send ⇒ fire `Send` hooks.
            inject::fire_post_send_hooks(
                &cfg,
                HookEvent::Send,
                &from,
                &to,
                subject.as_deref().unwrap_or(""),
                mid,
            );
        }

        Cmd::BroadcastNotify {
            from,
            subject,
            body,
            circle,
            json,
            priority,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let target_circle = resolve_list_circle(store, &cfg, &from, circle.as_deref(), false);
            let peers = store.list_peers()?;
            let online: Vec<_> = peers
                .into_iter()
                .filter(|p| {
                    target_circle
                        .as_ref()
                        .map(|c| model::circle_or_default(&p.circle) == c)
                        .unwrap_or(true)
                })
                .filter(|p| p.name != from)
                .filter(store::is_alive)
                .map(|p| p.name)
                .collect();
            if online.is_empty() {
                if json {
                    println!("{}", serde_json::json!({ "notified": 0, "peers": [] }));
                } else {
                    println!("broadcast-notify: no online peers in circle");
                }
            } else {
                let mut notified = 0usize;
                let mut child_json = Vec::new();
                for peer in &online {
                    let trace_id = model::mint_trace_id();
                    let mid = store.send(
                        &from,
                        peer,
                        subject.as_deref(),
                        &body,
                        None,
                        Some(&trace_id),
                    )?;
                    if let Some(p) = priority.as_ref() {
                        let _ = store.set_message_priority(mid, p);
                    }
                    let verdict = inject_and_trace(
                        store,
                        &cfg,
                        mid,
                        model::DeliveryRefKind::Notify,
                        &from,
                        peer,
                        &body,
                    )?;
                    notified += 1;
                    if json {
                        child_json.push(serde_json::json!({
                            "peer": peer, "message_id": mid, "verdict": verdict
                        }));
                    } else {
                        println!("  {peer}: #{mid} [{verdict}]");
                    }
                }
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "notified": notified,
                            "peers": child_json,
                        }))?
                    );
                } else {
                    println!("broadcast-notify: {notified} peer(s) notified");
                }
            }
        }

        Cmd::BroadcastAsk {
            from,
            subject,
            body,
            circle,
            reply_to: _,
            json,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let target_circle = resolve_list_circle(store, &cfg, &from, circle.as_deref(), false);
            let peers = store.list_peers()?;
            let online: Vec<String> = peers
                .into_iter()
                .filter(|p| {
                    target_circle
                        .as_ref()
                        .map(|c| model::circle_or_default(&p.circle) == c)
                        .unwrap_or(true)
                })
                .filter(|p| p.name != from)
                .filter(store::is_alive)
                .map(|p| p.name)
                .collect();
            if online.is_empty() {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "parent_id": null, "created": 0, "peers": [] })
                    );
                } else {
                    println!("broadcast-ask: no online peers in circle");
                }
            } else {
                let outcome = store.create_ask_many(&from, &online, subject.as_deref(), &body)?;
                let mut child_json = Vec::new();
                let mut created = 0usize;
                let mut failed = 0usize;
                for (peer, res) in &outcome.children {
                    match res {
                        Ok(cid) => {
                            created += 1;
                            let verdict = ask_inject_verdict(store, &cfg, &from, peer, &body);
                            if json {
                                child_json.push(serde_json::json!({
                                    "peer": peer, "correlation_id": cid, "verdict": verdict
                                }));
                            } else {
                                println!("  {peer}: {cid} ({verdict})");
                            }
                        }
                        Err(err) => {
                            failed += 1;
                            if json {
                                child_json.push(serde_json::json!({
                                    "peer": peer, "error": err
                                }));
                            } else {
                                println!("  {peer}: FAILED ({err})");
                            }
                        }
                    }
                }
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "parent_id": outcome.parent_id,
                            "created": created,
                            "failed": failed,
                            "peers": child_json,
                        }))?
                    );
                } else {
                    println!(
                        "broadcast-ask {}: {created} created, {failed} failed",
                        outcome.parent_id
                    );
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
            let pulled = store::pull_from_store(store, &me, &allow, &verify_policy(&cfg))?;
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
            no_memory,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let body = maybe_prefix_body(&cfg, &from, &body, no_memory);
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

        Cmd::Ask {
            to,
            body,
            subject,
            kind,
            options,
            reply_to,
            from,
            no_memory,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            if model::is_broadcast(&to) {
                anyhow::bail!(
                    "tracked ask is point-to-point; use `weave send` for broadcast (broadcast ask is P2)."
                );
            }
            let body = maybe_prefix_body(&cfg, &from, &body, no_memory);
            let ask_kind = kind
                .as_deref()
                .map(model::AskKind::parse)
                .unwrap_or_default();
            let (cid, _qid) = store.ask(
                &from,
                &to,
                subject.as_deref(),
                &body,
                ask_kind,
                options.as_deref(),
                reply_to.as_deref(),
            )?;
            // Honest delivery verdict via the caller-side nudge (no store->inject edge).
            let verdict = ask_inject_verdict(store, &cfg, &from, &to, &body);
            println!("opened ask {cid}: {from} -> {to} ({verdict})");
        }

        Cmd::Answer {
            id,
            in_reply_to,
            body,
            from,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            // Resolve the correlation id from --id or --in-reply-to (a message id).
            let cid = match (id, in_reply_to) {
                (Some(cid), _) => cid,
                (None, Some(mid)) => store
                    .ask_for_message(mid)?
                    .ok_or_else(|| anyhow::anyhow!("message #{mid} belongs to no tracked ask"))?,
                (None, None) => anyhow::bail!("provide either --id or --in-reply-to"),
            };
            let ask = store
                .get_ask(&cid)?
                .ok_or_else(|| anyhow::anyhow!("no tracked ask '{cid}'"))?;
            let asker = ask.asker.clone();
            store.answer(&from, &cid, &body)?;
            let verdict = ask_inject_verdict(store, &cfg, &from, &asker, &body);
            println!("answered ask {cid} -> {asker} ({verdict})");
        }

        Cmd::Ack { id, message, from } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            store.ack(&from, &id, message.as_deref())?;
            println!("closed ask {id} (acked)");
            // WL-036: fire `Ack` hooks post-state-change. The acker is the sender; the
            // original asker (who learns of the ack) is the recipient. Best-effort
            // lookup — a miss just yields an empty recipient (a `*` hook still fires).
            let asker = store
                .get_ask(&id)
                .ok()
                .flatten()
                .map(|a| a.asker)
                .unwrap_or_default();
            inject::fire_post_send_hooks(&cfg, HookEvent::Ack, &from, &asker, &id, 0);
        }

        Cmd::Asks {
            me,
            role,
            limit,
            json,
        } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            let asks = store.list_asks(&me, model::AskRole::parse(&role), limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "asks": asks }))?
                );
            } else if asks.is_empty() {
                println!("no tracked asks");
            } else {
                for a in &asks {
                    let subj = a
                        .subject
                        .as_ref()
                        .map(|s| format!(" | {s}"))
                        .unwrap_or_default();
                    println!(
                        "{} [{}] {} -> {}{} ({})",
                        a.id,
                        a.state.as_str(),
                        a.asker,
                        a.askee,
                        subj,
                        model::fmt_ts(a.opened_ts)
                    );
                }
            }
        }

        Cmd::AskGet { id, json } => {
            let ask = store.get_ask(&id)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "ask": ask }))?
                );
            } else {
                match ask {
                    None => println!("no tracked ask '{id}'"),
                    Some(a) => {
                        let answered = if a.answer_msg_id.is_some() {
                            " (answered)"
                        } else {
                            ""
                        };
                        println!(
                            "{} [{}] {} -> {}{}{}",
                            a.id,
                            a.state.as_str(),
                            a.asker,
                            a.askee,
                            a.subject
                                .as_ref()
                                .map(|s| format!(" | {s}"))
                                .unwrap_or_default(),
                            answered
                        );
                    }
                }
            }
        }

        Cmd::AskMany {
            to,
            body,
            subject,
            from,
            json,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            let outcome = store.create_ask_many(&from, &to, subject.as_deref(), &body)?;
            // Honest per-child delivery verdict via the caller-side nudge (no
            // store->inject edge), one per CREATED child.
            let mut child_json = Vec::new();
            let mut created = 0usize;
            let mut failed = 0usize;
            for (peer, res) in &outcome.children {
                match res {
                    Ok(cid) => {
                        created += 1;
                        let verdict = ask_inject_verdict(store, &cfg, &from, peer, &body);
                        if json {
                            child_json.push(serde_json::json!({
                                "peer": peer, "correlation_id": cid, "verdict": verdict
                            }));
                        } else {
                            println!("  {peer}: {cid} ({verdict})");
                        }
                    }
                    Err(err) => {
                        failed += 1;
                        if json {
                            child_json.push(serde_json::json!({
                                "peer": peer, "error": err
                            }));
                        } else {
                            println!("  {peer}: FAILED ({err})");
                        }
                    }
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "parent_id": outcome.parent_id,
                        "created": created,
                        "failed": failed,
                        "children": child_json,
                    }))?
                );
            } else {
                println!(
                    "opened ask-many {}: {created} created, {failed} failed",
                    outcome.parent_id
                );
            }
        }

        Cmd::AskManyResult {
            parent_id,
            age,
            json,
        } => {
            let r = store.ask_many_result(&parent_id, age)?;
            match r {
                None => {
                    if json {
                        println!("{}", serde_json::json!({ "result": null }));
                    } else {
                        anyhow::bail!("no ask-many '{parent_id}'");
                    }
                }
                Some(r) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({ "result": r }))?
                        );
                    } else {
                        println!(
                            "ask-many {} [{}] from {}: {}/{} answered, {} acked, {} pending, {} failed ({})",
                            r.parent_id,
                            r.state.as_str(),
                            r.asker,
                            r.answered,
                            r.target_count,
                            r.acked,
                            r.pending,
                            r.failed,
                            model::fmt_ts(r.opened_ts)
                        );
                        for c in &r.children {
                            let state = c.state.map(|s| s.as_str()).unwrap_or("failed");
                            let cid = c.correlation_id.as_deref().unwrap_or("-");
                            let ans = c
                                .answer_msg_id
                                .map(|m| format!(" answer=#{m}"))
                                .unwrap_or_default();
                            println!("  {} [{state}] {cid}{ans}", c.peer);
                        }
                    }
                }
            }
        }

        Cmd::Job { cmd } => dispatch_job(store, &cfg, cmd)?,

        Cmd::Orchestrator { cmd } => dispatch_orchestrator(store, &cfg, cmd)?,

        Cmd::Thread {
            root,
            limit,
            json,
            summarize: _summarize,
            refresh: _refresh,
        } => {
            let rows = store.thread(root, limit)?;
            #[cfg(feature = "llm")]
            if _summarize {
                let summary = if _refresh {
                    None
                } else {
                    store.get_summary(root)?
                };
                let text = match summary {
                    Some(s) => s.text,
                    None => {
                        let thread_text = rows
                            .iter()
                            .map(|m| format!("{}: {}", m.sender, m.body))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let sum = weave_core::llm::summarize_text(&cfg, &thread_text)?;
                        store.store_summary(
                            root,
                            &sum,
                            cfg.llm_model.as_deref().unwrap_or("unknown"),
                        )?;
                        sum
                    }
                };
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "root": root, "summary": text
                        }))?
                    );
                } else {
                    println!("thread #{root} summary:\n{text}");
                }
                return Ok(());
            }
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

        Cmd::Summarize { text, json } => {
            #[cfg(feature = "llm")]
            {
                let input = match text {
                    Some(t) => t,
                    None => {
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf)?;
                        buf
                    }
                };
                let sum = weave_core::llm::summarize_text(&cfg, &input)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "summary": sum
                        }))?
                    );
                } else {
                    println!("{sum}");
                }
            }
            #[cfg(not(feature = "llm"))]
            {
                let _ = text;
                let _ = json;
                anyhow::bail!("weave was compiled without the llm feature");
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

        Cmd::Delivery { id, json } => {
            // Transport trace (queued -> injected/inject_failed/not_injectable ->
            // drained). Read-only, metadata-only — no body. An unknown id is the
            // empty-trace line, not an error.
            let trace = store.list_delivery(id, model::MAX_DELIVERY_ROWS)?;
            if json {
                let arr: Vec<_> = trace
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "ts": t.ts,
                            "stage": t.stage,
                            "outcome": t.outcome,
                            "to_peer": t.to_peer,
                            "ref_kind": t.ref_kind,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "message_id": id, "delivery": arr
                    }))?
                );
            } else if trace.is_empty() {
                println!("#{id}: no delivery trace");
            } else {
                println!("#{id} delivery trace:");
                for t in &trace {
                    println!(
                        "  [{}] {}/{} -> {} ({})",
                        model::fmt_ts(t.ts),
                        t.stage,
                        t.outcome,
                        t.to_peer,
                        t.ref_kind
                    );
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

        Cmd::Search { query, limit, json } => {
            let rows = store.search(&query, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "query": query, "messages": rows
                    }))?
                );
            } else if rows.is_empty() {
                println!("search '{query}': no matches");
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
            }
        }

        Cmd::Export { out, for_id, limit } => {
            let (me, explicit) = resolve_me_explicit(for_id, None, &cfg);
            // Honor the identity cap / control-char rejection on the resolved id
            // before it reaches the store query.
            weave_core::store::check_ident("identity", &me)?;
            refresh_presence(store, &me, explicit);
            let limit = limit.unwrap_or(10_000) as i64;
            let rows = store.history(&me, None, limit)?;
            let html = weave_core::export::render_mailbox_html(&rows);
            std::fs::write(&out, html)
                .with_context(|| format!("failed to write export to {}", out.display()))?;
            println!(
                "exported {} message(s) for '{me}' -> {}",
                rows.len(),
                out.display()
            );
        }

        Cmd::Session { cmd } => match cmd {
            SessionCmd::Export {
                out,
                for_id,
                limit,
                force,
            } => {
                let (me, explicit) = resolve_me_explicit(for_id, None, &cfg);
                refresh_presence(store, &me, explicit);
                let limit = limit.unwrap_or(10_000) as i64;
                session::run_export(&cfg, store, &out, &me, limit, force)?;
            }
            SessionCmd::Import {
                in_path,
                as_id,
                dry_run,
            } => {
                let (me, _explicit) = resolve_me_explicit(as_id, None, &cfg);
                session::run_import(&cfg, store, &in_path, &me, dry_run)?;
            }
        },

        Cmd::Peers {
            json,
            circle,
            all_circles,
        } => {
            // A1 heartbeat-on-read: listing peers is a cheap, frequently-hit path,
            // so use it to keep our own `last_seen` warm. Best-effort and explicit-
            // identity-only (refresh_presence guards both): a heartbeat write failure
            // must never abort the read.
            let (me, explicit) = resolve_me_explicit(None, None, &cfg);
            refresh_presence(store, &me, explicit);
            // P4 circle scope: resolve the effective circle (explicit flag wins,
            // then --all-circles, then an orchestrator caller goes mesh-wide, else
            // the caller's own circle). `None` ⇒ no filter (mesh-wide).
            let effective = resolve_list_circle(store, &cfg, &me, circle.as_deref(), all_circles);
            // Tier-1 federation: union the local peers with any configured
            // read-only extra stores, origin-tagged. Default (no WEAVE_PEER_DBS /
            // [federation] peer_dbs) ⇒ `extra` is empty ⇒ output is the local
            // listing tagged `local`, byte-identical to single-store behavior.
            let extra = cfg.peer_db_sources();
            let mut views = store::federated_peers(store, &extra)?;
            // Caller-side circle filter over the merged views (federation
            // composes). With everyone in "default" and no flag, this keeps every
            // row ⇒ byte-identical to pre-P4.
            if let Some(target) = effective.as_deref() {
                views.retain(|v| model::circle_or_default(&v.peer.circle) == target);
            }
            // Host-aware liveness reason per peer (A2 vocabulary, display-only):
            // reinterpret the already-pulled read-only rows; never a cross-machine
            // probe. Deterministic given the captured this_host/now.
            let this_host = config::this_host();
            let now_ts = model::now();
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|v| {
                        let p = &v.peer;
                        let liveness = store::liveness_for(p, &this_host, now_ts);
                        serde_json::json!({
                            "name": p.name, "mux": p.mux, "target": p.target,
                            "socket": p.socket, "cwd": p.cwd,
                            "last_seen": p.last_seen,
                            "pid": p.pid, "host": p.host,
                            "repo": p.repo, "branch": p.branch, "worktree": p.worktree_id,
                            "circle": model::circle_or_default(&p.circle),
                            "role": p.role,
                            "turn_state": model::TurnState::from_str(&p.turn_state)
                                .unwrap_or_default()
                                .as_str(),
                            "description": p.description,
                            "description_ts": p.description_ts,
                            "online": is_alive(p),
                            "alive": is_alive(p),
                            "liveness": liveness.token(),
                            "remote": p.host != this_host,
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
                    let liveness = store::liveness_for(p, &this_host, now_ts);
                    let reason = scan_liveness_reason(p, liveness);
                    let remote_marker = if p.host != this_host { " <remote>" } else { "" };
                    let tgt = if p.target.is_empty() { "-" } else { &p.target };
                    let via = if v.origin.is_foreign() {
                        format!(" (via {})", v.origin.label())
                    } else {
                        String::new()
                    };
                    let tags = fmt_peer_tags(p);
                    let ts_marker = fmt_turn_state(p);
                    let desc = fmt_description(p);
                    println!(
                        "{}{remote_marker} [{presence}] [{reason}]{ts_marker} [{}] {} ({inj}){tags}{desc} seen {}{via}",
                        p.name,
                        p.mux,
                        tgt,
                        model::fmt_ts(p.last_seen)
                    );
                }
            }
        }

        Cmd::Sessions {
            json,
            watch,
            interval,
            iterations,
            repo,
            branch,
            circle,
            all_circles,
        } if watch => {
            // Presence dashboard (read-only). Groups federated PEER rows (which carry
            // repo/branch/worktree + liveness, unlike SessionView) by repo→branch.
            // The loop writes NOTHING per tick; at most one pre-loop owner self-
            // refresh (mirroring `scan`) so the watcher's own row shows current.
            let (me, explicit) = resolve_me_explicit(None, None, &cfg);
            if explicit {
                let t = inject::detect_target_with_preference(parse_mux_preference(&cfg));
                let cwd_val = std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                let tags = git_tags_for(cwd_val.as_deref());
                if let Err(e) = store.register_peer_full(
                    &me,
                    t.mux.as_str(),
                    &t.id,
                    &t.socket,
                    cwd_val.as_deref(),
                    Some(std::process::id() as i64),
                    &config::this_host(),
                    &tags.repo,
                    &tags.branch,
                    &tags.worktree_id,
                    &cfg.circle(),
                    None,
                ) {
                    eprintln!("[weave] sessions watch self-refresh skipped (non-fatal): {e}");
                }
            }
            let extra = cfg.peer_db_sources();
            // P4 circle scope (resolved once; the snapshot closure applies it).
            let effective = resolve_list_circle(store, &cfg, &me, circle.as_deref(), all_circles);

            // Build one snapshot (read-only) from federated peers, applying the same
            // exact-match repo/branch filters as `scan`. Closure so the loop re-reads
            // fresh data each tick without re-applying filter parsing.
            let snapshot = |store: &dyn Store| -> Result<Vec<SessionRow>> {
                let mut views = store::federated_peers(store, &extra)?;
                if let Some(r) = repo.as_deref() {
                    views.retain(|v| v.peer.repo == r);
                }
                if let Some(b) = branch.as_deref() {
                    views.retain(|v| v.peer.branch == b);
                }
                if let Some(target) = effective.as_deref() {
                    views.retain(|v| model::circle_or_default(&v.peer.circle) == target);
                }
                Ok(dashboard_rows(&views))
            };

            // Host-aware liveness reason vocabulary (A2) is computed deterministically
            // from the captured this_host/now — never a wall-clock per row.
            let this_host = config::this_host();
            if json {
                // `--watch --json` ⇒ a SINGLE snapshot then exit (no clear, no loop):
                // a stream of cleared frames is not machine-consumable. Same shape as
                // the non-watch scan JSON (presence-focused).
                let now_ts = model::now();
                let rows = snapshot(store)?;
                let arr: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        let liveness = row_liveness(r, &this_host, now_ts);
                        serde_json::json!({
                            "name": r.name, "repo": r.repo, "branch": r.branch,
                            "worktree": r.worktree, "mux": r.mux, "host": r.host,
                            "alive": !matches!(liveness, store::Liveness::Stale),
                            "liveness": liveness.token(),
                            "remote": r.host != this_host,
                            "via": r.via,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                let opts = DashboardOpts {
                    clear: clear_enabled(),
                    repo: repo.clone(),
                    branch: branch.clone(),
                };
                let interval = config::clamp_watch_interval(interval);
                eprintln!("[weave] watching sessions every {interval}s (Ctrl-C to stop)");
                // Bounded by ITERATION COUNT (never a wall-clock assertion): `0` ⇒
                // loop forever (interactive); `n>0` ⇒ render exactly n frames. Sleep
                // happens BETWEEN frames, never after the last, so `--iterations 1`
                // returns immediately after one render (no trailing wait, no hang).
                let mut n: u64 = 0;
                loop {
                    let rows = snapshot(store)?;
                    print!(
                        "{}",
                        render_sessions_dashboard(&rows, &opts, &this_host, model::now())
                    );
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    n += 1;
                    if iterations != 0 && n >= iterations {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(interval));
                }
            }
        }

        Cmd::Sessions {
            json,
            circle,
            all_circles,
            ..
        } => {
            // Tier-1 federation: union local sessions with read-only extra stores,
            // origin-tagged. Foreign sessions are kept distinct (no unread summing —
            // Tier 1 has no cross-store inbox). Default ⇒ identical-to-today.
            let extra = cfg.peer_db_sources();
            let mut views = store::federated_sessions(store, &extra)?;
            // Display-layer tag join (purely additive, no schema/trait/federation
            // change): SessionView is message-derived and carries no git tags, so we
            // look up the LOCAL peer by session name and attach repo/branch/worktree
            // for display only. Only the local store's peers are consulted (never
            // foreign rows), and a session without a registered peer shows `-`/empty.
            let local_peers = local_peer_tag_map(store);
            // P4 circle scope: a session's circle is its registered local peer's
            // circle (a session with no peer row classifies as "default" via
            // circle_or_default). With everyone in "default" and no flag this keeps
            // every row ⇒ byte-identical to pre-P4.
            let (me, _) = resolve_me_explicit(None, None, &cfg);
            let effective = resolve_list_circle(store, &cfg, &me, circle.as_deref(), all_circles);
            if let Some(target) = effective.as_deref() {
                views.retain(|v| {
                    let c = local_peers
                        .get(&v.name)
                        .map(|p| p.circle.as_str())
                        .unwrap_or("");
                    model::circle_or_default(c) == target
                });
            }
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|v| {
                        let (repo, branch, worktree) = local_peers
                            .get(&v.name)
                            .map(|p| (p.repo.clone(), p.branch.clone(), p.worktree_id.clone()))
                            .unwrap_or_default();
                        let (circle, role) = local_peers
                            .get(&v.name)
                            .map(|p| {
                                (
                                    model::circle_or_default(&p.circle).to_string(),
                                    p.role.clone(),
                                )
                            })
                            .unwrap_or_else(|| (model::DEFAULT_CIRCLE.to_string(), String::new()));
                        serde_json::json!({
                            "name": v.name, "unread": v.unread, "last_activity": v.last_activity,
                            "repo": repo, "branch": branch, "worktree": worktree,
                            "circle": circle, "role": role,
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
                    let tags = local_peers
                        .get(&v.name)
                        .map(fmt_peer_tags)
                        .unwrap_or_default();
                    println!(
                        "{}: {} unread (last {}){tags}{via}",
                        v.name,
                        v.unread,
                        model::fmt_ts(v.last_activity)
                    );
                }
            }
        }

        Cmd::Scan {
            repo,
            branch,
            json,
            circle,
            all_circles,
        } => {
            // Owner-only-writes: refresh ONLY the caller's own row (re-capture this
            // session's git tags + presence and upsert under our own identity),
            // exactly like attach. We never re-register a foreign/federated row.
            // Best-effort: a heartbeat/tag refresh failure must not sink the read.
            let (me, explicit) = resolve_me_explicit(None, None, &cfg);
            if explicit {
                let t = inject::detect_target_with_preference(parse_mux_preference(&cfg));
                let cwd_val = std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned());
                let tags = git_tags_for(cwd_val.as_deref());
                if let Err(e) = store.register_peer_full(
                    &me,
                    t.mux.as_str(),
                    &t.id,
                    &t.socket,
                    cwd_val.as_deref(),
                    Some(std::process::id() as i64),
                    &config::this_host(),
                    &tags.repo,
                    &tags.branch,
                    &tags.worktree_id,
                    &cfg.circle(),
                    None,
                ) {
                    eprintln!("[weave] scan self-refresh skipped (non-fatal): {e}");
                }
            }
            // Enumerate federated peers (local + read-only extra stores), read-only.
            let extra = cfg.peer_db_sources();
            let mut views = store::federated_peers(store, &extra)?;
            // Apply the optional repo/branch filters (exact match on the tag).
            if let Some(r) = repo.as_deref() {
                views.retain(|v| v.peer.repo == r);
            }
            if let Some(b) = branch.as_deref() {
                views.retain(|v| v.peer.branch == b);
            }
            // P4 circle scope (caller-side filter; federation composes). With
            // everyone in "default" and no flag this keeps every row.
            let effective = resolve_list_circle(store, &cfg, &me, circle.as_deref(), all_circles);
            if let Some(target) = effective.as_deref() {
                views.retain(|v| model::circle_or_default(&v.peer.circle) == target);
            }
            // Host-aware liveness reason per row (pure A2 reinterpretation of the
            // already-pulled read-only rows; never a cross-machine probe).
            let this_host = config::this_host();
            let now_ts = model::now();
            if json {
                let arr: Vec<_> = views
                    .iter()
                    .map(|v| {
                        let p = &v.peer;
                        let liveness = store::liveness_for(p, &this_host, now_ts);
                        serde_json::json!({
                            "name": p.name,
                            "repo": p.repo,
                            "branch": p.branch,
                            "worktree": p.worktree_id,
                            "mux": p.mux,
                            "pane": p.target,
                            "host": p.host,
                            "circle": model::circle_or_default(&p.circle),
                            "role": p.role,
                            "turn_state": model::TurnState::from_str(&p.turn_state)
                                .unwrap_or_default()
                                .as_str(),
                            "description": p.description,
                            "description_ts": p.description_ts,
                            "alive": is_alive(p),
                            "liveness": liveness.token(),
                            "remote": p.host != this_host,
                            "origin": v.origin.label(),
                            "foreign": v.origin.is_foreign(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                if views.is_empty() {
                    println!("no peers match the scan");
                }
                let mut local_alive = 0usize;
                let mut remote_alive = 0usize;
                let mut stale = 0usize;
                for v in &views {
                    let p = &v.peer;
                    let liveness = store::liveness_for(p, &this_host, now_ts);
                    let reason = scan_liveness_reason(p, liveness);
                    match liveness {
                        store::Liveness::AliveLocal => local_alive += 1,
                        store::Liveness::AliveRemote => remote_alive += 1,
                        store::Liveness::Stale => stale += 1,
                    }
                    let remote_marker = if p.host != this_host { " <remote>" } else { "" };
                    let repo = if p.repo.is_empty() { "-" } else { &p.repo };
                    let branch = if p.branch.is_empty() { "-" } else { &p.branch };
                    let wt = if p.worktree_id.is_empty() {
                        "-"
                    } else {
                        &p.worktree_id
                    };
                    let pane = if p.target.is_empty() { "-" } else { &p.target };
                    let host = if p.host.is_empty() { "-" } else { &p.host };
                    let via = if v.origin.is_foreign() {
                        format!(" (via {})", v.origin.label())
                    } else {
                        String::new()
                    };
                    let ts_marker = fmt_turn_state(p);
                    let desc = fmt_description(p);
                    println!(
                        "{}{remote_marker} [{reason}]{ts_marker} repo={repo} branch={branch} worktree={wt} mux={} pane={pane} host={host}{desc}{via}",
                        p.name, p.mux
                    );
                }
                if !views.is_empty() {
                    println!(
                        "summary: {local_alive} local-alive, {remote_alive} remote-alive, {stale} stale"
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
            let t = inject::detect_target_with_preference(parse_mux_preference(&cfg));
            let cwd_val = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            // Persist the captured kitty control socket (KITTY_LISTEN_ON; empty for
            // every other backend) so a remote sender can reach a `--listen-on`
            // kitty via `kitten --to <socket>` without re-detecting it. Capture
            // this process's PID + host so presence reflects real liveness. Tag the
            // session with its repo/branch/worktree id (best-effort from cwd; a git
            // failure never sinks registration — empty tags result).
            let tags = git_tags_for(cwd_val.as_deref());
            let cert = store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd_val.as_deref(),
                Some(std::process::id() as i64),
                &config::this_host(),
                &tags.repo,
                &tags.branch,
                &tags.worktree_id,
                &cfg.circle(),
                None,
            )?;
            let tgt = if t.id.is_empty() {
                "-".to_string()
            } else {
                t.id.clone()
            };
            println!(
                "registered '{me}' [{}] {} (save birth-cert: {cert})",
                t.mux.as_str(),
                tgt
            );
        }

        Cmd::Attach { name, cwd, cert } => {
            // Bind the row key to OUR OWN resolved identity — attach upserts the
            // caller's own peer row only, never an arg-supplied foreign target.
            let me = resolve_me(name, cwd.as_deref(), &cfg);
            // Validate identity up front (the store also enforces this, but failing
            // here keeps the error close to the input).
            store::check_ident("name", &me)?;
            let t = inject::detect_target_with_preference(parse_mux_preference(&cfg));
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
            // liveness (the whole point of zero-restart attach), plus the git tags.
            let tags = git_tags_for(cwd_val.as_deref());
            // If no --cert provided, try to reuse the stored cert so re-attach is
            // seamless for the peer owner (the common case).
            let stored_cert = store.get_birth_cert(&me)?;
            let cert = cert.as_deref().or(stored_cert.as_deref());
            let cert = store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd_val.as_deref(),
                Some(std::process::id() as i64),
                &config::this_host(),
                &tags.repo,
                &tags.branch,
                &tags.worktree_id,
                &cfg.circle(),
                cert,
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
            println!(
                "attached '{me}' [{}] {tgt} ({inj}) (birth-cert: {cert})",
                t.mux.as_str()
            );
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

        Cmd::Spawn {
            name,
            cmd,
            cwd,
            mux,
            window,
        } => {
            // The spawned agent mints a fresh identity; refuse to clobber a live one.
            store::check_ident("name", &name)?;
            if store.get_peer(&name)?.is_some() {
                anyhow::bail!(
                    "a peer named '{name}' is already registered; choose a fresh name for the spawned agent"
                );
            }
            // Resolve the working directory (default: the current dir).
            let cwd = match cwd {
                Some(c) if !c.trim().is_empty() => c,
                _ => std::env::current_dir()?.to_string_lossy().into_owned(),
            };
            // Spawn allowlist: operator-local CLI WARNS-but-proceeds when no allowlist
            // is configured (the operator already has a local shell), and HARD-DENIES
            // only when an allowlist IS set and this cwd is outside it. (The MCP/remote
            // surface denies by default — see tool_spawn_peer.)
            match cfg.spawn_allowed_dirs.as_deref() {
                Some(dirs) if !dirs.is_empty() => {
                    if !cfg.spawn_dir_allowed(std::path::Path::new(&cwd)) {
                        anyhow::bail!(
                            "refusing to spawn into {cwd:?}: not under a configured spawn_allowed_dirs"
                        );
                    }
                }
                _ => {
                    eprintln!(
                        "[weave] warning: no spawn_allowed_dirs configured; spawning into {cwd:?} (operator-local)"
                    );
                }
            }
            // Resolve the target mux: explicit override, else auto-detect this pane's.
            let mux = match mux {
                Some(s) if !s.trim().is_empty() => inject::Mux::parse(s.trim()),
                _ => inject::detect_target_with_preference(parse_mux_preference(&cfg)).mux,
            };
            if mux == inject::Mux::None {
                anyhow::bail!(
                    "no multiplexer detected to spawn into (run inside tmux/zellij/kitty/wezterm/screen, or pass --mux)"
                );
            }
            // Mint the birth cert in the parent (pure, no row) and thread it into the
            // child env; register the peer only when the mux echoes a target id.
            let cert = store::mint_birth_cert()?;
            let circle = cfg.circle();
            let outcome = inject::spawn_child(mux, &cwd, &name, &cert, &circle, &cmd, window)?;
            if !outcome.target.is_empty() && inject::id_valid(mux, &outcome.target) {
                store.register_peer_full(
                    &name,
                    mux.as_str(),
                    &outcome.target,
                    "",
                    Some(cwd.as_str()),
                    None,
                    "",
                    "",
                    "",
                    "",
                    &circle,
                    Some(cert.as_str()),
                )?;
            }
            let tgt = if outcome.target.is_empty() {
                "(self-registers on first hook)".to_string()
            } else {
                outcome.target.clone()
            };
            println!(
                "spawned '{name}' into {} {tgt} (cwd={cwd})\nsave birth-cert: {cert}",
                mux.as_str()
            );
        }

        Cmd::Kill { name } => {
            let peer = store
                .get_peer(&name)?
                .ok_or_else(|| anyhow::anyhow!("no registered peer '{name}'"))?;
            let t = inject::Target::from_peer(&peer);
            if matches!(t.mux, inject::Mux::ITerm2 | inject::Mux::None) {
                println!(
                    "peer '{name}' is on {} — kill not supported for that backend",
                    t.mux.as_str()
                );
            } else if !inject::id_valid(t.mux, &t.id) {
                anyhow::bail!(
                    "refusing to kill: peer '{name}' has an invalid {} target {:?}",
                    t.mux.as_str(),
                    t.id
                );
            } else {
                let killed = inject::kill_target(&t)?;
                if killed {
                    println!("killed '{name}' on {} (target {})", t.mux.as_str(), t.id);
                } else {
                    // iTerm2/None are handled above, so this is a supported backend
                    // whose kill command ran but reported failure: the pane/session
                    // is likely already gone or the mux server is unreachable (e.g. a
                    // non-default tmux socket). Report honestly, never a false "killed".
                    println!(
                        "could not confirm kill of '{name}' on {} (target {}) — the pane/session may already be gone or unreachable",
                        t.mux.as_str(),
                        t.id
                    );
                }
            }
        }

        #[cfg(feature = "sign")]
        Cmd::Key { cmd } => handle_key(store, &cfg, cmd)?,

        #[cfg(feature = "sign")]
        Cmd::Audit { cmd } => handle_audit(store, cmd)?,

        Cmd::Describe { text, me } => {
            // Self-only: bind the row key to OUR OWN resolved identity, never an
            // arg-supplied foreign target (the Attach precedent). The store
            // control-strips + caps the text.
            let me = resolve_me(me, None, &cfg);
            store::check_ident("name", &me)?;
            store.set_description(&me, &text)?;
            // Echo the stored (post-sanitize, post-TTL) view back.
            let shown = store
                .get_peer(&me)?
                .map(|p| p.description)
                .unwrap_or_default();
            if shown.is_empty() {
                println!("description cleared for '{me}'");
            } else {
                println!("description set for '{me}': {shown}");
            }
        }

        Cmd::Status { state, me } => {
            // Self-only; the store validates the state against the TurnState enum
            // (an unknown value is a hard error).
            let me = resolve_me(me, None, &cfg);
            store::check_ident("name", &me)?;
            store.set_turn_state(&me, &state)?;
            println!("turn_state set for '{me}': {state}");
        }

        Cmd::PeerPolicy { name, policy } => {
            store::check_ident("name", &name)?;
            if let Some(p) = policy {
                let parsed = crate::model::ContactPolicy::parse(&p);
                store.set_peer_policy(&name, parsed.as_str())?;
                println!("contact_policy set for '{name}': {}", parsed.as_str());
            } else {
                match store.get_peer_policy(&name)? {
                    Some(p) => println!("{p}"),
                    None => println!("(no peer '{name}' found)"),
                }
            }
        }

        Cmd::Memory { cmd } => dispatch_memory(&cfg, cmd)?,

        Cmd::Review { cmd } => dispatch_review(store, &cfg, cmd)?,

        Cmd::Permission { cmd } => dispatch_permission(store, &cfg, cmd)?,

        Cmd::Lease { cmd } => dispatch_lease(store, &cfg, cmd)?,

        Cmd::Daemon { cmd } => handle_daemon(store, &cfg, cmd)?,

        Cmd::Serve {
            port,
            bind,
            token,
            dangerous,
        } => {
            let token = token.unwrap_or_default();
            let extra_dbs = cfg.peer_db_sources();
            let pull = mcp::PullConsent {
                from: cfg.pull_from_sources(),
                inject_pulled: cfg.inject_pulled(),
                allow_inject_from: cfg.allow_inject_from_sources(),
                policy: verify_policy(&cfg),
            };
            let nudge_tpl = cfg.nudge_template().map(str::to_owned);
            weave_mcp::serve_http(
                store,
                cfg.session.clone(),
                nudge_tpl.as_deref(),
                extra_dbs,
                pull,
                &RealInjector {
                    preferred_mux: parse_mux_preference(&cfg),
                },
                &bind,
                port,
                &token,
                dangerous,
            )?;
        }

        Cmd::Graph { me, circle, json } => {
            let (me, _explicit) = resolve_me_explicit(me, None, &cfg);
            let target_circle = resolve_list_circle(store, &cfg, &me, circle.as_deref(), false);
            let peers = store.list_peers()?;
            let peers_in_circle: Vec<_> = peers
                .into_iter()
                .filter(|p| {
                    target_circle
                        .as_ref()
                        .map(|c| model::circle_or_default(&p.circle) == c)
                        .unwrap_or(true)
                })
                .map(|p| p.name)
                .collect();
            let peer_set: std::collections::HashSet<_> = peers_in_circle.iter().cloned().collect();

            let mut g = fnx_classes::Graph::new(fnx_runtime::CompatibilityMode::Strict);
            for peer in &peers_in_circle {
                g.add_node(peer.clone());
            }
            // Collect all messages among peers in the circle.
            let mut seen_edges = std::collections::HashSet::new();
            for peer in &peers_in_circle {
                let hist = store.history(peer, None, 10_000)?;
                for msg in &hist {
                    if peer_set.contains(&msg.sender)
                        && peer_set.contains(&msg.recipient)
                        && msg.sender != msg.recipient
                    {
                        let key = if msg.sender <= msg.recipient {
                            (msg.sender.clone(), msg.recipient.clone())
                        } else {
                            (msg.recipient.clone(), msg.sender.clone())
                        };
                        if seen_edges.insert(key) {
                            let _ = g.add_edge(&msg.sender, &msg.recipient);
                        }
                    }
                }
            }

            let cc = fnx_algorithms::connected_components(&g);
            let dc = fnx_algorithms::degree_centrality(&g);
            let dens = fnx_algorithms::density(&g);
            let comp_count = cc.components.len();
            let largest = cc.components.iter().map(|c| c.len()).max().unwrap_or(0);

            if json {
                let components: Vec<Vec<String>> = cc.components;
                let scores: std::collections::HashMap<String, f64> =
                    dc.scores.into_iter().map(|s| (s.node, s.score)).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "nodes": g.node_count(),
                        "edges": g.edge_count(),
                        "density": dens.density,
                        "components": components,
                        "component_count": comp_count,
                        "largest_component": largest,
                        "centrality": scores,
                    }))?
                );
            } else {
                println!(
                    "communication graph ({} nodes, {} edges)",
                    g.node_count(),
                    g.edge_count()
                );
                println!("  density: {:.4}", dens.density);
                println!("  components: {} (largest: {} nodes)", comp_count, largest);
                for (i, comp) in cc.components.iter().enumerate() {
                    println!("    component {}: {}", i + 1, comp.join(", "));
                }
                println!("  degree centrality:");
                for s in &dc.scores {
                    println!("    {}: {:.4}", s.node, s.score);
                }
            }
        }

        #[cfg(feature = "surfaces")]
        Cmd::Dashboard {
            port,
            bind,
            token,
            write,
        } => {
            // Reuse WL-022 bearer auth: generate a random token if none given and
            // print it to stderr (never stdout — MCP stdout discipline), like Serve.
            let token = match token {
                Some(t) => t,
                None => {
                    let t = store::mint_birth_cert()?;
                    eprintln!("[weave] dashboard bearer token: {t}");
                    t
                }
            };
            if write {
                eprintln!("[weave] dashboard WRITE surface enabled (POST /api)");
            }
            // Each accepted connection thread opens its OWN read-only store handle
            // (Store is Send, not Sync) — the factory clones the resolved Config.
            // WL-052a: the `--write` POST /api route reuses the SAME dispatch_request
            // handler as MCP/CLI, with the real injector so a dashboard send nudges
            // exactly like a CLI send.
            let cfg_for_factory = cfg.clone();
            let me_default = cfg.session.clone();
            let injector = RealInjector {
                preferred_mux: parse_mux_preference(&cfg),
            };
            weave_mcp::serve_dashboard(
                &bind,
                port,
                &token,
                write,
                me_default,
                &injector,
                move || open_store(&cfg_for_factory),
            )?;
        }

        #[cfg(feature = "surfaces")]
        Cmd::Push {
            to,
            host,
            token,
            body,
            subject,
            to_host,
            priority,
            ttl,
            idempotency_key,
            me,
        } => {
            push_to_remote(
                store,
                &cfg,
                PushArgs {
                    to,
                    host,
                    token,
                    body,
                    subject,
                    to_host,
                    priority,
                    ttl,
                    idempotency_key,
                    me,
                },
            )?;
        }

        #[cfg(feature = "surfaces")]
        Cmd::Telegram => {
            telegram::run(store, &cfg)?;
        }

        #[cfg(feature = "surfaces")]
        Cmd::Slack => {
            slack::run(store, &cfg)?;
        }

        #[cfg(feature = "obscura")]
        Cmd::Web {
            op,
            url,
            args,
            list,
            stop,
            lease_ttl,
            audit,
        } => {
            // `--stop` reaps the cached obscura child and exits.
            if stop {
                weave_mcp::mcp::stop_web();
                println!("obscura stopped");
                return Ok(());
            }
            // `--list` enumerates the ops (no spawn).
            let action = if list {
                "list".to_string()
            } else {
                op.ok_or_else(|| {
                    anyhow::anyhow!("a web op is required (e.g. `weave web navigate --url …`)")
                })?
            };
            // Build the op args object: --url plus repeated --arg key=value. Values
            // are forwarded opaquely to obscura. Never a shell — this is structured
            // JSON, not a command string.
            let mut op_args = serde_json::Map::new();
            if let Some(u) = url {
                op_args.insert("url".to_string(), serde_json::Value::String(u));
            }
            for kv in args {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--arg must be KEY=VALUE (got {kv:?})"))?;
                op_args.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
            let (me, _explicit) = resolve_me_explicit(None, None, &cfg);
            let mut req = serde_json::Map::new();
            req.insert("me".to_string(), serde_json::Value::String(me.clone()));
            req.insert("action".to_string(), serde_json::Value::String(action));
            req.insert("args".to_string(), serde_json::Value::Object(op_args));
            if let Some(ttl) = lease_ttl {
                req.insert("lease_ttl".to_string(), serde_json::json!(ttl));
            }
            if audit {
                req.insert("audit".to_string(), serde_json::Value::Bool(true));
            }
            let req = serde_json::Value::Object(req);
            match weave_mcp::mcp::run_web(store, &Some(me), &req) {
                Ok(text) => println!("{text}"),
                Err(e) => anyhow::bail!("{e}"),
            }
        }

        Cmd::Schedule {
            from,
            to,
            subject,
            body,
            at,
            every,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            store::check_ident("recipient", &to)?;
            store::check_body(&body)?;
            let (kind, cron_expr, next_run) = match (at, every) {
                (Some(ts), None) => {
                    if ts <= 0 {
                        anyhow::bail!("'at' must be a positive UNIX timestamp");
                    }
                    (model::ScheduleKind::OneShot, String::new(), ts)
                }
                (None, Some(expr)) => {
                    let expr = expr.trim();
                    if !model::cron_valid(expr) {
                        anyhow::bail!("'every' is not a valid cron expression");
                    }
                    let next = model::next_occurrence(expr, model::now()).ok_or_else(|| {
                        anyhow::anyhow!("could not compute next occurrence from cron expression")
                    })?;
                    (model::ScheduleKind::Recurring, expr.to_string(), next)
                }
                (Some(_), Some(_)) => {
                    anyhow::bail!("provide exactly one of --at or --every, not both");
                }
                (None, None) => {
                    anyhow::bail!("provide exactly one of --at or --every");
                }
            };
            let id = store.schedule_message(
                &from,
                &to,
                subject.as_deref(),
                &body,
                kind,
                &cron_expr,
                next_run,
            )?;
            println!(
                "scheduled #{id}: {from} -> {to} at {next_run} ({kind})",
                kind = kind.as_str()
            );
        }

        Cmd::Schedules { me, limit, json } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            let rows = store.list_schedules(&me, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "schedules": rows }))?
                );
            } else if rows.is_empty() {
                println!("no scheduled messages for '{me}'");
            } else {
                for s in &rows {
                    let subj = s
                        .subject
                        .as_ref()
                        .map(|s| format!(" | {s}"))
                        .unwrap_or_default();
                    let state = if s.cancelled {
                        "cancelled"
                    } else if s.executed_ts.is_some() {
                        "executed"
                    } else {
                        "pending"
                    };
                    println!(
                        "#{} [{}] {} -> {}{} ({}) next={}",
                        s.id,
                        state,
                        s.sender,
                        s.recipient,
                        subj,
                        s.kind.as_str(),
                        s.next_run
                    );
                }
            }
        }

        Cmd::CancelSchedule { id } => {
            let cancelled = store.cancel_schedule(id)?;
            if cancelled {
                println!("cancelled schedule #{id}");
            } else {
                println!("schedule #{id} was already terminal or did not exist");
            }
        }

        Cmd::Tick { me, all } => {
            let (me, explicit) = resolve_me_explicit(me, None, &cfg);
            refresh_presence(store, &me, explicit);
            execute_tick(store, &me, all)?;
        }

        Cmd::Hook { event, wake } => handle_hook(store, &cfg, &event, wake)?,
    }
    Ok(())
}

/// `weave orchestrator` handler (P4). Routes claim/status through the SAME store
/// methods the MCP tools use, so the force-demote transaction and the
/// `is_alive`-reuse liveness verdict are enforced once, in the store. No injector
/// involvement (a role is a pure DB bit; nothing is nudged or spawned).
fn dispatch_orchestrator(store: &dyn Store, cfg: &Config, cmd: OrchestratorCmd) -> Result<()> {
    match cmd {
        OrchestratorCmd::Claim {
            circle,
            force,
            from,
        } => {
            let me = resolve_me(from, None, cfg);
            store::check_ident("name", &me)?;
            match store.claim_orchestrator_role(&me, circle.as_deref(), force)? {
                model::ClaimOutcome::Claimed { circle, demoted } => {
                    println!("claimed role=orchestrator for '{me}' in circle '{circle}'");
                    if !demoted.is_empty() {
                        println!("demoted: {}", demoted.join(", "));
                    }
                }
                model::ClaimOutcome::Refused { circle, holder } => {
                    println!(
                        "refused: '{holder}' is the live orchestrator in circle '{circle}' (pass --force to steal)"
                    );
                }
            }
        }
        OrchestratorCmd::Status { circle } => {
            // Default the status query to the caller's own circle when no circle is
            // given (so `weave orchestrator status` reports YOUR circle).
            let effective = circle.or_else(|| Some(cfg.circle()));
            let st = store.orchestrator_status(effective.as_deref())?;
            if st.present {
                let names: Vec<_> = st.holders.iter().map(|h| h.name.as_str()).collect();
                println!(
                    "orchestrator(s) present in circle '{}': {} (online)",
                    st.circle,
                    names.join(", ")
                );
            } else {
                println!("no live orchestrator in circle '{}'", st.circle);
            }
        }
    }
    Ok(())
}

/// `weave memory` handler (WL-017). Filesystem-backed scoped memory with no store
/// involvement; each subcommand validates its scope and delegates to `memory::`.
fn dispatch_memory(cfg: &Config, cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Write {
            scope,
            key,
            title,
            tag,
            body,
        } => {
            let scope = parse_memory_scope(&scope, cfg, None)?;
            memory::memory_write(&scope, &key, &title, &tag, &body)?;
            println!("wrote {}/{key}", scope.label());
        }
        MemoryCmd::Read { scope, key } => {
            let scope = parse_memory_scope(&scope, cfg, None)?;
            let e = memory::memory_read(&scope, &key)?;
            println!("{}\n---\n{}", format_entry_human(&e), e.body);
        }
        MemoryCmd::Search {
            scope,
            query,
            limit,
        } => {
            let scope = scope
                .as_deref()
                .map(|s| parse_memory_scope(s, cfg, None))
                .transpose()?;
            let hits = memory::memory_search(scope.as_ref(), &query)?;
            for e in hits.iter().take(limit) {
                println!(
                    "{} | {} | {} | tags={:?}",
                    e.scope.label(),
                    e.key,
                    e.title,
                    e.tags
                );
            }
        }
        MemoryCmd::List { scope } => {
            let scope = parse_memory_scope(&scope, cfg, None)?;
            let list = memory::memory_list(&scope)?;
            if list.is_empty() {
                println!("no entries in {}", scope.label());
            } else {
                for e in &list {
                    println!("{} | {} | tags={:?}", e.key, e.title, e.tags);
                }
            }
        }
        MemoryCmd::Delete { scope, key } => {
            let scope = parse_memory_scope(&scope, cfg, None)?;
            if memory::memory_delete(&scope, &key)? {
                println!("deleted {}/{key}", scope.label());
            } else {
                println!("not found: {}/{key}", scope.label());
            }
        }
        MemoryCmd::Scopes => {
            let scopes = memory::memory_scopes()?;
            println!(
                "global: {}",
                memory::memory_dir(&MemoryScope::Global).display()
            );
            if let Some(ps) = memory::project_scope_from_cwd() {
                println!("project: {} (from cwd)", memory::memory_dir(&ps).display());
            } else {
                println!("project: <not in a git repo>");
            }
            let me = resolve_me(None, None, cfg);
            println!(
                "persona: {} (resolved as '{me}')",
                memory::memory_dir(&MemoryScope::Persona(me.clone())).display()
            );
            let circle = cfg.circle();
            println!(
                "orchestrator: {} (circle='{circle}')",
                memory::memory_dir(&MemoryScope::Orchestrator(circle.clone())).display()
            );
            if !scopes.is_empty() {
                println!("\nscopes with entries:");
                for s in scopes {
                    println!("  - {}", s.label());
                }
            }
        }
    }
    Ok(())
}

fn format_entry_human(e: &memory::MemoryEntry) -> String {
    format!(
        "title: {}\ntags: {:?}\ncreated: {}\nupdated: {}",
        e.title,
        e.tags,
        model::fmt_ts(e.created_ts),
        model::fmt_ts(e.updated_ts)
    )
}

/// `weave review` handler (WL-020).
fn dispatch_review(store: &dyn Store, _cfg: &Config, cmd: ReviewCmd) -> Result<()> {
    match cmd {
        ReviewCmd::Queue { filter, limit } => {
            let filter =
                model::ReviewQueueFilter::from_str(&filter).map_err(|e| anyhow::anyhow!(e))?;
            let items = store.review_queue(filter, limit)?;
            if items.is_empty() {
                println!("no review items");
            } else {
                for item in items {
                    let status = if let Some(ts) = item.reviewed_at {
                        format!(
                            "reviewed by {} at {}",
                            item.reviewed_by.unwrap_or_default(),
                            model::fmt_ts(ts)
                        )
                    } else {
                        "pending".to_string()
                    };
                    println!(
                        "{} | {} | {} | {} | {}",
                        item.id, item.repo, item.author, status, item.pr_url
                    );
                    if !item.title.is_empty() {
                        println!("  title: {}", item.title);
                    }
                }
            }
        }
        ReviewCmd::Add {
            pr_url,
            title,
            author,
            repo,
        } => {
            let title = title.unwrap_or_default();
            let author = author.unwrap_or_default();
            let repo = repo.unwrap_or_default();
            let id = store.add_review_item(
                &pr_url,
                &title,
                &author,
                &repo,
                model::ReviewItemState::Open,
                None,
            )?;
            println!("added review item {}", id);
        }
        ReviewCmd::Mark { id } => {
            let me = resolve_me(None, None, _cfg);
            if store.mark_reviewed(&id, &me)? {
                println!("marked {} as reviewed", id);
            } else {
                println!("not found: {}", id);
            }
        }
        ReviewCmd::Remove { id } => {
            if store.remove_review_item(&id)? {
                println!("removed {}", id);
            } else {
                println!("not found: {}", id);
            }
        }
    }
    Ok(())
}

/// `weave permission` handler (WL-021).
fn dispatch_permission(store: &dyn Store, cfg: &Config, cmd: PermissionCmd) -> Result<()> {
    match cmd {
        PermissionCmd::Status { id, timeout } => {
            let (status, answer) = store.permission_verdict(&id, timeout.unwrap_or(0))?;
            match status {
                model::PermissionStatus::Pending => println!("{} pending", id),
                model::PermissionStatus::Approved => {
                    println!("{} approved (answer: {})", id, answer.unwrap_or_default())
                }
                model::PermissionStatus::Denied => {
                    println!("{} denied (answer: {})", id, answer.unwrap_or_default())
                }
                model::PermissionStatus::Timeout => {
                    println!("{} timeout (denied by default)", id)
                }
            }
        }
        PermissionCmd::List { limit } => {
            let me = resolve_me(None, None, cfg);
            let asks = store.list_permissions(&me, limit)?;
            if asks.is_empty() {
                println!("no permission asks");
            } else {
                for a in &asks {
                    let (status, _) = store.permission_verdict(&a.id, 0)?;
                    let tool = a
                        .options
                        .as_ref()
                        .and_then(|o| o.lines().next())
                        .unwrap_or("?");
                    println!(
                        "{} | {} | {} -> {} | {} | {}",
                        a.id,
                        status.as_str(),
                        a.asker,
                        a.askee,
                        tool,
                        model::fmt_ts(a.opened_ts)
                    );
                }
            }
        }
    }
    Ok(())
}

/// `weave lease` handler (WL-024). Advisory reservations with TTL.
fn dispatch_lease(store: &dyn Store, cfg: &Config, cmd: LeaseCmd) -> Result<()> {
    let me = resolve_me(None, None, cfg);
    match cmd {
        LeaseCmd::Reserve {
            resource,
            ttl,
            note,
        } => match store.reserve_lease(&me, &resource, ttl, note.as_deref()) {
            Ok(lease) => {
                println!(
                    "leased {} (expires {})",
                    lease.resource,
                    model::fmt_ts(lease.expires)
                );
            }
            Err(e) => {
                println!("failed: {}", e);
                std::process::exit(1);
            }
        },
        LeaseCmd::Release { resource } => {
            let ok = store.release_lease(&me, &resource)?;
            if ok {
                println!("released {}", resource);
            } else {
                println!("no active lease for {} held by you", resource);
                std::process::exit(1);
            }
        }
        LeaseCmd::List { limit, json } => {
            let leases = store.list_leases(limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&leases)?);
            } else if leases.is_empty() {
                println!("no active leases");
            } else {
                for l in &leases {
                    println!(
                        "{} | {} | acquired {} | expires {} | {}",
                        l.resource,
                        l.holder,
                        model::fmt_ts(l.acquired),
                        model::fmt_ts(l.expires),
                        if l.note.is_empty() { "-" } else { &l.note }
                    );
                }
            }
        }
        LeaseCmd::Sweep => {
            let n = store.sweep_expired_leases()?;
            println!("swept {} expired lease(s)", n);
        }
        LeaseCmd::Guard => {
            // Get staged files from git.
            let out = std::process::Command::new("git")
                .args(["diff", "--cached", "--name-only", "--relative"])
                .stderr(std::process::Stdio::null())
                .output();
            let files: Vec<String> = match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => {
                    println!("warning: could not get staged files from git");
                    return Ok(());
                }
            };
            if files.is_empty() {
                return Ok(());
            }
            let leases = store.list_leases(1000)?;
            let mut blocked = Vec::new();
            for f in &files {
                let norm = model::lease_path_normalize(f);
                for l in &leases {
                    if l.holder == me {
                        continue;
                    }
                    if model::lease_path_conflicts(&l.resource, &norm) {
                        blocked.push((f.clone(), l.resource.clone(), l.holder.clone()));
                        break;
                    }
                }
            }
            if !blocked.is_empty() {
                println!("Blocked: staged files conflict with active leases:");
                for (file, res, holder) in &blocked {
                    println!(
                        "  {} conflicts with lease '{}' held by {}",
                        file, res, holder
                    );
                }
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// `weave job` handler (P3 poll-only board). Routes the 8 subcommands through the
/// SAME 7 Store methods the MCP tools use, so attempt_id fencing, the state machine,
/// and input caps are enforced once, in the store. NO injector involvement (jobs do
/// not nudge in P3). Each subcommand supports `--json` for machine-readable output.
fn dispatch_job(store: &dyn Store, cfg: &Config, cmd: JobCmd) -> Result<()> {
    match cmd {
        JobCmd::Create {
            title,
            desc,
            kind,
            owner,
            assignee,
            circle,
            prompt,
            deadline,
            from,
            json,
        } => {
            let (creator, explicit) = resolve_me_explicit(from, None, cfg);
            refresh_presence(store, &creator, explicit);
            let spec = model::JobSpec {
                title,
                description: desc,
                kind,
                owner,
                assignee,
                circle,
                prompt,
                deadline_at: deadline,
                ..Default::default()
            };
            let job = store.create_job(&creator, spec)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "job": job }))?
                );
            } else {
                println!(
                    "created job {} [{}] '{}' (creator={})",
                    job.id,
                    job.state.as_str(),
                    job.title,
                    job.creator
                );
            }
        }
        JobCmd::List {
            state,
            owner,
            creator,
            assignee,
            circle,
            limit,
            json,
        } => {
            let state = match state {
                Some(s) if !s.trim().is_empty() => {
                    Some(model::JobState::from_str(s.trim()).map_err(|m| anyhow::anyhow!(m))?)
                }
                _ => None,
            };
            let filter = model::JobFilter {
                state,
                owner,
                creator,
                assignee,
                circle,
            };
            let jobs = store.list_jobs(filter, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "jobs": jobs }))?
                );
            } else if jobs.is_empty() {
                println!("no jobs");
            } else {
                for j in &jobs {
                    print_job_line(j);
                }
            }
        }
        JobCmd::Show { job_id, json } | JobCmd::Status { job_id, json } => {
            let job = store.get_job(&job_id)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "job": job }))?
                );
            } else {
                match job {
                    None => println!("no job '{job_id}'"),
                    Some(j) => print_job_line(&j),
                }
            }
        }
        JobCmd::Claim {
            job_id,
            as_who,
            json,
        } => {
            let (me, explicit) = resolve_me_explicit(as_who, None, cfg);
            refresh_presence(store, &me, explicit);
            let job = store
                .claim_job(&job_id, &me)?
                .ok_or_else(|| anyhow::anyhow!("no job '{job_id}'"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "job": job }))?
                );
            } else {
                println!(
                    "claimed job {} as '{}'; attempt_id={} state={}",
                    job.id,
                    me,
                    job.attempt_id.as_deref().unwrap_or("-"),
                    job.state.as_str()
                );
            }
        }
        JobCmd::Update {
            job_id,
            attempt,
            state,
            state_reason,
            phase,
            note,
            result_summary,
            result,
            error,
            artifacts,
            json,
        } => {
            let state = match state {
                Some(s) if !s.trim().is_empty() => {
                    Some(model::JobState::from_str(s.trim()).map_err(|m| anyhow::anyhow!(m))?)
                }
                _ => None,
            };
            let patch = model::JobPatch {
                state,
                state_reason,
                phase,
                progress_note: note,
                result_summary,
                result_json: result,
                error_json: error,
                artifacts_json: artifacts,
            };
            let job = store.update_job(&job_id, attempt.as_deref(), patch)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "job": job }))?
                );
            } else {
                println!("updated job {} [{}]", job.id, job.state.as_str());
            }
        }
        JobCmd::Result { job_id, json } => {
            let r = store.job_result(&job_id)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "result": r }))?
                );
            } else {
                match r {
                    None => println!("no job '{job_id}'"),
                    Some(r) if !r.ready => {
                        println!("job {} [{}] not_ready", r.id, r.state.as_str())
                    }
                    Some(r) => {
                        println!(
                            "job {} [{}] summary={} result={} error={} artifacts={}",
                            r.id,
                            r.state.as_str(),
                            r.result_summary.as_deref().unwrap_or("-"),
                            r.result_json,
                            r.error_json,
                            r.artifacts_json
                        );
                    }
                }
            }
        }
        JobCmd::Cancel {
            job_id,
            reason,
            from,
            json,
        } => {
            let (me, explicit) = resolve_me_explicit(from, None, cfg);
            refresh_presence(store, &me, explicit);
            let job = store
                .cancel_job(&job_id, &me, reason.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("no job '{job_id}'"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "job": job }))?
                );
            } else {
                println!(
                    "cancel requested for job {} (state {}, cancel_requested={})",
                    job.id,
                    job.state.as_str(),
                    job.cancel_requested
                );
            }
        }
    }
    Ok(())
}

/// One-line human summary of a [`model::Job`] for the CLI listings/show.
fn print_job_line(j: &model::Job) {
    println!(
        "{} [{}] {} (creator={}, assignee={}, updated {})",
        j.id,
        j.state.as_str(),
        j.title,
        j.creator,
        j.assignee.as_deref().unwrap_or("-"),
        model::fmt_ts(j.updated_ts)
    );
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
            let fp = sign::fingerprint(&pubkey).unwrap_or_else(|| "<unavailable>".to_string());
            println!("generated signing key for '{me}'");
            println!(
                "private key: {} (0600, keep secret)",
                sign::key_path().display()
            );
            println!("public key:  {pubkey}");
            println!("fingerprint: {fp}");
            println!("share the public key with peers so they can `weave key add {me} {pubkey}`");
        }
        KeyCmd::Show { me } => {
            let me = resolve_me(me, None, cfg);
            match sign::local_public_key()? {
                Some(pk) => {
                    let fp = sign::fingerprint(&pk).unwrap_or_else(|| "<unavailable>".to_string());
                    println!("identity:   {me}");
                    println!("public key: {pk}");
                    println!("fingerprint: {fp}");
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
        KeyCmd::Fingerprint { me, json } => {
            let me = resolve_me(me, None, cfg);
            match sign::local_public_key()? {
                Some(pk) => {
                    let fp = sign::fingerprint(&pk).unwrap_or_else(|| "<unavailable>".to_string());
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "identity": me,
                                "pubkey": pk,
                                "fingerprint": fp,
                            }))?
                        );
                    } else {
                        println!("{fp}");
                    }
                }
                None => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "identity": me,
                                "pubkey": serde_json::Value::Null,
                                "fingerprint": serde_json::Value::Null,
                            }))?
                        );
                    } else {
                        println!(
                            "no signing key configured for '{me}' — run `weave key gen` to create one"
                        );
                    }
                }
            }
        }
        KeyCmd::Rotate { me } => {
            let me = resolve_me(me, None, cfg);
            let (old_pub, new_pub) = sign::rotate_keypair()?;
            // Register the NEW key under this identity. With multi-key registration
            // (#7) this APPENDS — the OLD key (if any) stays registered for overlap,
            // so in-flight messages signed by the old key still verify against THIS
            // store until the old key is explicitly removed/revoked.
            store.register_key(&me, &new_pub)?;
            let new_fp = sign::fingerprint(&new_pub).unwrap_or_else(|| "<unavailable>".to_string());
            println!("rotated signing key for '{me}'");
            println!(
                "private key: {} (0600, keep secret; old key archived alongside)",
                sign::key_path().display()
            );
            match old_pub {
                Some(old) => {
                    let old_fp =
                        sign::fingerprint(&old).unwrap_or_else(|| "<unavailable>".to_string());
                    // FULL-digest forms — the ONLY values trust/revocation match on
                    // (the truncated display fp above is never trusted/revoked, R3).
                    let old_full = sign::fingerprint_full(&old)
                        .map(|f| format!("SHA256:{f}"))
                        .unwrap_or_else(|| old_fp.clone());
                    let new_full = sign::fingerprint_full(&new_pub)
                        .map(|f| format!("SHA256:{f}"))
                        .unwrap_or_else(|| new_fp.clone());
                    println!("old public key: {old}");
                    println!("old fingerprint: {old_fp}");
                    println!("new public key: {new_pub}");
                    println!("new fingerprint: {new_fp}");
                    println!(
                        "OVERLAP: the OLD key is kept registered locally alongside the new one, so \
                         in-flight messages signed by EITHER key verify during the window. RECEIVERS \
                         should also `weave key add {me} {old}` (multi-key: both stay registered) \
                         and trust BOTH keys' FULL fingerprints:\n  WEAVE_TRUST={old_full},{new_full}\n\
                         Once all peers have your new key, prune the old one with \
                         `weave key remove {me} {old}` and `weave key revoke {old_full}`."
                    );
                }
                None => {
                    println!("new public key: {new_pub}");
                    println!("new fingerprint: {new_fp}");
                    println!("(no prior key was present — this was a fresh generate)");
                }
            }
        }
        KeyCmd::Revoke { fp } => {
            // Config-driven revocation (no store table): validate the value and echo
            // exactly what to add to WEAVE_REVOKED / the config `revoked` list. weave
            // does not persist a managed config here, so we print the value rather
            // than rewriting a file. Accepts a `SHA256:<hex>` fingerprint or a full
            // pubkey hex; reject anything malformed so a footgun cannot enter the set.
            let entry = fp.trim();
            let normalized = if let Some(rest) = entry.strip_prefix("SHA256:") {
                if rest.len() == 64 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
                    entry.to_string()
                } else {
                    anyhow::bail!(
                        "invalid fingerprint: expected SHA256:<64 hex chars> \
                         (the FULL digest from `weave key fingerprint` / `key list`)"
                    );
                }
            } else {
                // Treat as a full pubkey hex; derive its fingerprint for display.
                if sign::check_pubkey(entry).is_err() {
                    anyhow::bail!(
                        "revoke expects a SHA256:<64-hex> fingerprint or a full 64-hex pubkey"
                    );
                }
                // Show the full-digest fingerprint so it matches what trust compares
                // against (the truncated display form is never trusted/revoked).
                match sign::fingerprint_full(entry) {
                    Some(full) => format!("SHA256:{full}"),
                    None => entry.to_string(),
                }
            };
            // Record a `Declared` provenance event in the local audit log so
            // `weave audit revocations` shows operator intent (which fp was marked
            // revoked, when, from this host) even before any enforcement fires. The
            // config predicate remains the SOLE decision source — this row is never
            // read by the verifier. BEST-EFFORT: a write failure prints a stderr note
            // and does NOT fail the command (the printed config guidance is what
            // actually revokes the key).
            let ev = store::RevocationEvent {
                id: 0,
                ts: model::now(),
                fp: normalized.clone(),
                identity: String::new(),
                source: String::new(),
                kind: store::RevocationKind::Declared,
            };
            if let Err(e) = store.record_revocation(&ev) {
                eprintln!("[weave] note: failed to record declared-revocation audit event: {e}");
            }
            println!("to revoke this key, add its fingerprint to your revocation list:");
            println!("  WEAVE_REVOKED={normalized}");
            println!("or in ~/.config/weave/config.toml:");
            println!("  revoked = [\"{normalized}\"]");
            println!(
                "a signature verifying against a revoked key is rejected UNCONDITIONALLY \
                 (even with strict_verify = false)."
            );
        }
        KeyCmd::Add { identity, pubkey } => {
            // Validate the identity and the hex pubkey before it touches the store.
            store::check_ident("identity", &identity)?;
            sign::check_pubkey(&pubkey)?;
            store.register_key(&identity, &pubkey)?;
            println!("registered public key for '{identity}'");
        }
        KeyCmd::Remove { identity, key } => {
            // Validate the identity at the seam. The `key` may be a full hex pubkey
            // or a SHA256:<full-64-hex> fingerprint; resolve a fingerprint to the
            // exact registered pubkey so the DELETE targets the right row. No shell.
            store::check_ident("identity", &identity)?;
            let entry = key.trim();
            let target = if sign::check_pubkey(entry).is_ok() {
                // A full hex pubkey: remove it directly.
                entry.to_string()
            } else {
                // Treat as a fingerprint and resolve it against this identity's
                // registered keys (FULL-digest match; R3). Only keys weave actually
                // has registered for this identity are candidates.
                let registered: Vec<String> = store
                    .get_keys(&identity)?
                    .into_iter()
                    .filter(|pk| sign::fingerprint_matches(entry, pk))
                    .collect();
                match registered.len() {
                    1 => registered.into_iter().next().unwrap(),
                    0 => anyhow::bail!(
                        "no registered key for '{identity}' matches '{entry}' \
                         (give a full hex pubkey or a SHA256:<64-hex> fingerprint)"
                    ),
                    _ => anyhow::bail!(
                        "'{entry}' matches multiple registered keys for '{identity}'; \
                         remove by full pubkey hex instead"
                    ),
                }
            };
            if store.remove_key(&identity, &target)? {
                println!("removed registered key for '{identity}'");
            } else {
                println!("no matching registered key for '{identity}' (nothing removed)");
            }
        }
        KeyCmd::List { json } => {
            let keys = store.list_keys()?;
            // Trust/revoked sets are receiver-local policy, surfaced read-only so the
            // listing shows which registered keys are trusted/revoked. Secret-free.
            let trust = cfg.trust_set();
            let revoked = cfg.revoked_set();
            let is_trusted = |pk: &str| trust.iter().any(|e| sign::fingerprint_matches(e, pk));
            let is_revoked = |pk: &str| revoked.iter().any(|e| sign::fingerprint_matches(e, pk));
            if json {
                let arr: Vec<_> = keys
                    .iter()
                    .map(|(i, p)| {
                        serde_json::json!({
                            "identity": i,
                            "pubkey": p,
                            "fingerprint": sign::fingerprint(p),
                            "trusted": is_trusted(p),
                            "revoked": is_revoked(p),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "keys": arr,
                        "trust_set": trust,
                        "revoked_set": revoked,
                    }))?
                );
            } else if keys.is_empty() {
                println!("no registered keys");
            } else {
                println!("{} registered key(s):", keys.len());
                for (identity, pubkey) in &keys {
                    let fp =
                        sign::fingerprint(pubkey).unwrap_or_else(|| "<unavailable>".to_string());
                    let mut tags = Vec::new();
                    if is_trusted(pubkey) {
                        tags.push("trusted");
                    }
                    if is_revoked(pubkey) {
                        tags.push("REVOKED");
                    }
                    let suffix = if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", tags.join(", "))
                    };
                    println!("  {identity}  {fp}{suffix}");
                    println!("    {pubkey}");
                }
            }
        }
    }
    Ok(())
}

/// `weave audit` handler (only built with `--features sign`). Read-only, secret-free
/// views over the local audit logs. The observed-revocation log surfaces enforced
/// rejections + operator declared revokes; it is NEVER consulted by the verifier (the
/// config `revoked` predicate stays the single decision source), so reading it has no
/// bearing on R1. Output carries fingerprints + public identities/labels only.
#[cfg(feature = "sign")]
fn handle_audit(store: &dyn Store, cmd: AuditCmd) -> Result<()> {
    match cmd {
        AuditCmd::Revocations { json, limit } => {
            let rows = store.list_revocations(limit)?;
            if json {
                let arr: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "ts": r.ts,
                            "fp": r.fp,
                            "identity": r.identity,
                            "source": r.source,
                            "kind": r.kind.as_str(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "revocations": arr,
                        "count": rows.len(),
                    }))?
                );
            } else if rows.is_empty() {
                println!("0 revocation event(s)");
            } else {
                println!("{} revocation event(s):", rows.len());
                for r in &rows {
                    let id = if r.identity.is_empty() {
                        "-"
                    } else {
                        &r.identity
                    };
                    let src = if r.source.is_empty() { "-" } else { &r.source };
                    println!(
                        "  {} [{}] identity={id} source={src} fp={}",
                        model::fmt_ts(r.ts),
                        r.kind.as_str(),
                        r.fp
                    );
                }
            }
        }
    }
    Ok(())
}

/// PID file for the optional presence daemon.  Overridable via `WEAVE_PIDFILE`
/// so integration tests can use temp-scoped paths for parallel safety.
fn daemon_pidfile() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("WEAVE_PIDFILE") {
        return std::path::PathBuf::from(p);
    }
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| std::path::PathBuf::from(d).join("weave").join("weaved.pid"))
        .unwrap_or_else(|_| std::env::temp_dir().join("weaved.pid"))
}

/// argv-only probe: `kill -0 <pid>` returns success iff the process exists.
fn daemon_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

fn handle_daemon(store: &dyn Store, cfg: &Config, cmd: DaemonCmd) -> Result<()> {
    let me = resolve_me(None, None, cfg);
    store::check_ident("name", &me)?;
    let pidfile = daemon_pidfile();
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match cmd {
        DaemonCmd::Start => {
            if pidfile.exists() {
                if let Ok(pid_str) = std::fs::read_to_string(&pidfile) {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        if daemon_running(pid) {
                            println!("daemon already running (pid {pid})");
                            return Ok(());
                        }
                    }
                }
            }
            let exe = std::env::current_exe()?;
            let child = std::process::Command::new(&exe)
                .args(["daemon", "run", "--me", &me])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            std::fs::write(&pidfile, child.id().to_string())?;
            println!("daemon started (pid {})", child.id());
        }
        DaemonCmd::Stop => {
            if !pidfile.exists() {
                println!("daemon not running");
                return Ok(());
            }
            let pid_str = std::fs::read_to_string(&pidfile)?;
            let pid = pid_str.trim().parse::<u32>()?;
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
            let _ = std::fs::remove_file(&pidfile);
            println!("daemon stopped (pid {pid})");
        }
        DaemonCmd::Status => {
            if !pidfile.exists() {
                println!("daemon: stopped");
                return Ok(());
            }
            let pid_str = std::fs::read_to_string(&pidfile).unwrap_or_default();
            let pid = pid_str.trim().parse::<u32>().unwrap_or(0);
            if daemon_running(pid) {
                println!("daemon: running (pid {pid})");
            } else {
                println!("daemon: stopped (stale pidfile)");
                let _ = std::fs::remove_file(&pidfile);
            }
        }
        DaemonCmd::Run { me: me_arg } => {
            let name = resolve_me(me_arg, None, cfg);
            store::check_ident("name", &name)?;
            let host = config::this_host();
            let pid = std::process::id() as i64;
            let heartbeat_secs = std::env::var("WEAVE_DAEMON_HEARTBEAT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(15);
            let evict_secs = std::env::var("WEAVE_DAEMON_EVICT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60);
            let evict_cutoff_secs = std::env::var("WEAVE_DAEMON_EVICT_CUTOFF_SECS")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(30);
            let mut last_evict = std::time::Instant::now();
            loop {
                if let Err(e) = store.heartbeat(&name, &host, Some(pid)) {
                    eprintln!("[weaved] heartbeat error: {e}");
                }
                if last_evict.elapsed().as_secs() >= evict_secs {
                    if let Err(e) = store.evict_stale_presence(evict_cutoff_secs) {
                        eprintln!("[weaved] evict error: {e}");
                    }
                    last_evict = std::time::Instant::now();
                }
                std::thread::sleep(std::time::Duration::from_secs(heartbeat_secs));
            }
        }
    }
    Ok(())
}

/// WL-016: execute all due schedules for `me` (or every sender when `all=true`).
/// For each due schedule, fires `store.send`, advances/closes the row via
/// `mark_schedule_executed`, and records a best-effort delivery trace.
fn execute_tick(store: &dyn Store, me: &str, all: bool) -> Result<()> {
    let now_ts = model::now();
    let due = store.get_due_schedules(now_ts)?;
    let mut fired = 0usize;
    let mut skipped = 0usize;
    for sched in &due {
        if !all && sched.sender != me {
            skipped += 1;
            continue;
        }
        let mid = store.send(
            &sched.sender,
            &sched.recipient,
            sched.subject.as_deref(),
            &sched.body,
            None,
            None,
        )?;
        store.mark_schedule_executed(sched.id)?;
        record_delivery_best_effort(
            store,
            mid,
            model::DeliveryRefKind::Message,
            &sched.recipient,
            model::DeliveryStage::Queued,
            model::DeliveryOutcome::Ok,
        );
        fired += 1;
    }
    if fired > 0 || skipped > 0 {
        println!("tick: {fired} schedule(s) fired, {skipped} skipped");
    }
    Ok(())
}

fn handle_hook(store: &dyn Store, cfg: &Config, event: &str, wake_flag: bool) -> Result<()> {
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
            let t = inject::detect_target_with_preference(parse_mux_preference(cfg));
            // Pass the captured kitty control socket through (empty for non-kitty);
            // see the Register arm. A poisoned/empty socket is harmless — only the
            // kitty injector consults it. Capture PID + host so presence reflects
            // real process-liveness for this hook-registered session, plus the git
            // tags. The hot path stays cheap: tag capture is a single fs read
            // primary + a timeout-bounded best-effort git fallback that never sinks
            // the hook.
            let tags = git_tags_for(cwd);
            let env_cert = std::env::var("WEAVE_BIRTH_CERT")
                .ok()
                .filter(|s| !s.is_empty());
            // SessionStart re-fires on every restart, so an already-registered
            // peer holds a minted cert. Mirror `attach`: fall back to our OWN
            // stored cert so re-registration is idempotent for the peer owner
            // instead of failing the hook with "already registered; provide
            // --cert". The spawn-threaded WEAVE_BIRTH_CERT still takes
            // precedence; identity-takeover protection is unchanged — a
            // *mismatched* supplied cert still hard-bails inside
            // register_peer_full, and a brand-new peer still mints fresh.
            let stored_cert = store.get_birth_cert(&me)?;
            let cert = env_cert.as_deref().or(stored_cert.as_deref());
            let cert = store.register_peer_full(
                &me,
                t.mux.as_str(),
                &t.id,
                &t.socket,
                cwd,
                Some(std::process::id() as i64),
                &config::this_host(),
                &tags.repo,
                &tags.branch,
                &tags.worktree_id,
                &cfg.circle(),
                cert,
            )?;
            eprintln!(
                "[weave] registered peer '{me}' [{}] (birth-cert: {cert})",
                t.mux.as_str()
            );
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
            // P5: a freshly-registered session that has not taken its first turn is
            // `pending_first_turn`. Best-effort + AFTER registration so a setter
            // failure can never sink registration/presence (the gc/git-tags
            // precedent). UPDATE-only on the caller's own row — not gated on
            // `explicit_identity` because a guessed name worst-case touches 0 rows
            // (it cannot consume an inbox the way read-marking can).
            set_turn_state_best_effort(store, &me, model::TurnState::PendingFirstTurn);
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
        //
        // WL-025: with --wake (or WEAVE_STOP_WAKE=1), the stop hook switches to
        // blocking mode: it drains the inbox with mark_read=true and, if messages
        // exist, emits a structured JSON block that Claude Code uses as the next
        // turn's input. The wake IS the delivery, so marking read is correct.
        "prompt" | "stop" => {
            // Tier-2: opportunistically pull cross-store intents into the local
            // inbox BEFORE draining, so a freshly-pulled message is delivered in
            // this same turn. Best-effort: a pull failure never sinks the drain.
            try_pull(store, cfg, &me);
            let is_wake_stop = event == "stop"
                && (wake_flag || std::env::var("WEAVE_STOP_WAKE").ok().as_deref() == Some("1"));
            let mut mark_read = event == "prompt" || is_wake_stop;
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
                if is_wake_stop {
                    // WL-025: blocking wake — render all unread messages into the
                    // structured JSON that Claude Code uses as the next turn input.
                    let reason = rows
                        .iter()
                        .map(|m| {
                            let subj = m
                                .subject
                                .as_ref()
                                .map(|s| format!(" ({s})"))
                                .unwrap_or_default();
                            format!("#{} from {}{}: {}", m.id, m.sender, subj, m.body)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!(
                        "{}",
                        serde_json::json!({
                            "decision": "block",
                            "reason": format!("[weave] {} new message(s) for '{me}':\n{reason}", rows.len()),
                            "suppressOutput": true,
                        })
                    );
                } else {
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
                // P6 drain trace: ONLY on the marking-read branch (a peek/Stop does not
                // "drain"). This is the transport-side proof the message actually landed
                // in a turn. Best-effort + AFTER the drain (the hot path) so a trace
                // write can never sink delivery; the OUTCOME is recorded by the store —
                // no store->inject edge.
                if mark_read {
                    for m in &rows {
                        record_delivery_best_effort(
                            store,
                            m.id,
                            model::DeliveryRefKind::Message,
                            &me,
                            model::DeliveryStage::Drained,
                            model::DeliveryOutcome::Ok,
                        );
                    }
                }
            }
            // P5: a UserPromptSubmit means a turn just started (working); a Stop
            // means the turn finished cleanly (idle). Best-effort + AFTER the drain
            // (the delivery hot path) so a setter failure never blocks delivery.
            let next = if event == "prompt" {
                model::TurnState::Working
            } else {
                model::TurnState::Idle
            };
            set_turn_state_best_effort(store, &me, next);
            // WL-014: remind the recipient of any open asks on every prompt.
            // WL-015: render open asks as actionable prompts.
            if event == "prompt" {
                nudge_open_asks(store, &me);
                render_open_asks(store, &me);
                // WL-016: daemon-free schedule tick. Best-effort: a tick failure must
                // never sink the prompt hook's primary delivery path.
                if let Err(e) = execute_tick(store, &me, false) {
                    eprintln!("[weave] tick skipped (non-fatal): {e}");
                }
            }
        }
        "wake" => {
            // Wake is a non-consuming guard. When we cannot trust the resolved
            // identity, do not mutate the wake watermark for a guessed peer.
            if !explicit_identity {
                eprintln!(
                    "[weave] no explicit session identity (set WEAVE_SESSION or config `session`); \
                     skipping wake block for guessed '{me}'"
                );
            } else {
                try_pull(store, cfg, &me);
                match store.peek_oldest_unread(&me) {
                    Ok(Some(msg)) => match store.wake_last_acked(&me) {
                        Ok(acked) if msg.id > acked => {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "decision": "block",
                                    "reason": wake_reason(&msg),
                                    "suppressOutput": true,
                                })
                            );
                            if let Err(e) = store.set_wake_ack(&me, msg.id) {
                                eprintln!("[weave] wake ack update skipped (non-fatal): {e}");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[weave] wake ack lookup skipped (non-fatal): {e}"),
                    },
                    Ok(None) => {}
                    Err(e) => eprintln!("[weave] wake peek skipped (non-fatal): {e}"),
                }
            }
        }
        // Notification: the agent's prompt is live + unconsumed (awaiting input).
        // This arm has no drain — just the best-effort turn_state setter.
        "notification" => {
            set_turn_state_best_effort(store, &me, model::TurnState::AwaitingInput);
        }
        // WL-055: PreToolUse approval gate. Distinct codepath (`handle_pretooluse_hook`)
        // because the contract differs from the other hooks: it reads its OWN stdin
        // JSON shape (`tool_name`/`tool_input`), emits a `hookSpecificOutput`
        // permission decision to stdout as PURE JSON, and must FAIL CLOSED (deny) on
        // any ambiguity for a dangerous tool. We re-route here BEFORE this arm's
        // `me`/inbox handling so the PreToolUse drain never marks an inbox read.
        "pretooluse" => {
            // The generic `handle_hook` body above already consumed stdin into `v`;
            // pass the parsed payload + parse-ok flag straight through.
            return handle_pretooluse_hook(store, cfg, &v, payload_ok);
        }
        other => eprintln!("[weave] unknown hook event: {other}"),
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// WL-055 — the enforcing PreToolUse approval gate.
//
// weave already had the approval *primitive* (the `weave_ask_permission` /
// `weave answer … approve` ToolPermission ask + the read-time `permission_verdict`,
// and the `DANGEROUS_TOOLS` list), but installed NO PreToolUse hook, so nothing ever
// *blocked* a tool call. This drain closes that gap: it is the command Claude runs
// for `hooks.PreToolUse`, and it raises a BLOCKING approval on the existing
// machinery, emitting Claude's `permissionDecision`.
//
// Contract (Claude Code, verified): stdin carries `{tool_name, tool_input, …}`. To
// DENY, stdout is `{"hookSpecificOutput":{"hookEventName":"PreToolUse",
// "permissionDecision":"deny","permissionDecisionReason":"…"}}` and we exit 0.
// `permissionDecision ∈ allow|deny|ask|defer`. The hook FAILS OPEN (Claude's 600s
// default timeout lets the tool proceed on timeout), so this drain enforces its OWN
// short internal timeout (`Config::pretooluse_timeout`) and emits an explicit `deny`
// — it NEVER relies on Claude's timeout. stdout is PURE JSON (no banner noise; all
// diagnostics go to stderr).
// ───────────────────────────────────────────────────────────────────────────

/// Claude-native tools the matcher (`Bash|Edit|Write`) routes to this gate. weave's
/// own `DANGEROUS_TOOLS` list (`weave_*`) does not name these host tools, so we treat
/// the matched filesystem/shell mutators as dangerous here too. The PreToolUse
/// *matcher* already narrows which tools reach the drain; this is the in-drain
/// confirmation so a mis-registered (over-broad) matcher still can't sneak a benign
/// read-only tool into a blocking ask, and a future widened matcher fails closed only
/// for genuinely dangerous tools.
const PRETOOLUSE_NATIVE_DANGEROUS: &[&str] =
    &["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Is `tool` one the PreToolUse gate must block on? Reuses weave's existing
/// `is_dangerous_tool` (the `weave_*` MCP tools) and adds the Claude-native mutators
/// the matcher routes here. Case-sensitive on the native set (Claude tool names are
/// fixed PascalCase).
fn pretooluse_is_dangerous(tool: &str) -> bool {
    weave_mcp::mcp::is_dangerous_tool(tool) || PRETOOLUSE_NATIVE_DANGEROUS.contains(&tool)
}

/// Build the pure-JSON PreToolUse response for `decision` (allow|deny|defer|ask) with
/// `reason`. This is the ONLY thing that must reach stdout.
fn pretooluse_response(decision: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    })
}

/// Emit a PreToolUse decision as pure JSON on stdout and return `Ok(())` (exit 0).
fn emit_pretooluse(decision: &str, reason: &str) -> Result<()> {
    println!("{}", pretooluse_response(decision, reason));
    Ok(())
}

/// The enforcing PreToolUse drain (WL-055). `v` is the already-parsed stdin payload
/// (parsed tolerantly by `handle_hook`); `payload_ok` is false when stdin was empty
/// or unparseable.
///
/// FAIL-OPEN on ambiguity that is NOT a dangerous tool (malformed stdin, missing
/// `tool_name`, a non-dangerous tool) — emit `defer`/`allow` and exit 0 so we never
/// break the user's session. FAIL-CLOSED (`deny`) for a dangerous tool whenever we
/// cannot positively prove approval (no approver configured, deny verdict, or our own
/// short timeout).
fn handle_pretooluse_hook(
    store: &dyn Store,
    cfg: &Config,
    v: &serde_json::Value,
    payload_ok: bool,
) -> Result<()> {
    let (decision, reason) = pretooluse_decision(store, cfg, v, payload_ok);
    emit_pretooluse(decision, &reason)
}

/// The pure decision core of the PreToolUse gate: given the parsed stdin payload,
/// return `(permissionDecision, reason)`. Side effects are confined to the store
/// (opening/polling the approval ask) and a best-effort live nudge — never stdout, so
/// this is unit-testable. `handle_pretooluse_hook` is the thin stdout-emitting shell.
fn pretooluse_decision(
    store: &dyn Store,
    cfg: &Config,
    v: &serde_json::Value,
    payload_ok: bool,
) -> (&'static str, String) {
    // Tolerant parse: a garbled/empty payload means we cannot identify the tool, so
    // we must not block — defer to Claude's normal flow (fail open).
    if !payload_ok {
        eprintln!("[weave] pretooluse: unparseable/empty stdin; deferring (fail-open)");
        return (
            "defer",
            "weave: no parseable PreToolUse payload".to_string(),
        );
    }
    let Some(tool_name) = v.get("tool_name").and_then(|x| x.as_str()) else {
        eprintln!("[weave] pretooluse: no tool_name in payload; deferring (fail-open)");
        return (
            "defer",
            "weave: no tool_name in PreToolUse payload".to_string(),
        );
    };

    // Not a dangerous tool → passthrough (defer; let Claude's own permission flow
    // decide). The gate only ever blocks dangerous tools.
    if !pretooluse_is_dangerous(tool_name) {
        return ("defer", format!("weave: '{tool_name}' is not gated"));
    }

    // Dangerous tool. DENY-BY-DEFAULT unless we can positively prove an approval.
    let Some(approver) = cfg.pretooluse_approver() else {
        eprintln!(
            "[weave] pretooluse: dangerous tool '{tool_name}' but no approver configured \
             (set `pretooluse_approver` / WEAVE_PRETOOLUSE_APPROVER); DENYING"
        );
        return (
            "deny",
            format!(
                "weave: '{tool_name}' requires approval but no approver is configured \
                 (deny-by-default)"
            ),
        );
    };
    if model::is_broadcast(approver) {
        // A tracked ask is point-to-point; a broadcast approver can never resolve.
        eprintln!("[weave] pretooluse: approver '{approver}' is the broadcast id; DENYING");
        return (
            "deny",
            "weave: approver must be a single peer, not broadcast (deny-by-default)".to_string(),
        );
    }

    // Identify the requesting session (this hook's own identity) as the asker. A
    // guessed/empty identity is fine here — the ask still resolves; the askee (the
    // approver) is what matters for the verdict.
    let cwd = v.get("cwd").and_then(|x| x.as_str());
    let from = resolve_me(None, cwd, cfg);
    let from = if from.is_empty() || from == "unknown" {
        "weave-pretooluse".to_string()
    } else {
        from
    };

    // Summarize the tool input for the human approver, capped so a huge diff/body can
    // never bloat the ask. `options` carries the structured `tool\nargs` the existing
    // permission tooling expects (mirrors `tool_ask_permission`).
    let tool_input = v
        .get("tool_input")
        .map(|ti| ti.to_string())
        .unwrap_or_default();
    let args_preview: String = tool_input.chars().take(400).collect();
    let options = format!("{tool_name}\n{args_preview}");
    let body = format!(
        "PreToolUse approval: session '{from}' wants to run '{tool_name}'. \
         Reply `approve` to allow, anything else to deny."
    );

    let (cid, _qid) = match store.ask(
        &from,
        approver,
        Some("PreToolUse approval"),
        &body,
        model::AskKind::ToolPermission,
        Some(&options),
        None,
    ) {
        Ok(ok) => ok,
        Err(e) => {
            // We could not even open the ask → we cannot prove approval → DENY.
            eprintln!("[weave] pretooluse: failed to open approval ask ({e}); DENYING");
            return (
                "deny",
                "weave: could not raise the approval request (deny-by-default)".to_string(),
            );
        }
    };

    // Fire the caller-side live nudge to the approver (best-effort; no store->inject
    // edge). If they're not injectable the ask still waits in their inbox — they can
    // approve on their next turn within our timeout window.
    let verdict = ask_inject_verdict(store, cfg, &from, approver, &body);
    eprintln!(
        "[weave] pretooluse: opened approval ask {cid} ({from} -> {approver}, {verdict}); \
         waiting up to {}s",
        cfg.pretooluse_timeout()
    );

    // Block (with our OWN short timeout) polling the EXISTING read-time verdict until
    // it leaves Pending. Approve ⇒ allow; deny/timeout ⇒ deny (fail closed).
    let status = pretooluse_wait_for_verdict(store, &cid, cfg.pretooluse_timeout());
    match status {
        model::PermissionStatus::Approved => {
            eprintln!("[weave] pretooluse: approved (ask {cid}); ALLOWING '{tool_name}'");
            ("allow", format!("weave: approved by '{approver}'"))
        }
        model::PermissionStatus::Denied => {
            eprintln!("[weave] pretooluse: denied (ask {cid}); DENYING '{tool_name}'");
            ("deny", format!("weave: denied by '{approver}'"))
        }
        model::PermissionStatus::Timeout | model::PermissionStatus::Pending => {
            // Timeout (or still pending at our deadline) ⇒ DENY-BY-DEFAULT. Claude's
            // own timeout would have failed OPEN, so we MUST emit deny ourselves.
            eprintln!(
                "[weave] pretooluse: no approval within {}s (ask {cid}); DENYING '{tool_name}'",
                cfg.pretooluse_timeout()
            );
            (
                "deny",
                format!("weave: no approval from '{approver}' within timeout (deny-by-default)"),
            )
        }
    }
}

/// Poll the EXISTING `permission_verdict` for ask `cid` until it leaves `Pending` or
/// `timeout_secs` elapses, then return the final status. The verdict is derived at
/// read time (`model::permission_status`), so a fresh `weave answer … approve`/`deny`
/// is observed on the next poll. We pass `timeout_secs` so the store's own
/// open-age→Timeout transition lines up with our wall-clock deadline. A store error
/// on a poll is logged and treated as still-pending (fail closed at the deadline).
fn pretooluse_wait_for_verdict(
    store: &dyn Store,
    cid: &str,
    timeout_secs: i64,
) -> model::PermissionStatus {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(0) as u64);
    let poll = std::time::Duration::from_millis(250);
    loop {
        match store.permission_verdict(cid, timeout_secs) {
            Ok((model::PermissionStatus::Pending, _)) => {}
            Ok((status, _)) => return status, // Approved | Denied | Timeout — terminal here.
            Err(e) => {
                eprintln!("[weave] pretooluse: verdict poll error (treating as pending): {e}");
            }
        }
        if std::time::Instant::now() >= deadline {
            // Resolve once more so an answer that landed in the last poll window is
            // honored; otherwise this is a Timeout (deny).
            return match store.permission_verdict(cid, timeout_secs) {
                Ok((status, _)) => status,
                Err(_) => model::PermissionStatus::Timeout,
            };
        }
        std::thread::sleep(poll);
    }
}

/// Best-effort turn_state update for the hook hot path (P5). The setter targets the
/// CALLER's own row only and is SWALLOWED on error (to stderr) so a turn_state write
/// can never sink message delivery or registration — the gc/git-tags precedent.
fn set_turn_state_best_effort(store: &dyn Store, me: &str, state: model::TurnState) {
    if let Err(e) = store.set_turn_state(me, state.as_str()) {
        eprintln!("[weave] turn_state update skipped (non-fatal): {e}");
    }
}

fn wake_reason(msg: &model::Message) -> String {
    let subj = msg
        .subject
        .as_ref()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    format!(
        "unread message #{} from {}{}: {}",
        msg.id, msg.sender, subj, msg.body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed clock for every render test: frame contents must be deterministic, so
    /// `now` is always passed in — never `model::now()`. (2021-01-01T00:00:00Z.)
    const FIXED_NOW: i64 = 1_609_459_200;

    /// The fixed `this_host` every render test passes in — never the real hostname,
    /// so the host-aware liveness verdict is deterministic. Rows built by `row()`
    /// share this host (`h1`), so a recent same-host null-pid row is `AliveLocal`
    /// (ttl) and an old one is `Stale`; a remote row sets a different host.
    const FIXED_HOST: &str = "h1";

    /// Offset past the online TTL window (`store::ONLINE_TTL_SECS` = 900s) so a row
    /// reads `Stale` regardless of host. Kept local to the tests; not load-bearing
    /// elsewhere.
    const STALE_OFFSET: i64 = 1_000;

    /// Build a same-host (`h1`), null-pid dashboard row. `alive` maps to the TTL
    /// recency field: alive ⇒ `last_seen == now` (recent ⇒ `AliveLocal`, ttl), dead
    /// ⇒ far past the window (`Stale`). No PID is set, so an alive same-host row is
    /// the "ttl" reason variant. Liveness is computed by the render, not stored.
    fn row(name: &str, repo: &str, branch: &str, alive: bool) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            worktree: "(main)".to_string(),
            mux: "tmux".to_string(),
            host: FIXED_HOST.to_string(),
            pid: None,
            last_seen: if alive {
                FIXED_NOW
            } else {
                FIXED_NOW - STALE_OFFSET
            },
            via: String::new(),
            turn_state: String::new(),
            description: String::new(),
        }
    }

    /// WL-057 (#107): `weave setup --exe <path>` parses into `Cmd::Setup{ exe: Some }`.
    #[test]
    fn setup_exe_flag_parses() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let cli = Cli::try_parse_from(["weave", "setup", "--exe", "/tmp/x"])
                    .expect("`weave setup --exe /tmp/x` should parse");
                match cli.cmd {
                    Cmd::Setup { exe, .. } => assert_eq!(exe.as_deref(), Some("/tmp/x")),
                    _ => panic!("expected Cmd::Setup"),
                }
            })
            .expect("spawn parser test")
            .join()
            .expect("parser test should not panic");
    }

    /// Default `weave setup` (no `--exe`) parses with `exe: None` — the byte-identical
    /// default path is preserved.
    #[test]
    fn setup_without_exe_flag_parses_none() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let cli =
                    Cli::try_parse_from(["weave", "setup"]).expect("`weave setup` should parse");
                match cli.cmd {
                    Cmd::Setup { exe, .. } => assert!(exe.is_none()),
                    _ => panic!("expected Cmd::Setup"),
                }
            })
            .expect("spawn parser test")
            .join()
            .expect("parser test should not panic");
    }

    /// Empty snapshot ⇒ a stable header + `no sessions` body, zeroed counts.
    #[test]
    fn dashboard_empty_case() {
        let out = render_sessions_dashboard(&[], &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        assert!(out.contains(
            "0 session(s), 0 local-alive, 0 remote-alive, 0 stale, 0 repo(s), 0 branch(es)"
        ));
        assert!(out.contains("no sessions"));
        // No per-group section header (those contain " / ") is emitted when empty.
        assert!(
            !out.contains(" / "),
            "empty frame had a group section: {out}"
        );
    }

    /// Header summary counts: total sessions, the local/remote/stale liveness
    /// breakdown, #repos, #branches.
    #[test]
    fn dashboard_header_summary_counts() {
        let rows = vec![
            row("a", "repoA", "main", true),
            row("b", "repoA", "feat", false),
            row("c", "repoB", "main", true),
        ];
        let out =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        // 3 sessions: 2 same-host recent (local-alive), 0 remote, 1 stale; 2 repos,
        // 3 distinct (repo,branch) branches.
        assert!(
            out.contains(
                "3 session(s), 2 local-alive, 0 remote-alive, 1 stale, 2 repo(s), 3 branch(es)"
            ),
            "header was: {out}"
        );
    }

    /// Mixed local/remote/stale snapshot: each row shows the right reason marker and
    /// the header carries the correct three-count breakdown. Remote rows force a
    /// non-matching host so the verdict is deterministic (no PID probe on the remote
    /// arm); same-host rows are TTL-based (null-pid) so they never depend on a live
    /// process — fully deterministic from (`this_host`, `now`).
    #[test]
    fn dashboard_mixed_liveness_reasons_and_summary() {
        // same-host recent, with a known pid ⇒ "alive (local, pid)"
        let mut local_pid = row("withpid", "repoA", "main", true);
        local_pid.pid = Some(std::process::id() as i64);
        // same-host recent, null pid ⇒ "alive (local, ttl)"
        let local_ttl = row("nopid", "repoA", "main", true);
        // remote host, recent ⇒ "alive (remote, ttl)"
        let mut remote = row("remote", "repoA", "main", true);
        remote.host = "otherbox".to_string();
        // same-host but old ⇒ "stale"
        let stale = row("old", "repoA", "main", false);
        let rows = dashboard_rows_sorted(vec![local_pid, local_ttl, remote, stale]);
        let out =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        // The local-pid row prints "pid" iff the pid is actually alive (the running
        // test process is): assert a stable membership instead of an exact verdict.
        assert!(
            out.contains("[alive (local, pid)]") || out.contains("[stale]"),
            "missing local-pid reason: {out}"
        );
        assert!(
            out.contains("[alive (local, ttl)]"),
            "missing local-ttl: {out}"
        );
        assert!(
            out.contains("[alive (remote, ttl)]"),
            "missing remote: {out}"
        );
        assert!(out.contains("[stale]"), "missing stale: {out}");
        // The remote row carries the <remote> marker; same-host rows do not.
        assert!(
            out.contains("remote <remote>"),
            "remote marker missing: {out}"
        );
        // Header breakdown: 2 local-alive (pid+ttl, the test pid is alive), 1
        // remote-alive, 1 stale.
        assert!(
            out.contains("4 session(s), 2 local-alive, 1 remote-alive, 1 stale"),
            "header breakdown wrong: {out}"
        );
    }

    /// Grouping by repo then branch: equal-key rows form one section with a header,
    /// and sections appear in sorted (repo, branch) order.
    #[test]
    fn dashboard_groups_by_repo_then_branch() {
        let rows = dashboard_rows_sorted(vec![
            row("z", "repoB", "main", true),
            row("a", "repoA", "main", true),
            row("m", "repoA", "feat", false),
        ]);
        let out =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        // Section headers present.
        assert!(out.contains("[repoA / feat]"), "{out}");
        assert!(out.contains("[repoA / main]"), "{out}");
        assert!(out.contains("[repoB / main]"), "{out}");
        // repoA sections precede repoB; within repoA, feat precedes main.
        let i_feat = out.find("[repoA / feat]").unwrap();
        let i_main = out.find("[repoA / main]").unwrap();
        let i_b = out.find("[repoB / main]").unwrap();
        assert!(i_feat < i_main && i_main < i_b, "ordering wrong: {out}");
        // Per-group alive count: repoA/main has 1 session, 1 alive.
        assert!(out.contains("[repoA / main] 1 session(s), 1 alive"));
    }

    /// Helper to sort rows like `dashboard_rows` does (the render assumes sorted,
    /// contiguous equal-key rows). Tests that build rows by hand reuse this.
    fn dashboard_rows_sorted(mut rows: Vec<SessionRow>) -> Vec<SessionRow> {
        rows.sort_by(|a, b| (&a.repo, &a.branch, &a.name).cmp(&(&b.repo, &b.branch, &b.name)));
        rows
    }

    /// `--repo`/`--branch` filters are echoed in the header; the caller pre-filters
    /// the rows, so a narrowed snapshot renders only the surviving group.
    #[test]
    fn dashboard_filter_echo_and_narrowing() {
        let rows = vec![row("a", "repoA", "main", true)];
        let opts = DashboardOpts {
            clear: false,
            repo: Some("repoA".to_string()),
            branch: Some("main".to_string()),
        };
        let out = render_sessions_dashboard(&rows, &opts, FIXED_HOST, FIXED_NOW);
        assert!(out.contains("repo=repoA"), "{out}");
        assert!(out.contains("branch=main"), "{out}");
        assert!(out.contains("[repoA / main]"));
        // A filter that excludes everything yields the stable empty body.
        let empty = render_sessions_dashboard(&[], &opts, FIXED_HOST, FIXED_NOW);
        assert!(empty.contains("no sessions"));
    }

    /// A group exceeding the row budget renders the budgeted rows plus `+N more`.
    #[test]
    fn dashboard_truncates_oversized_group() {
        let n = DASHBOARD_GROUP_ROW_BUDGET + 5;
        let rows: Vec<SessionRow> = (0..n)
            .map(|i| row(&format!("s{i:03}"), "repoA", "main", true))
            .collect();
        let rows = dashboard_rows_sorted(rows);
        let out =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        assert!(out.contains("+5 more"), "{out}");
        // Exactly the budgeted number of session rows are printed (each same-host
        // recent null-pid row reads "alive (local, ttl)").
        let shown = out.matches("[alive (local, ttl)]").count();
        assert_eq!(shown, DASHBOARD_GROUP_ROW_BUDGET);
    }

    /// alive/total accounting across the whole frame and per group.
    #[test]
    fn dashboard_alive_and_total_counts() {
        let rows = dashboard_rows_sorted(vec![
            row("a", "repoA", "main", true),
            row("b", "repoA", "main", false),
            row("c", "repoA", "main", true),
        ]);
        let out =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        // 3 sessions: 2 same-host recent (local-alive), 1 stale.
        assert!(out.contains("3 session(s), 2 local-alive, 0 remote-alive, 1 stale"));
        assert!(out.contains("[repoA / main] 3 session(s), 2 alive"));
    }

    /// ANSI-on vs plain: enabling the clear adds EXACTLY the clear-home prefix; the
    /// plain frame contains no escape byte at all.
    #[test]
    fn dashboard_ansi_prefix_vs_plain() {
        let rows = vec![row("a", "repoA", "main", true)];
        let plain =
            render_sessions_dashboard(&rows, &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        let ansi = render_sessions_dashboard(
            &rows,
            &DashboardOpts {
                clear: true,
                ..DashboardOpts::default()
            },
            FIXED_HOST,
            FIXED_NOW,
        );
        // Plain output has no ESC byte.
        assert!(
            !plain.as_bytes().contains(&0x1b),
            "plain frame had an escape"
        );
        // ANSI output is exactly the prefix + the plain frame.
        assert_eq!(ansi, format!("{ANSI_CLEAR_HOME}{plain}"));
        assert!(ansi.starts_with("\x1b[2J\x1b[H"));
    }

    /// Empty/missing tag fields render as `-` (never a blank column or a panic).
    #[test]
    fn dashboard_empty_tags_render_dash() {
        let mut r = row("a", "", "", true);
        r.worktree = String::new();
        r.mux = String::new();
        r.host = String::new();
        // host is now empty (≠ FIXED_HOST) so this row is AliveRemote — still a
        // deterministic, recent row; the dash-rendering of the tag columns is the
        // assertion under test, not the verdict.
        let out = render_sessions_dashboard(&[r], &DashboardOpts::default(), FIXED_HOST, FIXED_NOW);
        assert!(out.contains("[- / -]"), "{out}");
        assert!(out.contains("worktree=- mux=- host=-"), "{out}");
    }

    /// `liveness_for` delegates to `liveness_from_fields` with byte-identical results
    /// over a representative matrix (the #6 truth table). This pins the refactor: the
    /// field-level seam the dashboard uses must agree with the `Peer`-level classifier
    /// every other surface uses.
    #[test]
    fn liveness_for_matches_from_fields() {
        let now = FIXED_NOW;
        let mk = |host: &str, pid: Option<i64>, last_seen: i64| model::Peer {
            name: "p".to_string(),
            mux: String::new(),
            target: String::new(),
            socket: String::new(),
            cwd: None,
            last_seen,
            pid,
            host: host.to_string(),
            repo: String::new(),
            branch: String::new(),
            worktree_id: String::new(),
            circle: model::DEFAULT_CIRCLE.to_string(),
            role: model::PeerRole::Peer.as_str().to_string(),
            turn_state: String::new(),
            description: String::new(),
            description_ts: 0,
            birth_cert: None,
            contact_policy: "open".to_string(),
        };
        let cases: &[(&str, Option<i64>, i64)] = &[
            ("h1", None, now),                               // same host, recent, no pid
            ("h1", Some(std::process::id() as i64), now),    // same host, live pid
            ("h1", Some(1), now),                            // same host, (likely) dead pid
            ("h1", None, now - STALE_OFFSET),                // same host, stale
            ("other", None, now),                            // remote, recent
            ("other", Some(std::process::id() as i64), now), // remote, recent w/ pid
            ("other", None, now - STALE_OFFSET),             // remote, stale
            ("", None, now),                                 // empty host = remote
        ];
        for (host, pid, last_seen) in cases {
            let peer = mk(host, *pid, *last_seen);
            assert_eq!(
                store::liveness_for(&peer, FIXED_HOST, now),
                store::liveness_from_fields(host, *pid, *last_seen, FIXED_HOST, now),
                "delegation mismatch for host={host:?} pid={pid:?} last_seen={last_seen}",
            );
        }
    }

    /// HOOK BEST-EFFORT (P5): a turn_state write FAILURE is swallowed and can never
    /// sink the hook. Proven against a read-only store (every write traps): the
    /// underlying `set_turn_state` returns Err, yet `set_turn_state_best_effort`
    /// returns normally (no panic, no propagation) — the gc/git-tags precedent. Since
    /// the helper is the LAST statement in each hook arm (after the drain/registration
    /// `?`-paths), a swallowed setter cannot affect delivery. (sqlite-only because it
    /// builds a concrete `SqliteStore` read-only handle; the swallow logic itself is
    /// backend-agnostic, and the libsql read-only trap is proven in
    /// `presence_setters_trap_on_readonly_libsql`.)
    #[cfg(feature = "sqlite")]
    #[test]
    fn turn_state_best_effort_swallows_a_write_failure() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("weave-besteffort-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        // Seed the peer via a normal RW open, then drop it.
        {
            let s = store::SqliteStore::open(&path).unwrap();
            s.register_peer("a", "tmux", "%1", "", Some("/x")).unwrap();
        }
        // A read-only handle: any write (including set_turn_state) traps.
        let ro = store::SqliteStore::open_readonly(&path).unwrap();
        assert!(
            ro.set_turn_state("a", "working").is_err(),
            "precondition: a write through a read-only store must error"
        );
        // The best-effort wrapper swallows that error and returns normally — no panic,
        // no propagation. A hook arm calling this after its drain is unaffected.
        set_turn_state_best_effort(&ro, "a", model::TurnState::Working);
        // The failed write was a no-op (the row is untouched).
        assert_eq!(ro.get_peer("a").unwrap().unwrap().turn_state, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── WL-055: enforcing PreToolUse approval-gate drain logic ────────────────

    #[cfg(feature = "sqlite")]
    fn pretooluse_test_store(tag: &str) -> (store::SqliteStore, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-pretooluse-{tag}-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        (store::SqliteStore::open(&path).unwrap(), dir)
    }

    #[test]
    fn pretooluse_native_dangerous_predicate() {
        // Claude-native mutators are gated; weave's own dangerous MCP tools too.
        assert!(pretooluse_is_dangerous("Bash"));
        assert!(pretooluse_is_dangerous("Edit"));
        assert!(pretooluse_is_dangerous("Write"));
        assert!(pretooluse_is_dangerous("weave_spawn_peer"));
        // Read-only / benign tools are NOT gated.
        assert!(!pretooluse_is_dangerous("Read"));
        assert!(!pretooluse_is_dangerous("Glob"));
        assert!(!pretooluse_is_dangerous("weave_inbox"));
    }

    #[test]
    fn pretooluse_response_is_pure_decision_json() {
        let v = pretooluse_response("deny", "nope");
        assert_eq!(
            v.pointer("/hookSpecificOutput/hookEventName")
                .and_then(|x| x.as_str()),
            Some("PreToolUse")
        );
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecision")
                .and_then(|x| x.as_str()),
            Some("deny")
        );
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(|x| x.as_str()),
            Some("nope")
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_malformed_stdin_fails_open() {
        let (s, dir) = pretooluse_test_store("malformed");
        let cfg = Config::default();
        // payload_ok=false ⇒ defer (never break the session).
        let (decision, _) = pretooluse_decision(&s, &cfg, &serde_json::json!({}), false);
        assert_eq!(decision, "defer");
        // missing tool_name ⇒ also defer.
        let (decision, _) = pretooluse_decision(&s, &cfg, &serde_json::json!({"cwd": "/x"}), true);
        assert_eq!(decision, "defer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_safe_tool_defers() {
        let (s, dir) = pretooluse_test_store("safe");
        // even WITH an approver configured…
        let cfg = Config {
            pretooluse_approver: Some("approver".to_string()),
            ..Config::default()
        };
        let v = serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "/x"}});
        let (decision, _) = pretooluse_decision(&s, &cfg, &v, true);
        assert_eq!(decision, "defer"); // …a non-dangerous tool is never blocked.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_dangerous_no_approver_denies() {
        let (s, dir) = pretooluse_test_store("noapprover");
        let cfg = Config::default(); // no approver configured.
        let v = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}});
        let (decision, reason) = pretooluse_decision(&s, &cfg, &v, true);
        assert_eq!(decision, "deny", "deny-by-default with no approver");
        assert!(reason.contains("no approver"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_dangerous_denies_on_timeout() {
        let (s, dir) = pretooluse_test_store("timeout");
        let cfg = Config {
            pretooluse_approver: Some("approver".to_string()),
            pretooluse_timeout_secs: Some(1), // clamps to the 1s floor.
            ..Config::default()
        };
        let v = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "id"}});
        // No one answers ⇒ the drain's own short timeout fires ⇒ DENY (fail closed),
        // never relying on Claude's fail-open 600s.
        let (decision, reason) = pretooluse_decision(&s, &cfg, &v, true);
        assert_eq!(decision, "deny");
        assert!(reason.contains("within timeout"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_dangerous_allows_on_approve() {
        let (s, dir) = pretooluse_test_store("approve");
        let path = dir.join("t.db");
        let cfg = Config {
            pretooluse_approver: Some("approver".to_string()),
            pretooluse_timeout_secs: Some(5), // ample room for the approver thread.
            ..Config::default()
        };

        // A background "approver": poll for the freshly-opened ask and answer it
        // `approve` (a real second-session response over the same store).
        let approver_path = path.clone();
        let h = std::thread::spawn(move || {
            let approver = store::SqliteStore::open(&approver_path).unwrap();
            for _ in 0..200 {
                let asks = approver
                    .list_asks("approver", model::AskRole::Askee, 10)
                    .unwrap_or_default();
                if let Some(a) = asks
                    .iter()
                    .find(|a| a.kind == model::AskKind::ToolPermission)
                {
                    approver.answer("approver", &a.id, "approve").unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("approver never saw the PreToolUse ask");
        });

        let v = serde_json::json!({"tool_name": "Write", "tool_input": {"file_path": "/tmp/x"}});
        let (decision, reason) = pretooluse_decision(&s, &cfg, &v, true);
        h.join().unwrap();
        assert_eq!(
            decision, "allow",
            "an approve verdict ⇒ allow (reason: {reason})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn pretooluse_dangerous_denies_on_explicit_deny() {
        let (s, dir) = pretooluse_test_store("explicit-deny");
        let path = dir.join("t.db");
        let cfg = Config {
            pretooluse_approver: Some("approver".to_string()),
            pretooluse_timeout_secs: Some(5),
            ..Config::default()
        };

        let approver_path = path.clone();
        let h = std::thread::spawn(move || {
            let approver = store::SqliteStore::open(&approver_path).unwrap();
            for _ in 0..200 {
                let asks = approver
                    .list_asks("approver", model::AskRole::Askee, 10)
                    .unwrap_or_default();
                if let Some(a) = asks
                    .iter()
                    .find(|a| a.kind == model::AskKind::ToolPermission)
                {
                    // Anything other than "approve" ⇒ a Denied verdict.
                    approver.answer("approver", &a.id, "no").unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("approver never saw the PreToolUse ask");
        });

        let v = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "id"}});
        let (decision, _) = pretooluse_decision(&s, &cfg, &v, true);
        h.join().unwrap();
        assert_eq!(decision, "deny", "an explicit non-approve answer ⇒ deny");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
