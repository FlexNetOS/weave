#!/usr/bin/env python3
"""Build and smoke-test Weave's generated target artifacts.

The script is intentionally stdlib-only. It proves that Cargo creates the target
cache/artifacts, then executes the compiled binaries directly (not through
`cargo test`) and writes a machine-readable JSON report.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "target"
REPORT_DIR = TARGET / "target-smoke"
DEFAULT_REPORT = REPORT_DIR / "target-smoke.json"
TIMEOUT = 30
ALLOWED_RUSTUP_TOOLCHAIN_CHANNELS = ("stable", "nightly")


@dataclass
class Step:
    name: str
    status: str
    command: list[str] | None = None
    cwd: str | None = None
    exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    duration_ms: int = 0
    details: dict[str, Any] | None = None


@dataclass
class Artifact:
    name: str
    kind: str
    command: list[str]
    expected_binary: str
    target_dir: str
    required_features: str
    build: Step | None = None
    metadata: dict[str, Any] | None = None
    smoke_steps: list[Step] | None = None


def now_ms() -> int:
    return int(time.time() * 1000)


def run(argv: list[str], *, cwd: Path = ROOT, env: dict[str, str] | None = None, timeout: int = TIMEOUT) -> Step:
    start = now_ms()
    merged = os.environ.copy()
    if env:
        merged.update(env)
    try:
        proc = subprocess.run(
            argv,
            cwd=str(cwd),
            env=merged,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
        return Step(
            name=" ".join(argv),
            status="pass" if proc.returncode == 0 else "fail",
            command=argv,
            cwd=str(cwd),
            exit_code=proc.returncode,
            stdout=proc.stdout[-8000:],
            stderr=proc.stderr[-8000:],
            duration_ms=now_ms() - start,
        )
    except subprocess.TimeoutExpired as exc:
        return Step(
            name=" ".join(argv),
            status="fail",
            command=argv,
            cwd=str(cwd),
            exit_code=None,
            stdout=(exc.stdout or "")[-8000:] if isinstance(exc.stdout, str) else "",
            stderr=((exc.stderr or "")[-8000:] if isinstance(exc.stderr, str) else "") + "\nTIMEOUT",
            duration_ms=now_ms() - start,
        )


def parse_rustup_toolchain_names(stdout: str) -> list[str]:
    """Return rustup toolchain names from `rustup toolchain list` output."""
    names: list[str] = []
    for raw in stdout.splitlines():
        line = raw.strip()
        if not line:
            continue
        name = line.split()[0]
        if name:
            names.append(name)
    return names


def stale_rustup_toolchains(names: list[str]) -> list[str]:
    """Toolchains that are not the current channel aliases we expect operators to keep.

    The target-smoke contract is intentionally about generated artifacts from the
    latest channel aliases. Date-pinned nightlies and version-pinned stable
    duplicates are useful temporarily during bisects, but should not remain in the
    normal operator toolchain cache after a refresh/prune pass.
    """
    stale: list[str] = []
    for name in names:
        channel = name.split("-", 1)[0]
        if channel not in ALLOWED_RUSTUP_TOOLCHAIN_CHANNELS:
            stale.append(name)
            continue
        if channel == "nightly" and re.match(r"^nightly-\d{4}-\d{2}-\d{2}(?:-|$)", name):
            stale.append(name)
    return stale


def rustup_toolchain_hygiene(*, enforce: bool) -> Step:
    if not shutil.which("rustup"):
        status = "skip" if enforce else "warn"
        return Step(name="rustup toolchain hygiene", status=status, details={"reason": "rustup not on PATH"})
    listed = run(["rustup", "toolchain", "list"], timeout=30)
    names = parse_rustup_toolchain_names(listed.stdout)
    stale = stale_rustup_toolchains(names)
    status = "pass"
    if listed.status != "pass":
        status = "fail" if enforce else "warn"
    elif stale:
        status = "fail" if enforce else "warn"
    return Step(
        name="rustup toolchain hygiene",
        status=status,
        command=listed.command,
        cwd=listed.cwd,
        exit_code=listed.exit_code,
        stdout=listed.stdout,
        stderr=listed.stderr,
        duration_ms=listed.duration_ms,
        details={
            "enforced": enforce,
            "installed": names,
            "allowed_channels": list(ALLOWED_RUSTUP_TOOLCHAIN_CHANNELS),
            "stale": stale,
        },
    )


def self_test() -> int:
    sample = """\
