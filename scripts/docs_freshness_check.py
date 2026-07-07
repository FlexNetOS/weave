#!/usr/bin/env python3
"""WL-076 backlog/docs freshness gate.

A pull request that touches Weave's operator-visible behavior must either update
CHANGELOG.md / .handoff/loop/backlog.md or carry an explicit no-doc-change marker
in the PR body (or commit messages for local use).
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

DOC_PATHS = {"CHANGELOG.md", ".handoff/loop/backlog.md"}
NO_DOC_MARKERS = {
    "[no backlog/doc change]",
    "[no docs/backlog change]",
    "NO_BACKLOG_DOC_CHANGE",
    "NO_DOC_BACKLOG_CHANGE",
}
USER_VISIBLE_PREFIXES = (
    "weave/src/",
    "weave-core/src/",
    "weave-mcp/src/",
    "weave-inject/src/",
    "weave/tests/",
    "weave-core/tests/",
    "weave-mcp/tests/",
    "weave-inject/tests/",
    "docs/",
    ".github/workflows/",
)
USER_VISIBLE_EXACT = {
    "Cargo.toml",
    "Cargo.lock",
    "README.md",
    "ARCHITECTURE.md",
    "deny.toml",
    "scripts/docs_freshness_check.py",
}


def run(args: list[str]) -> str:
    return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL).strip()


def event_base_sha() -> str:
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        return ""
    try:
        event = json.loads(Path(event_path).read_text())
    except Exception:
        return ""
    return event.get("pull_request", {}).get("base", {}).get("sha", "")


def resolve_base_ref(base_ref: str | None) -> str | None:
    if base_ref is not None:
        return base_ref
    env_base = os.environ.get("GITHUB_BASE_REF")
    return event_base_sha() or (f"origin/{env_base}" if env_base else None)


def changed_files(base_ref: str | None, head_ref: str | None) -> list[str]:
    base_ref = resolve_base_ref(base_ref)
    if head_ref is None:
        head_ref = "HEAD"
    candidates: list[list[str]] = []
    if base_ref:
        candidates.append(["git", "diff", "--name-only", f"{base_ref}...{head_ref}"])
        candidates.append(["git", "diff", "--name-only", f"{base_ref}", head_ref])
    candidates.append(["git", "diff", "--name-only", "--cached"])
    candidates.append(["git", "diff", "--name-only"])
    for candidate in candidates:
        try:
            out = run(candidate)
        except Exception:
            continue
        files = [line for line in out.splitlines() if line]
        if files:
            return sorted(set(files))
    return []


def is_user_visible(path: str) -> bool:
    if path in DOC_PATHS or path in USER_VISIBLE_EXACT:
        return True
    return path.startswith(USER_VISIBLE_PREFIXES)


def has_no_doc_marker(text: str) -> bool:
    upper_text = text.upper()
    return any(marker in text or marker in upper_text for marker in NO_DOC_MARKERS)


def pr_body_marker() -> str:
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        return ""
    try:
        event = json.loads(Path(event_path).read_text())
    except Exception:
        return ""
    body = event.get("pull_request", {}).get("body")
    return body or ""


def commit_marker(base_ref: str | None, head_ref: str | None) -> str:
    if base_ref is None:
        return ""
    if head_ref is None:
        head_ref = "HEAD"
    try:
        return run(["git", "log", "--format=%B", f"{base_ref}...{head_ref}"])
    except Exception:
        return ""


def evaluate(files: list[str], marker_text: str = "") -> tuple[bool, list[str], bool, bool]:
    visible = [p for p in files if is_user_visible(p)]
    docs_touched = any(p in DOC_PATHS for p in files)
    marker = has_no_doc_marker(marker_text)
    ok = not visible or docs_touched or marker
    return ok, visible, docs_touched, marker


def self_test() -> None:
    ok, visible, docs, marker = evaluate(["weave/src/main.rs", "CHANGELOG.md"])
    assert ok and visible and docs and not marker
    ok, visible, docs, marker = evaluate(["weave-mcp/src/mcp.rs"], "body [no backlog/doc change]")
    assert ok and visible and not docs and marker
    ok, visible, docs, marker = evaluate(["weave-core/src/store.rs"])
    assert not ok and visible and not docs and not marker
    ok, visible, docs, marker = evaluate(["scripts/docs_freshness_check.py"])
    assert not ok and visible and not docs and not marker
    print("docs_freshness_check self-test: ok")


def main() -> int:
    parser = argparse.ArgumentParser(description="Require changelog/backlog freshness for user-visible changes")
    parser.add_argument("--base", help="base ref, default origin/$GITHUB_BASE_REF when available")
    parser.add_argument("--head", help="head ref, default HEAD")
    parser.add_argument("--marker", default="", help="extra marker text to consider")
    parser.add_argument("--self-test", action="store_true", help="run script unit checks")
    ns = parser.parse_args()
    if ns.self_test:
        self_test()
        return 0

    base_ref = resolve_base_ref(ns.base)
    files = changed_files(base_ref, ns.head)
    marker_text = "\n".join([ns.marker, pr_body_marker(), commit_marker(base_ref, ns.head)])
    ok, visible, docs_touched, marker = evaluate(files, marker_text)
    print(f"docs freshness: changed={len(files)} user_visible={len(visible)} docs_touched={docs_touched} no_doc_marker={marker}")
    if visible:
        print("user-visible files:")
        for path in visible:
            print(f"  {path}")
    if ok:
        return 0
    print(
        "::error::user-visible CLI/MCP/operator behavior changed without updating CHANGELOG.md or .handoff/loop/backlog.md. "
        "Either update one of those files or add [no backlog/doc change] to the PR body.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
