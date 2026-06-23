#!/usr/bin/env python3
"""Local supply-chain advisory gate helper for Weave (WL-075).

This reproduces the CI cargo-deny advisory posture without hiding the important
boundary: default sqlite builds must stay advisory-clean, while the known
upstream-pinned libsql remote-TLS advisories must remain explicitly listed in
deny.toml until libsql moves to a patched rustls stack.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

EXPECTED_IGNORES = {
    "RUSTSEC-2026-0098",
    "RUSTSEC-2026-0099",
    "RUSTSEC-2026-0049",
    "RUSTSEC-2026-0104",
    "RUSTSEC-2025-0134",
}
REMOVED_IGNORES = {"RUSTSEC-2025-0141"}  # bincode, eliminated by trimming libsql features.


@dataclass
class Check:
    name: str
    status: str
    detail: str = ""

    def as_dict(self) -> dict[str, str]:
        return {"name": self.name, "status": self.status, "detail": self.detail}


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def parse_ignored_advisories(text: str) -> set[str]:
    return set(re.findall(r'id\s*=\s*"(RUSTSEC-\d{4}-\d{4})"', text))


def command_text(argv: Iterable[str]) -> str:
    return " ".join(argv)


def run(argv: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def validate_deny_toml(root: Path) -> Check:
    path = root / "deny.toml"
    text = path.read_text(encoding="utf-8")
    found = parse_ignored_advisories(text)
    missing = sorted(EXPECTED_IGNORES - found)
    extra = sorted(found - EXPECTED_IGNORES)
    removed = sorted(REMOVED_IGNORES & found)
    problems: list[str] = []
    if missing:
        problems.append(f"missing expected ignores: {', '.join(missing)}")
    if extra:
        problems.append(f"unexpected ignore ids: {', '.join(extra)}")
    if removed:
        problems.append(f"removed advisories reintroduced: {', '.join(removed)}")
    if "all-features = true" not in text:
        problems.append("deny.toml must keep [graph] all-features = true")
    if problems:
        return Check("deny.toml advisory policy", "fail", "; ".join(problems))
    return Check("deny.toml advisory policy", "pass", f"{len(found)} explicit ignores")


def check_default_tree_clean(root: Path) -> Check:
    argv = ["cargo", "tree", "-i", "rustls-webpki", "--locked"]
    proc = run(argv, root)
    combined = proc.stdout + proc.stderr
    if proc.returncode != 0 and "did not match any packages" in combined:
        return Check("default rustls-webpki tree", "pass", "default sqlite graph has no rustls-webpki")
    return Check(
        "default rustls-webpki tree",
        "fail",
        f"expected no rustls-webpki in default graph; `{command_text(argv)}` returned {proc.returncode}: {combined.strip()[:500]}",
    )


def check_libsql_tree_tracks_tls(root: Path) -> Check:
    argv = [
        "cargo",
        "tree",
        "-i",
        "rustls-webpki",
        "--locked",
        "--no-default-features",
        "--features",
        "libsql",
    ]
    proc = run(argv, root)
    out = proc.stdout + proc.stderr
    if proc.returncode == 0 and "rustls-webpki v0.102" in out and "libsql v" in out:
        return Check("libsql TLS advisory tree", "pass", "rustls-webpki remains confined to libsql TLS graph")
    if proc.returncode != 0 and "did not match any packages" in out:
        return Check(
            "libsql TLS advisory tree",
            "fail",
            "rustls-webpki disappeared from libsql graph; verify libsql patched TLS and remove stale deny.toml ignores",
        )
    return Check(
        "libsql TLS advisory tree",
        "fail",
        f"unexpected libsql TLS tree result from `{command_text(argv)}`: {out.strip()[:500]}",
    )


def run_cargo_deny(root: Path, allow_missing: bool) -> Check:
    cargo_deny = shutil.which("cargo-deny")
    if cargo_deny is None:
        detail = (
            "cargo-deny is not installed. Install it with `cargo install cargo-deny --locked` "
            "or rely on GitHub's EmbarkStudios/cargo-deny-action; then run "
            "`cargo deny check advisories`."
        )
        return Check("cargo-deny check advisories", "warn" if allow_missing else "fail", detail)
    proc = run([cargo_deny, "check", "advisories"], root)
    if proc.returncode == 0:
        return Check("cargo-deny check advisories", "pass", "local cargo-deny matches CI command")
    return Check(
        "cargo-deny check advisories",
        "fail",
        (proc.stdout + proc.stderr).strip()[-1200:],
    )


def run_checks(allow_missing_cargo_deny: bool) -> list[Check]:
    root = repo_root()
    return [
        validate_deny_toml(root),
        check_default_tree_clean(root),
        check_libsql_tree_tracks_tls(root),
        run_cargo_deny(root, allow_missing_cargo_deny),
    ]


def self_test() -> None:
    sample = '\n'.join(f'{{ id = "{rid}", reason = "x" }},' for rid in sorted(EXPECTED_IGNORES))
    parsed = parse_ignored_advisories(sample)
    assert parsed == EXPECTED_IGNORES, parsed
    assert parse_ignored_advisories('{ id = "RUSTSEC-2025-0141", reason = "bad" }') == {"RUSTSEC-2025-0141"}
    assert command_text(["cargo", "deny", "check", "advisories"]) == "cargo deny check advisories"
    fake_check = Check("x", "pass", "y").as_dict()
    assert fake_check == {"name": "x", "status": "pass", "detail": "y"}
    print("supply_chain_audit self-test: pass")


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--self-test", action="store_true", help="run stdlib-only unit checks")
    p.add_argument(
        "--allow-missing-cargo-deny",
        action="store_true",
        help="treat a missing cargo-deny binary as a warning so docs/CI-free environments can still audit the rest",
    )
    p.add_argument("--json", action="store_true", help="emit machine-readable check results")
    args = p.parse_args(argv)
    if args.self_test:
        self_test()
        return 0
    checks = run_checks(args.allow_missing_cargo_deny)
    if args.json:
        print(json.dumps({"checks": [c.as_dict() for c in checks]}, indent=2))
    else:
        print("Weave supply-chain advisory audit (WL-075)")
        for c in checks:
            suffix = f" — {c.detail}" if c.detail else ""
            print(f"[{c.status}] {c.name}{suffix}")
    if any(c.status == "fail" for c in checks):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
