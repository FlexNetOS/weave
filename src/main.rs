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

mod config;
mod gh;
mod git;
mod inject;
mod mcp;
mod model;
mod setup;
#[cfg(feature = "sign")]
mod sign;
mod store;
#[cfg(feature = "libsql")]
mod store_libsql;
#[cfg(test)]
mod testenv;

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
    /// PR review queue (P7): record your reviews and poll their live state.
    /// `weave review mark <pr_url> [sha]` records a review (sha omitted ⇒ best-effort
    /// gh fill); `weave review queue [--offline]` lists your reviews with a derived
    /// my_action (none-needed | re-review-suggested | merged-since-review |
    /// closed-since-review | unknown). Live state needs the `gh` CLI; absent ⇒ unknown.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
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
        /// prior correlation id this ask chains/closes
        #[arg(long = "reply-to")]
        reply_to: Option<String>,
        #[arg(long)]
        from: Option<String>,
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
    /// `weave hook session|prompt|stop|notification`; this is the manual override.
    /// Self-only. Valid states: pending_first_turn|working|awaiting_input|idle.
    Status {
        /// turn-state label (pending_first_turn|working|awaiting_input|idle)
        state: String,
        #[arg(long)]
        me: Option<String>,
    },
    /// Claude Code lifecycle hook: session|prompt|stop|notification (reads JSON on stdin).
    Hook { event: String },
}