stable-x86_64-unknown-linux-gnu (active, default)
nightly-x86_64-unknown-linux-gnu
nightly-2026-04-29-x86_64-unknown-linux-gnu
1.96.0-x86_64-unknown-linux-gnu
"""
    names = parse_rustup_toolchain_names(sample)
    expected_names = [
        "stable-x86_64-unknown-linux-gnu",
        "nightly-x86_64-unknown-linux-gnu",
        "nightly-2026-04-29-x86_64-unknown-linux-gnu",
        "1.96.0-x86_64-unknown-linux-gnu",
    ]
    if names != expected_names:
        print(f"parse_rustup_toolchain_names failed: {names!r}", file=sys.stderr)
        return 1
    stale = stale_rustup_toolchains(names)
    expected_stale = ["nightly-2026-04-29-x86_64-unknown-linux-gnu", "1.96.0-x86_64-unknown-linux-gnu"]
    if stale != expected_stale:
        print(f"stale_rustup_toolchains failed: {stale!r}", file=sys.stderr)
        return 1
    clean = ["stable-x86_64-unknown-linux-gnu", "nightly-x86_64-unknown-linux-gnu"]
    if stale_rustup_toolchains(clean):
        print("stale_rustup_toolchains rejected clean stable/nightly aliases", file=sys.stderr)
        return 1
    print("target_smoke self-test: pass")
    return 0


def require(condition: bool, name: str, details: dict[str, Any] | None = None) -> Step:
    return Step(name=name, status="pass" if condition else "fail", details=details or {})


def cargo_metadata(target_dir: Path | None = None) -> dict[str, Any]:
    env = {}
    if target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(target_dir)
    step = run(["cargo", "metadata", "--no-deps", "--format-version", "1"], env=env, timeout=60)
    if step.status != "pass":
        return {"error": asdict(step)}
    try:
        return json.loads(step.stdout)
    except json.JSONDecodeError as err:
        return {"error": f"metadata JSON parse failed: {err}", "raw": step.stdout[-1000:]}


def file_metadata(path: Path) -> dict[str, Any]:
    meta: dict[str, Any] = {
        "path": str(path),
        "exists": path.exists(),
    }
    if path.exists():
        st = path.stat()
        meta.update({"size_bytes": st.st_size, "mode_octal": oct(st.st_mode & 0o777), "mtime": int(st.st_mtime)})
        file_step = run(["file", str(path)], timeout=10) if shutil.which("file") else None
        if file_step:
            meta["file"] = file_step.stdout.strip()
    return meta


def target_markers(target_dir: Path) -> dict[str, Any]:
    cachedir = target_dir / "CACHEDIR.TAG"
    rustc_info = target_dir / ".rustc_info.json"
    markers: dict[str, Any] = {
        "target_dir": str(target_dir),
        "exists": target_dir.exists(),
        "cachedir_tag_exists": cachedir.exists(),
        "rustc_info_exists": rustc_info.exists(),
        "debug_dir_exists": (target_dir / "debug").exists(),
        "release_dir_exists": (target_dir / "release").exists(),
    }
    if cachedir.exists():
        text = cachedir.read_text(errors="replace")
        markers["cachedir_signature_ok"] = text.startswith("Signature: 8a477f597d28d172789f06886806bc55")
        markers["cachedir_mentions_cargo"] = "created by cargo" in text.lower()
    if rustc_info.exists():
        try:
            markers["rustc_info"] = json.loads(rustc_info.read_text(errors="replace"))
        except json.JSONDecodeError:
            markers["rustc_info_parse_error"] = True
    return markers


def isolated_env(db: Path, home: Path, session: str = "smoke") -> dict[str, str]:
    env = {
        "WEAVE_DB": str(db),
        "WEAVE_SESSION": session,
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
        "WEAVE_MUX": "none",
    }
    # Scrub common live mux variables so artifact smokes never type into a pane.
    for key in ["TMUX", "TMUX_PANE", "ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID", "WEZTERM_PANE", "KITTY_WINDOW_ID", "KITTY_LISTEN_ON", "STY"]:
        env[key] = ""
    return env


def parse_first_int(pattern: str, text: str) -> int | None:
    m = re.search(pattern, text)
    return int(m.group(1)) if m else None


def parse_ask_id(text: str) -> str | None:
    m = re.search(r"opened ask\s+(ask_[A-Za-z0-9_]+)", text)
    return m.group(1) if m else None


def assert_json(step: Step, name: str) -> Step:
    if step.status != "pass":
        return Step(name=name, status="fail", details={"reason": "command failed", "command": step.command, "exit_code": step.exit_code})
    try:
        json.loads(step.stdout)
        return Step(name=name, status="pass")
    except json.JSONDecodeError as err:
        return Step(name=name, status="fail", details={"error": str(err), "stdout": step.stdout[-500:]})


def mcp_smoke(binary: Path, env: dict[str, str]) -> list[Step]:
    steps: list[Step] = []
    start = now_ms()
    proc = subprocess.Popen(
        [str(binary), "mcp", "--session", "mcp-smoke"],
        cwd=str(ROOT),
        env={**os.environ.copy(), **env},
        text=True,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    def send(obj: dict[str, Any]) -> None:
        assert proc.stdin is not None
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()

    def recv(timeout_s: float = 5.0) -> dict[str, Any] | None:
        assert proc.stdout is not None
        # TextIO has no portable nonblocking timeout; use a short helper thread.
        import queue
        import threading

        q: queue.Queue[str | None] = queue.Queue(maxsize=1)

        def reader() -> None:
            q.put(proc.stdout.readline())

        t = threading.Thread(target=reader, daemon=True)
        t.start()
        try:
            line = q.get(timeout=timeout_s)
        except queue.Empty:
            return None
        if not line:
            return None
        return json.loads(line)

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        init = recv()
        steps.append(require(bool(init and init.get("result", {}).get("serverInfo", {}).get("name") == "weave"), "mcp initialize", {"response": init}))
        send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        listed = recv()
        tools = [t.get("name") for t in listed.get("result", {}).get("tools", [])] if listed else []
        steps.append(require("weave" in tools, "mcp tools/list includes token-light meta tool", {"tool_count": len(tools), "tools": tools[:20]}))
        send({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "weave", "arguments": {"mode": "list"}}})
        listed_ops = recv()
        steps.append(require(bool(listed_ops and "result" in listed_ops and "weave_send" in json.dumps(listed_ops)), "mcp meta-tool mode=list", {"response": listed_ops}))
        send({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "weave", "arguments": {"mode": "describe", "name": "send"}}})
        described = recv()
        steps.append(require(bool(described and "result" in described and "weave_send" in json.dumps(described)), "mcp meta-tool mode=describe send", {"response": described}))
        send({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "weave", "arguments": {"mode": "call", "name": "inbox", "arguments": {"me": "mcp-smoke", "peek": True}}}})
        called = recv()
        steps.append(require(bool(called and "result" in called), "mcp meta-tool mode=call inbox", {"response": called}))
    except Exception as err:  # noqa: BLE001 - smoke report should record, not crash.
        steps.append(Step(name="mcp smoke exception", status="fail", stderr=str(err), duration_ms=now_ms() - start))
    finally:
        if proc.stdin:
            proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        err = proc.stderr.read() if proc.stderr else ""
        if err:
            steps.append(Step(name="mcp stderr", status="warn", stderr=err[-2000:]))
    return steps


def command_available(binary: Path, command: str, env: dict[str, str]) -> bool:
    help_step = run([str(binary), "--help"], env=env, timeout=10)
    return help_step.status == "pass" and re.search(rf"^  {re.escape(command)}\s", help_step.stdout, re.MULTILINE) is not None


def smoke_artifact(binary: Path, artifact_name: str) -> list[Step]:
    steps: list[Step] = []
    with tempfile.TemporaryDirectory(prefix=f"weave-{artifact_name}-") as td:
        base = Path(td)
        home = base / "home"
        home.mkdir(parents=True)
        db = base / "messages.db"
        env = isolated_env(db, home)

        version = run([str(binary), "--version"], env=env)
        steps.append(version)
        steps.append(require("backends:" in version.stdout, "version reports backend provenance", {"stdout": version.stdout.strip()}))

        doctor = run([str(binary), "doctor", "--json"], env=env)
        steps.append(doctor)
        steps.append(assert_json(doctor, "doctor --json parses"))

        reg_alice = run([str(binary), "register", "--name", "alice", "--cwd", str(base / "repo-a")], env=env)
        reg_bob = run([str(binary), "register", "--name", "bob", "--cwd", str(base / "repo-b")], env=env)
        steps.extend([reg_alice, reg_bob])

        send = run([str(binary), "send", "--from", "alice", "--to", "bob", "--body", "target smoke hello"], env=env)
        steps.append(send)
        msg_id = parse_first_int(r"sent #(\d+):", send.stdout)
        steps.append(require(msg_id is not None, "send returns message id", {"stdout": send.stdout.strip()}))

        inbox = run([str(binary), "inbox", "--me", "bob", "--all", "--json"], env=env)
        steps.append(inbox)
        steps.append(assert_json(inbox, "inbox --json parses"))
        try:
            messages = json.loads(inbox.stdout).get("messages", [])
            has_body = any(m.get("body") == "target smoke hello" for m in messages)
        except Exception:
            has_body = False
        steps.append(require(has_body, "inbox contains sent body"))

        if msg_id is not None:
            delivery = run([str(binary), "delivery", "--id", str(msg_id), "--json"], env=env)
            steps.append(delivery)
            steps.append(assert_json(delivery, "delivery --json parses"))

        ask = run([str(binary), "ask", "--from", "alice", "--to", "bob", "--body", "target smoke question?"], env=env)
        steps.append(ask)
        ask_id = parse_ask_id(ask.stdout)
        steps.append(require(ask_id is not None, "ask returns correlation id", {"stdout": ask.stdout.strip()}))
        if ask_id:
            status1 = run([str(binary), "ask-status", "--id", ask_id, "--json"], env=env)
            responder = run([str(binary), "responder", "--me", "bob", "--status", "received", "--json"], env=env)
            status2 = run([str(binary), "ask-status", "--id", ask_id, "--json"], env=env)
            answer = run([str(binary), "answer", "--from", "bob", "--id", ask_id, "--body", "target smoke answer"], env=env)
            ack = run([str(binary), "ack", "--from", "bob", "--id", ask_id, "--message", "target smoke ack"], env=env)
            status3 = run([str(binary), "ask-status", "--id", ask_id, "--json"], env=env)
            steps.extend([status1, responder, status2, answer, ack, status3])
            for i, st in enumerate([status1, responder, status2, status3], start=1):
                steps.append(assert_json(st, f"ask/responder status JSON {i}"))

        job = run([str(binary), "job", "delegate", "--from", "alice", "--to", "bob", "--title", "target smoke job", "--json"], env=env)
        jobs = run([str(binary), "job", "list", "--json"], env=env)
        steps.extend([job, jobs, assert_json(job, "job delegate --json parses"), assert_json(jobs, "job list --json parses")])

        sessions = run([str(binary), "sessions", "--json"], env=env)
        graph = run([str(binary), "graph", "--json"], env=env)
        steps.extend([sessions, assert_json(sessions, "sessions --json parses"), graph, assert_json(graph, "graph --json parses")])

        session_file = base / "session.json"
        sess_export = run([str(binary), "session", "export", "--for", "bob", "--out", str(session_file), "--force"], env=env)
        sess_import_dry = run([str(binary), "session", "import", "--in", str(session_file), "--as", "bob-resume", "--dry-run"], env=env) if session_file.exists() else Step(name="session import skipped", status="skip", details={"reason": "export missing"})
        steps.extend([sess_export, sess_import_dry])

        backup_file = base / "backup.tar"
        backup = run([str(binary), "backup", "--out", str(backup_file), "--force"], env=env)
        steps.append(backup)
        steps.append(require(backup_file.exists(), "backup artifact exists", {"path": str(backup_file)}))
        restore_env = isolated_env(base / "restored.db", base / "restore-home", session="restore-smoke")
        restore = run([str(binary), "restore", "--in", str(backup_file), "--force"], env=restore_env) if backup_file.exists() else Step(name="restore skipped", status="skip", details={"reason": "backup missing"})
        steps.append(restore)

        readonly_db = base / "readonly.db"
        if db.exists():
            shutil.copy2(db, readonly_db)
            readonly_db.chmod(0o400)
            ro_env = isolated_env(readonly_db, home / "ro-home")
            ro = run([str(binary), "send", "--from", "alice", "--to", "bob", "--body", "must fail readonly"], env=ro_env)
            ro.status = "pass" if ro.exit_code != 0 else "fail"
            ro.name = "readonly DB rejects write"
            steps.append(ro)

        if command_available(binary, "provider-switch", env):
            missing_cc = run([str(binary), "provider-switch", "list", "--db", str(base / "missing-cc-switch.db")], env=env)
            missing_cc.status = "pass" if missing_cc.exit_code != 0 and "unable to open" not in missing_cc.stderr.lower() else "warn"
            missing_cc.name = "absent CC Switch DB is diagnostic not panic"
            steps.append(missing_cc)
        else:
            steps.append(Step(name="absent CC Switch DB diagnostic", status="skip", details={"reason": "provider-switch command is not compiled in this artifact"}))

        unknown = run([str(binary), "connect", "--to", "no-such-peer"], env=env)
        unknown.status = "pass" if unknown.exit_code != 0 else "fail"
        unknown.name = "unknown peer connect fails closed"
        steps.append(unknown)

        steps.extend(mcp_smoke(binary, env))

    return steps


def artifact_matrix(full: bool) -> list[Artifact]:
    items = [
        Artifact("debug-default", "debug", ["cargo", "build", "-p", "weave"], str(TARGET / "debug" / "weave"), str(TARGET), "sqlite"),
        Artifact("release-default", "release", ["cargo", "build", "-p", "weave", "--release"], str(TARGET / "release" / "weave"), str(TARGET), "sqlite"),
    ]
    if full:
        combos = [
            ("debug-sign", ["--features", "sign"], "sqlite,sign"),
            ("debug-surfaces", ["--features", "surfaces"], "sqlite,surfaces"),
            ("debug-obscura", ["--features", "obscura"], "sqlite,obscura"),
            ("debug-libsql", ["--no-default-features", "--features", "libsql"], "libsql"),
            ("debug-libsql-sign", ["--no-default-features", "--features", "libsql sign"], "libsql,sign"),
            ("debug-libsql-surfaces", ["--no-default-features", "--features", "libsql surfaces"], "libsql,surfaces"),
        ]
        for name, flags, features in combos:
            tdir = REPORT_DIR / "build" / name
            items.append(Artifact(name, "feature-debug", ["cargo", "build", "-p", "weave", "--target-dir", str(tdir), *flags], str(tdir / "debug" / "weave"), str(tdir), features))
    return items


def main() -> int:
    parser = argparse.ArgumentParser(description="Build and smoke Weave target artifacts")
    parser.add_argument("--full", action="store_true", help="build and smoke feature-gated artifact matrix too")
    parser.add_argument("--clean-target", action="store_true", help="delete ./target first to prove Cargo recreates generated output")
    parser.add_argument("--check-rustup-hygiene", action="store_true", help="fail if rustup has stale date/version-pinned toolchains beside stable/nightly aliases")
    parser.add_argument("--self-test", action="store_true", help="run pure tests for this smoke runner and exit")
    parser.add_argument("--output", type=Path, default=DEFAULT_REPORT, help="JSON report path")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    if args.clean_target and TARGET.exists():
        cachedir = TARGET / "CACHEDIR.TAG"
        if not cachedir.exists() and any(TARGET.iterdir()):
            print("Refusing to delete target without CACHEDIR.TAG marker", file=sys.stderr)
            return 2
        shutil.rmtree(TARGET)

    report: dict[str, Any] = {
        "schema": "weave.target_smoke.v1",
        "started_at_epoch_ms": now_ms(),
        "root": str(ROOT),
        "clean_target_requested": args.clean_target,
        "full_matrix": args.full,
        "environment": {
            "cargo": asdict(run(["cargo", "-Vv"], timeout=10)),
            "rustc": asdict(run(["rustc", "-Vv"], timeout=10)),
            "env": {k: os.environ.get(k) for k in ["CARGO", "RUSTC", "RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_TARGET_DIR"]},
            "cargo_metadata": cargo_metadata(),
            "pre_build_target_markers": target_markers(TARGET),
        },
        "artifacts": [],
    }
    rustup_hygiene = rustup_toolchain_hygiene(enforce=args.check_rustup_hygiene)
    report["environment"]["rustup_toolchain_hygiene"] = asdict(rustup_hygiene)

    artifacts = artifact_matrix(args.full)
    any_fail = rustup_hygiene.status == "fail"
    for artifact in artifacts:
        build = run(artifact.command, timeout=1800)
        artifact.build = build
        binary = Path(artifact.expected_binary)
        artifact.metadata = {
            "binary": file_metadata(binary),
            "target_markers": target_markers(Path(artifact.target_dir)),
            "cargo_metadata": cargo_metadata(Path(artifact.target_dir) if Path(artifact.target_dir) != TARGET else None),
        }
        if build.status != "pass" or not binary.exists():
            artifact.smoke_steps = [require(False, "binary exists after build", {"path": str(binary)})]
        else:
            artifact.smoke_steps = smoke_artifact(binary, artifact.name)
        for step in [artifact.build, *(artifact.smoke_steps or [])]:
            if step and step.status == "fail":
                any_fail = True
        report["artifacts"].append(asdict(artifact))

    if not args.full:
        report["skipped_feature_matrix"] = [a.name for a in artifact_matrix(True)[2:]]
        report["note"] = "Run with --full to build/sign/surfaces/obscura/libsql feature artifacts in isolated target dirs."

    report["finished_at_epoch_ms"] = now_ms()
    report["status"] = "fail" if any_fail else "pass"

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")

    print(f"target smoke status: {report['status']}")
    print(f"report: {args.output}")
    for art in report["artifacts"]:
        fails = [s for s in (art.get("smoke_steps") or []) if s.get("status") == "fail"]
        print(f"- {art['name']}: build={art['build']['status'] if art.get('build') else 'missing'} smoke_failures={len(fails)} binary={art['expected_binary']}")
    return 1 if any_fail else 0


if __name__ == "__main__":
    raise SystemExit(main())
