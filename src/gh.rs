//! Best-effort PR state lookup via the `gh` CLI as an EXTERNAL TRUSTED BINARY.
//!
//! weave is daemon-free and dependency-light, so it links NO HTTP client. To learn
//! a PR's current head SHA + lifecycle state (for the P7 review queue's per-row
//! `my_action`), the **client process** invokes the user's `gh` CLI exactly the way
//! [`crate::git`] invokes `git`:
//!
//! - resolve `gh` to a TRUSTED absolute path via [`crate::inject::resolve_trusted`]
//!   (NEVER ambient `$PATH`; also how tests point at a fake `gh` via `WEAVE_MUX_DIR`),
//! - spawn it with an EXPLICIT argv vector — `gh pr view <url> --json headRefOid,state`
//!   — NEVER `sh -c`, never a built command string: the PR url (even with shell
//!   metacharacters) is a single inert argv element and never reaches a shell,
//! - bound it with a wall-clock timeout + kill, `Stdio::null()` stderr+stdin.
//!
//! Pure-parse-first + total: a pure [`parse_gh_pr_json`] turns gh's `--json` output
//! into `(head_sha, PrState)` and is fixture-tested with ZERO subprocess. The impure
//! [`gh_pr_info`] is best-effort — gh absent / unauthenticated / offline / timeout /
//! non-zero / bad-JSON / bad-url ⇒ `state=Unknown, head_sha=None`, the graceful
//! fallback that drives `my_action=unknown`. It NEVER errors and NEVER sinks a
//! listing.
//!
//! gh auth is gh's OWN concern: weave never reads, handles, or stores a token —
//! the only thing recorded is secret-free url/sha/state/title metadata.

use crate::model::{pr_url_valid, PrState};
use std::time::Duration;

/// Wall-clock cap on the `gh` subprocess. Matches repowire's 10s PR-fetch timeout;
/// generous for a cold network call, short enough that a wedged/offline gh cannot
/// stall a `review queue` read. Mirrors [`crate::git`]'s bounded-probe discipline.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// The best-effort live state of a PR. `head_sha` is the PR's current head commit
/// (`None` when gh could not report one); `state` is its [`PrState`] (`Unknown` on
/// any failure). Pure data — the caller pairs it with the recorded review sha and
/// derives `my_action` via [`crate::model::compute_my_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhPrInfo {
    pub head_sha: Option<String>,
    pub state: PrState,
}

impl GhPrInfo {
    /// The total fallback: gh absent/offline/errored ⇒ no head, unknown state.
    fn unknown() -> Self {
        GhPrInfo {
            head_sha: None,
            state: PrState::Unknown,
        }
    }
}

/// Map gh's `state` string (`OPEN`/`MERGED`/`CLOSED`, case-insensitive) to a
/// [`PrState`]; anything unrecognized ⇒ `Unknown`. Pure.
fn pr_state_from_str(s: &str) -> PrState {
    match s.trim().to_ascii_lowercase().as_str() {
        "open" => PrState::Open,
        "merged" => PrState::Merged,
        "closed" => PrState::Closed,
        _ => PrState::Unknown,
    }
}

/// PURE parse of `gh pr view <url> --json headRefOid,state` output (a single JSON
/// object). Returns `Some((head_sha, state))` when the JSON parses to an object with
/// a usable `state` (and, when present, a non-empty `headRefOid` head SHA), or `None`
/// for empty/garbage/non-object input. Robust to gh's exact field set:
///   * `state` is GitHub's `OPEN`/`MERGED`/`CLOSED` (gh's `pr view --json state`
///     reports `MERGED` distinctly, so no separate `merged` bool is needed);
///   * `headRefOid` is the head SHA (empty/absent ⇒ `None` head, still a usable row).
///
/// Fixture-tested with ZERO subprocess (the [`crate::git`] pure-parse-first model).
pub fn parse_gh_pr_json(stdout: &str) -> Option<(Option<String>, PrState)> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let obj = v.as_object()?;
    // `state` is required to classify; an object lacking it is not a usable PR view.
    let state = pr_state_from_str(obj.get("state").and_then(|s| s.as_str())?);
    let head = obj
        .get("headRefOid")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((head, state))
}

/// Best-effort live PR state for `pr_url` via the `gh` CLI. TOTAL: returns
/// `GhPrInfo { head_sha: None, state: Unknown }` on EVERY failure path — invalid url,
/// gh not in a trusted dir, spawn error, non-zero exit (unauth/offline), timeout, or
/// unparseable JSON. NEVER errors; a single failed lookup yields `unknown` for that
/// one row and never sinks the surrounding listing.
///
/// The `gh` subprocess is spawned EXACTLY like [`crate::git`]'s `git_capture`: a
/// trusted absolute path + an explicit argv vector (NO shell, NO command string),
/// `Stdio::null()` stderr+stdin, a wall-clock timeout with `try_wait`/`kill`/`wait`.
/// The `pr_url` is a single inert argv element behind the `pr view` subcommand — it
/// never reaches a shell even if it bears metacharacters.
pub fn gh_pr_info(pr_url: &str) -> GhPrInfo {
    // Cheap reject FIRST: a malformed/oversized url never reaches the subprocess.
    if !pr_url_valid(pr_url) {
        return GhPrInfo::unknown();
    }
    match gh_capture(&["pr", "view", pr_url, "--json", "headRefOid,state"]) {
        Some(out) => match parse_gh_pr_json(&out) {
            Some((head_sha, state)) => GhPrInfo { head_sha, state },
            None => GhPrInfo::unknown(),
        },
        None => GhPrInfo::unknown(),
    }
}