/// `weave review` subcommands (P7). Daemon-free PR review tracking: record your own
/// reviews (owner-only) and poll their live state via the external `gh` CLI.
#[derive(Subcommand)]
enum ReviewCmd {
    /// Record (OWNER-ONLY) that YOU reviewed a PR at a commit SHA. `sha` is optional:
    /// when omitted, weave best-effort fills the PR's current head via `gh` (gh
    /// absent ⇒ no sha recorded ⇒ every future read suggests a re-review). The PR url
    /// and sha reach `gh` only as inert argv elements — never a shell.
    Mark {
        /// the PR url you reviewed (http(s) GitHub /pull/ URL)
        pr_url: String,
        /// the commit SHA you reviewed (7..=64 hex); omit to best-effort gh-fill
        sha: Option<String>,
        /// optional owner/repo metadata
        #[arg(long)]
        repo: Option<String>,
        /// optional PR title metadata
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        me: Option<String>,
    },
    /// List YOUR recorded reviews with a derived my_action (none-needed |
    /// re-review-suggested | merged-since-review | closed-since-review | unknown).
    /// Live state is fetched best-effort per row via `gh`; `--offline` skips it (all
    /// rows report unknown). Read-only.
    Queue {
        /// skip the gh calls entirely (all rows report my_action=unknown)
        #[arg(long)]
        offline: bool,
        /// machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// max reviews to list (clamped to a sane cap)
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long)]
        me: Option<String>,
    },
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
        _ => match inject::inject(&t, &cfg.nudge(from, body)) {
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
            match inject::inject(&t, &cfg.nudge(from, body)) {
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
    let target = inject::detect_target();
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
                policy: verify_policy(&cfg),
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
                    let _ = inject_and_trace(
                        store,
                        &cfg,
                        mid,
                        model::DeliveryRefKind::Message,
                        &from,
                        &to,
                        &body,
                    )?;
                }
            }
        }

        Cmd::Notify {
            from,
            to,
            subject,
            body,
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
            let mid = store.send(&from, &to, subject.as_deref(), &body)?;
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

        Cmd::Ask {
            to,
            body,
            subject,
            reply_to,
            from,
        } => {
            let (from, explicit) = resolve_me_explicit(from, None, &cfg);
            refresh_presence(store, &from, explicit);
            if model::is_broadcast(&to) {
                anyhow::bail!(
                    "tracked ask is point-to-point; use `weave send` for broadcast (broadcast ask is P2)."
                );
            }
            let (cid, _qid) =
                store.ask(&from, &to, subject.as_deref(), &body, reply_to.as_deref())?;
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
                let t = inject::detect_target();
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
                let t = inject::detect_target();
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
            let t = inject::detect_target();
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
            store.register_peer_full(
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
            // liveness (the whole point of zero-restart attach), plus the git tags.
            let tags = git_tags_for(cwd_val.as_deref());
            store.register_peer_full(
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

        Cmd::Review { cmd } => handle_review(store, &cfg, cmd)?,

        Cmd::Hook { event } => handle_hook(store, &cfg, &event)?,
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
            match st.holder {
                Some(h) if st.present => {
                    println!(
                        "orchestrator present in circle '{}': '{}' (online)",
                        st.circle, h.name
                    );
                }
                _ => println!("no live orchestrator in circle '{}'", st.circle),
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

/// P7 `weave review {mark,queue}` handler. The ONLY place store + gh meet (no
/// store→gh edge): record an owner-only review, or list reviews and derive each
/// row's my_action PURELY from the PR's best-effort live state.
fn handle_review(store: &dyn Store, cfg: &Config, cmd: ReviewCmd) -> Result<()> {
    match cmd {
        ReviewCmd::Mark {
            pr_url,
            sha,
            repo,
            title,
            me,
        } => {
            let me = resolve_me(me, None, cfg);
            store::check_ident("name", &me)?;
            // Validate the url shape + bound BEFORE any DB bind / gh spawn.
            if !model::pr_url_valid(&pr_url) {
                anyhow::bail!(
                    "'{pr_url}' is not a valid PR url (expected an http(s) GitHub /pull/ URL, \
                     max {} chars)",
                    model::MAX_PR_URL_LEN
                );
            }
            // sha optional: when provided, must be hex + bounded; when omitted,
            // best-effort gh-fill the PR's current head (gh absent ⇒ '').
            let sha = match sha.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(s) => {
                    if !model::sha_valid(s) {
                        anyhow::bail!(
                            "'{s}' is not a valid commit SHA (7..={} hex digits)",
                            model::MAX_SHA_LEN
                        );
                    }
                    s.to_string()
                }
                None => gh::gh_pr_info(&pr_url)
                    .head_sha
                    .filter(|s| model::sha_valid(s))
                    .unwrap_or_default(),
            };
            let repo = repo.as_deref().unwrap_or("");
            let title = title.as_deref().unwrap_or("");
            store.mark_reviewed(&me, &pr_url, &sha, repo, title)?;
            if sha.is_empty() {
                println!(
                    "recorded review of {pr_url} as '{me}' (no sha — re-review will be suggested)"
                );
            } else {
                println!("recorded review of {pr_url} at {sha} as '{me}'");
            }
        }
        ReviewCmd::Queue {
            offline,
            json,
            limit,
            me,
        } => {
            let me = resolve_me(me, None, cfg);
            store::check_ident("name", &me)?;
            let rows = store.list_reviews(&me, limit)?;
            // Best-effort live state per row (skipped when offline). gh failure or
            // offline ⇒ unknown for that row; the listing always renders.
            let enriched: Vec<(model::ReviewRow, gh::GhPrInfo, model::MyAction)> = rows
                .into_iter()
                .map(|r| {
                    let info = if offline {
                        gh::GhPrInfo {
                            head_sha: None,
                            state: model::PrState::Unknown,
                        }
                    } else {
                        gh::gh_pr_info(&r.pr_url)
                    };
                    let action = model::compute_my_action(
                        &r.reviewed_sha,
                        info.head_sha.as_deref(),
                        info.state,
                    );
                    (r, info, action)
                })
                .collect();
            if json {
                let arr: Vec<_> = enriched
                    .iter()
                    .map(|(r, info, action)| {
                        serde_json::json!({
                            "pr_url": r.pr_url,
                            "last_reviewed_sha": r.reviewed_sha,
                            "current_head_sha": info.head_sha,
                            "state": info.state.as_str(),
                            "my_action": action.as_str(),
                            "repo": r.repo,
                            "title": r.title,
                            "reviewed_ts": r.reviewed_ts,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "reviewer": me,
                        "reviews": arr,
                        "count": enriched.len(),
                    }))?
                );
            } else if enriched.is_empty() {
                println!("0 recorded review(s) for '{me}'");
            } else {
                println!("{} recorded review(s) for '{me}':", enriched.len());
                for (r, info, action) in &enriched {
                    let head = info.head_sha.as_deref().unwrap_or("-");
                    let reviewed = if r.reviewed_sha.is_empty() {
                        "-"
                    } else {
                        &r.reviewed_sha
                    };
                    println!(
                        "  {} [reviewed {reviewed} / head {head} / {}] => {}",
                        r.pr_url,
                        info.state.as_str(),
                        action.as_str()
                    );
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
            // real process-liveness for this hook-registered session, plus the git
            // tags. The hot path stays cheap: tag capture is a single fs read
            // primary + a timeout-bounded best-effort git fallback that never sinks
            // the hook.
            let tags = git_tags_for(cwd);
            store.register_peer_full(
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
        }
        // Notification: the agent's prompt is live + unconsumed (awaiting input).
        // This arm has no drain — just the best-effort turn_state setter.
        "notification" => {
            set_turn_state_best_effort(store, &me, model::TurnState::AwaitingInput);
        }
        other => eprintln!("[weave] unknown hook event: {other}"),
    }
    Ok(())
}

/// Best-effort turn_state update for the hook hot path (P5). The setter targets the
/// CALLER's own row only and is SWALLOWED on error (to stderr) so a turn_state write
/// can never sink message delivery or registration — the gc/git-tags precedent.
fn set_turn_state_best_effort(store: &dyn Store, me: &str, state: model::TurnState) {
    if let Err(e) = store.set_turn_state(me, state.as_str()) {
        eprintln!("[weave] turn_state update skipped (non-fatal): {e}");
    }
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
}
