//! Best-effort git session tagging: derive a session's **repo name**, **branch**,
//! and a **canonical, path-stable worktree id** from its cwd, so a `peers` row is
//! self-describing across the mesh/federation.
//!
//! Two acquisition halves, both pure-parse-first and unit-testable over fixture
//! strings (no subprocess, no network, no real-repo mutation in the unit layer):
//!
//! 1. **`.git`-file parse FIRST** (hermetic, zero-subprocess): every weave session
//!    is a *linked* git worktree (the develop-base ritual), so `<cwd>/.git` is a
//!    plain file containing `gitdir: …/.git/worktrees/<name>/.git`. A single
//!    `std::fs::read` + [`parse_worktree_id_from_gitdir`] recovers the canonical
//!    worktree id with no `git` binary at all. A main (non-linked) worktree has a
//!    `.git` *directory* → the canonical id is the literal `"(main)"` sentinel.
//!
//! 2. **argv `git` FALLBACK** for branch + repo name: spawn the TRUSTED absolute
//!    `git` (resolved via [`weave_inject::resolve_trusted`], the same trusted-dir
//!    discipline the injector uses) with an **explicit argv** vector, a wall-clock
//!    timeout, and `Stdio::null()` stderr — exactly like `inject::run_capture`.
//!    NEVER `sh -c`, never a built command string: cwd/repo/branch text never
//!    reaches a shell.
//!
//! Acquisition is **best-effort and total**: any fs/git failure (or a non-git cwd)
//! yields empty tags — it must NEVER sink registration (the hook hot path). The
//! store seam ([`crate::store::sanitize_tag`]) bounds + control-strips every tag,
//! so this module returns raw captured strings and trusts the store to clamp them.

use std::path::Path;
use std::time::Duration;

/// Wall-clock cap on each `git` subprocess. Generous enough for a cold repo, short
/// enough that a wedged/networked git cannot stall the hook hot path. Mirrors the
/// injector's bounded-probe discipline.
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

pub use weave_core::model::WorktreeTags;

/// Extract the canonical worktree id `<name>` from the contents of a *linked*
/// worktree's `.git` FILE, which git writes as a single line:
///
/// ```text
/// gitdir: /abs/main/.git/worktrees/<name>/.git
/// ```
///
/// Returns `Some("<name>")` for that shape, or `None` for anything else (a `.git`
/// directory's contents, garbage, or a `gitdir:` not pointing under
/// `.git/worktrees/`). Pure; the caller maps `None` to the `"(main)"` sentinel
/// (when `.git` is a directory ⇒ main worktree) or to `""` (non-git).
pub fn parse_worktree_id_from_gitdir(contents: &str) -> Option<String> {
    // Find the `gitdir:` line (the .git file is normally a single such line, but
    // scan all lines to be robust to trailing whitespace/newlines).
    let rest = contents
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("gitdir:"))?
        .trim();
    // Take the `<name>` of `.git/worktrees/<name>/...`. Splitting on the segment is
    // robust to a trailing `/.git` (or its absence). A `gitdir:` not under
    // `.git/worktrees/` (e.g. a submodule's `.git/modules/...`) yields `None`.
    let after = rest.split("/.git/worktrees/").nth(1)?;
    let name = after.split('/').next().unwrap_or("").trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Parse `git worktree list --porcelain` and return the **branch** recorded for the
/// worktree whose `worktree <path>` stanza matches `for_path` (canonicalized-string
/// compare is the caller's job; here we match the literal path field). A detached
/// HEAD stanza (no `branch` line) yields `Some("")`. Returns `None` when no stanza
/// matches `for_path`. Pure; fixture-testable.
///
/// Porcelain stanzas are blank-line-separated; each starts with `worktree <path>`
/// and may carry `HEAD <sha>` and `branch refs/heads/<name>`. We surface the short
/// branch name (`refs/heads/` prefix stripped).
///
/// This is the branch fallback used by [`capture_worktree_tags`] when
/// `rev-parse --abbrev-ref HEAD` yields nothing (e.g. it errors or returns blank):
/// it recovers the branch for the current worktree path from
/// `git worktree list --porcelain`. Kept pure and exhaustively fixture-tested.
pub fn parse_worktree_porcelain(out: &str, for_path: &str) -> Option<String> {
    let mut cur_path: Option<&str> = None;
    let mut cur_branch: Option<String> = None;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            // Stanza boundary: a matching path resolves now (detached ⇒ "").
            if cur_path == Some(for_path) {
                return Some(cur_branch.take().unwrap_or_default());
            }
            cur_path = None;
            cur_branch = None;
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(p.trim());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let short = b.trim().strip_prefix("refs/heads/").unwrap_or(b.trim());
            cur_branch = Some(short.to_string());
        }
    }
    // Flush the final stanza (porcelain may omit a trailing blank line).
    if cur_path == Some(for_path) {
        return Some(cur_branch.unwrap_or_default());
    }
    None
}