/// Run `gh <args...>` via the TRUSTED absolute `gh` binary with an explicit argv
/// vector (no shell, no command string), a wall-clock timeout, and null stderr/stdin.
/// Returns the trimmed stdout on exit 0, or `None` on any failure (gh not in a
/// trusted dir, spawn error, non-zero exit, timeout). A VERBATIM mirror of
/// [`crate::git`]'s `git_capture` — the same trusted-binary discipline.
fn gh_capture(args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    // Resolve `gh` to a trusted absolute path (never ambient $PATH); also how tests
    // point at a fake gh via WEAVE_MUX_DIR. Absent ⇒ skip the subprocess ⇒ unknown.
    let gh = crate::inject::resolve_trusted("gh")?;
    let mut child = Command::new(&gh)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut buf);
                }
                return if status.success() {
                    Some(buf.trim().to_string())
                } else {
                    None
                };
            }
            Ok(None) => {
                if start.elapsed() >= GH_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_open_pr_with_head() {
        let json = r#"{"headRefOid":"abc123def","state":"OPEN"}"#;
        assert_eq!(
            parse_gh_pr_json(json),
            Some((Some("abc123def".to_string()), PrState::Open))
        );
    }

    #[test]
    fn parse_merged_pr() {
        let json = r#"{"headRefOid":"deadbeef","state":"MERGED"}"#;
        assert_eq!(
            parse_gh_pr_json(json),
            Some((Some("deadbeef".to_string()), PrState::Merged))
        );
    }

    #[test]
    fn parse_closed_pr() {
        let json = r#"{"headRefOid":"cafef00d","state":"CLOSED"}"#;
        assert_eq!(
            parse_gh_pr_json(json),
            Some((Some("cafef00d".to_string()), PrState::Closed))
        );
    }

    #[test]
    fn parse_missing_head_is_usable_with_none() {
        // A PR view lacking headRefOid is still classifiable; head is None.
        let json = r#"{"state":"OPEN"}"#;
        assert_eq!(parse_gh_pr_json(json), Some((None, PrState::Open)));
        // Empty-string head normalizes to None.
        let json2 = r#"{"headRefOid":"","state":"OPEN"}"#;
        assert_eq!(parse_gh_pr_json(json2), Some((None, PrState::Open)));
    }

    #[test]
    fn parse_unknown_state_maps_to_unknown() {
        let json = r#"{"headRefOid":"abc","state":"DRAFTish"}"#;
        assert_eq!(
            parse_gh_pr_json(json),
            Some((Some("abc".to_string()), PrState::Unknown))
        );
    }

    #[test]
    fn parse_garbage_or_no_state_is_none() {
        assert_eq!(parse_gh_pr_json(""), None);
        assert_eq!(parse_gh_pr_json("not json"), None);
        assert_eq!(parse_gh_pr_json("[1,2,3]"), None); // not an object
        assert_eq!(parse_gh_pr_json(r#"{"headRefOid":"abc"}"#), None); // no state
    }

    /// HERMETIC bad-url path: a malformed url is rejected BEFORE any `resolve_trusted`
    /// or subprocess, so `gh_pr_info` returns the unknown fallback with ZERO network
    /// and ZERO dependency on whether a real `gh` is installed. This exercises the
    /// "graceful unknown" contract deterministically on any runner.
    #[test]
    fn gh_bad_url_yields_unknown_without_subprocess() {
        for bad in [
            "",
            "not a url",
            "github.com/o/r/pull/1",                 // no scheme
            "https://github.com/owner/repo",         // not a /pull/ url
            "https://github.com/o/r/pull/1; rm -rf", // metachar + space
        ] {
            let info = gh_pr_info(bad);
            assert_eq!(info, GhPrInfo::unknown(), "url {bad:?} must yield unknown");
            assert_eq!(info.state, PrState::Unknown);
            assert!(info.head_sha.is_none());
        }
    }

    /// gh-ABSENT path: with `WEAVE_MUX_DIR` pointed at an empty dir AND `PATH` scrubbed,
    /// `resolve_trusted("gh")` returns None UNLESS the runner happens to ship a real
    /// `gh` in a hardcoded system dir (`/usr/bin`, …) which `resolve_trusted` also
    /// scans. So we ONLY assert the no-subprocess unknown when gh is genuinely absent;
    /// otherwise we skip (still HERMETIC — never spawns, never hits the network).
    /// Serialized on the canonical env lock since env is process-global.
    #[test]
    fn gh_absent_yields_unknown() {
        let _g = crate::testenv::lock_env();
        let empty = std::env::temp_dir().join(format!(
            "weave-gh-absent-{}-{}",
            std::process::id(),
            crate::model::now()
        ));
        std::fs::create_dir_all(&empty).unwrap();
        let _mux = crate::testenv::EnvVarGuard::set("WEAVE_MUX_DIR", &empty.to_string_lossy());
        let _path = crate::testenv::EnvVarGuard::set("PATH", &empty.to_string_lossy());

        // Only meaningful when no `gh` resolves anywhere trusted (no system gh).
        if crate::inject::resolve_trusted("gh").is_none() {
            let info = gh_pr_info("https://github.com/owner/repo/pull/1");
            assert_eq!(info, GhPrInfo::unknown());
            assert_eq!(info.state, PrState::Unknown);
            assert!(info.head_sha.is_none());
        }
        let _ = std::fs::remove_dir_all(&empty);
    }
}