/// Derive the repo name (basename of a git toplevel path) from a
/// `rev-parse --show-toplevel` line. Pure: trims, takes the last path component.
/// Empty input / no component ⇒ `""`.
pub fn repo_name_from_toplevel(toplevel: &str) -> String {
    let t = toplevel.trim();
    Path::new(t)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Capture the [`WorktreeTags`] for `cwd`, best-effort. NEVER errors: a non-git
/// cwd or any fs/git failure yields empty tags (with `worktree_id` falling back to
/// `"(main)"` only when `<cwd>/.git` is a directory, i.e. the cwd really is a git
/// repo's main worktree). Capture is cheap on the hot path: the primary worktree-id
/// path is a single `std::fs::read`; the branch/repo `git` calls are timeout-bounded
/// and skipped entirely when no trusted `git` is available.
pub fn capture_worktree_tags(cwd: &Path) -> WorktreeTags {
    let mut tags = WorktreeTags::default();

    // --- worktree_id: pure `.git`-file parse first (zero subprocess) ---
    let dot_git = cwd.join(".git");
    match std::fs::metadata(&dot_git) {
        Ok(meta) if meta.is_file() => {
            // Linked worktree: parse `gitdir: …/.git/worktrees/<name>/.git`.
            if let Ok(contents) = std::fs::read_to_string(&dot_git) {
                if let Some(id) = parse_worktree_id_from_gitdir(&contents) {
                    tags.worktree_id = id;
                }
            }
        }
        Ok(meta) if meta.is_dir() => {
            // Main (non-linked) worktree: stable canonical id is the sentinel.
            tags.worktree_id = "(main)".to_string();
        }
        // No `.git` at all (or unreadable): not a git repo's worktree → leave "".
        _ => {}
    }

    // If the cwd is not a git repo at all, there is nothing more to capture and we
    // skip the subprocess entirely (cheap negative path).
    if tags.worktree_id.is_empty() {
        return tags;
    }

    // --- branch + repo via the TRUSTED argv `git` runner (best-effort) ---
    if let Some(branch) = git_capture(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        let b = branch.trim();
        // `HEAD` here means detached: leave branch empty rather than literal "HEAD".
        if !b.is_empty() && b != "HEAD" {
            tags.branch = b.to_string();
        }
    }
    // Branch fallback: when `rev-parse --abbrev-ref HEAD` yields nothing (errored or
    // returned blank), recover the branch from `git worktree list --porcelain` for
    // THIS worktree's path. Same no-shell argv runner; pure-parsed by
    // [`parse_worktree_porcelain`]. Robust against path normalization by trying the
    // canonicalized cwd first, then the raw cwd string.
    if tags.branch.is_empty() {
        if let Some(out) = git_capture(cwd, &["worktree", "list", "--porcelain"]) {
            let canon = std::fs::canonicalize(cwd)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let raw = cwd.to_string_lossy();
            let branch = canon
                .as_deref()
                .and_then(|p| parse_worktree_porcelain(&out, p))
                .or_else(|| parse_worktree_porcelain(&out, &raw));
            if let Some(b) = branch {
                if !b.is_empty() {
                    tags.branch = b;
                }
            }
        }
    }
    if let Some(top) = git_capture(cwd, &["rev-parse", "--show-toplevel"]) {
        tags.repo = repo_name_from_toplevel(&top);
    }

    tags
}

/// Run `git <args...>` in `cwd` via the TRUSTED absolute `git` binary with an
/// explicit argv vector (no shell, no command string), a wall-clock timeout, and
/// null stderr. Returns the trimmed stdout on exit 0, or `None` on any failure
/// (git not in a trusted dir, spawn error, non-zero exit, timeout). Mirrors
/// `inject::run_capture`'s spawn/timeout/kill discipline.
fn git_capture(cwd: &Path, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    // Resolve `git` to a trusted absolute path (never ambient $PATH); also how
    // tests point at a fake git via WEAVE_MUX_DIR. Absent ⇒ skip the subprocess.
    let git = weave_inject::resolve_trusted("git")?;
    let mut child = Command::new(&git)
        .args(args)
        .current_dir(cwd)
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
                return if status.success() { Some(buf) } else { None };
            }
            Ok(None) => {
                if start.elapsed() >= GIT_TIMEOUT {
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
    fn gitdir_linked_worktree_yields_name() {
        let c = "gitdir: /home/u/weave/.git/worktrees/session-scan/.git\n";
        assert_eq!(
            parse_worktree_id_from_gitdir(c),
            Some("session-scan".to_string())
        );
    }

    #[test]
    fn gitdir_handles_no_trailing_dotgit_and_whitespace() {
        let c = "  gitdir: /a/b/.git/worktrees/wt-7  \n";
        assert_eq!(parse_worktree_id_from_gitdir(c), Some("wt-7".to_string()));
    }

    #[test]
    fn gitdir_garbage_or_main_dir_is_none() {
        assert_eq!(parse_worktree_id_from_gitdir(""), None);
        assert_eq!(parse_worktree_id_from_gitdir("ref: refs/heads/x\n"), None);
        // A `gitdir:` not under .git/worktrees/ (e.g. a submodule) is not a
        // linked-worktree id.
        assert_eq!(
            parse_worktree_id_from_gitdir("gitdir: /a/.git/modules/sub\n"),
            None
        );
    }

    #[test]
    fn porcelain_matches_path_and_strips_refs_heads() {
        let out = "\
worktree /home/u/weave
HEAD abc123
branch refs/heads/master

worktree /home/u/weave-session-scan
HEAD def456
branch refs/heads/feat/session-scan-tag
";
        assert_eq!(
            parse_worktree_porcelain(out, "/home/u/weave-session-scan"),
            Some("feat/session-scan-tag".to_string())
        );
        assert_eq!(
            parse_worktree_porcelain(out, "/home/u/weave"),
            Some("master".to_string())
        );
        // A path not present yields None.
        assert_eq!(parse_worktree_porcelain(out, "/nope"), None);
    }

    #[test]
    fn porcelain_detached_head_is_empty_branch() {
        // A detached stanza has no `branch` line ⇒ "".
        let out = "worktree /x/detached\nHEAD deadbeef\n";
        assert_eq!(
            parse_worktree_porcelain(out, "/x/detached"),
            Some(String::new())
        );
    }

    #[test]
    fn repo_name_is_toplevel_basename() {
        assert_eq!(repo_name_from_toplevel("/home/u/weave\n"), "weave");
        assert_eq!(repo_name_from_toplevel("  /a/b/c  "), "c");
        assert_eq!(repo_name_from_toplevel(""), "");
    }

    #[test]
    fn capture_non_git_cwd_yields_empty_tags() {
        // A temp dir with no `.git` is not a git repo: all tags empty, never panics.
        let dir = std::env::temp_dir().join(format!(
            "weave-git-nongit-{}-{}",
            std::process::id(),
            weave_core::model::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tags = capture_worktree_tags(&dir);
        assert_eq!(tags, WorktreeTags::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_linked_worktree_gitfile_yields_id_without_git_binary() {
        // Craft a temp dir whose `.git` is a FILE (linked-worktree shape). The
        // worktree id is recovered purely from the file — no `git` binary needed.
        let dir = std::env::temp_dir().join(format!(
            "weave-git-linked-{}-{}",
            std::process::id(),
            weave_core::model::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".git"),
            "gitdir: /somewhere/.git/worktrees/my-wt/.git\n",
        )
        .unwrap();
        let tags = capture_worktree_tags(&dir);
        assert_eq!(tags.worktree_id, "my-wt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_main_worktree_dir_yields_sentinel() {
        // A `.git` DIRECTORY ⇒ main worktree ⇒ "(main)" sentinel.
        let dir = std::env::temp_dir().join(format!(
            "weave-git-main-{}-{}",
            std::process::id(),
            weave_core::model::now()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let tags = capture_worktree_tags(&dir);
        assert_eq!(tags.worktree_id, "(main)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
