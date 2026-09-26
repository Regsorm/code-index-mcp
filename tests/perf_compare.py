"""Reproducible performance comparison: working tree vs a baseline ref from the repo.

Builds both binaries (current tree and a baseline git ref, default ``v1.5.1``),
runs a full fresh index on the same repository for each of them several times,
parses the stage breakdown from the logs and prints a side-by-side table.

With ``--daemon`` it additionally starts daemon+serve for both versions and
measures two readiness milestones via MCP:

* time until non-search tools answer (``get_stats`` stops reporting "indexing");
* time until ``search_function`` returns a result list (full-text search ready).

Examples:
    python tests/perf_compare.py --repo tests/cf --runs 2
    python tests/perf_compare.py --repo tests/cf --runs 1 --daemon
    python tests/perf_compare.py --baseline 0daf10f --skip-build
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parent.parent


def binary_name() -> str:
    return "bsl-indexer.exe" if os.name == "nt" else "bsl-indexer"


def run_capture(
    command: list[str], cwd: Path, env: dict[str, str], timeout: float
) -> tuple[int, str]:
    proc = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    text = proc.stdout.decode("utf-8", errors="replace")
    return proc.returncode, text


# ── log parsing ───────────────────────────────────────────────────────────────


TOTAL_RE = re.compile(r"Индексация завершена за (\d+) мс \(ядро (\d+) \+ extras (\d+)")
STAGE_RE = re.compile(r"^\s*этап\s+(\d+)\s+(.+?)\s{2,}(.+?)\s*$")
DURATION_RE = re.compile(
    r"(?:(?P<min>\d+)\s+мин\s+)?(?P<val>\d+(?:[.,]\d+)?)\s*(?P<unit>мс|с)\s*$"
)


def parse_duration(text: str) -> float | None:
    match = DURATION_RE.search(text.strip())
    if not match:
        return None
    value = float(match.group("val").replace(",", "."))
    if match.group("unit") == "мс":
        value /= 1000.0
    return value + (int(match.group("min")) * 60.0 if match.group("min") else 0.0)


def parse_log(text: str) -> dict[str, Any]:
    """Extract totals and per-stage seconds from an indexer log."""
    result: dict[str, Any] = {"stages": {}}
    total = TOTAL_RE.search(text)
    if total:
        result["total_s"] = int(total.group(1)) / 1000.0
        result["core_s"] = int(total.group(2)) / 1000.0
        result["extras_s"] = int(total.group(3)) / 1000.0
    for line in text.splitlines():
        match = STAGE_RE.match(line)
        if not match:
            continue
        name = match.group(2).strip()
        duration = parse_duration(match.group(3))
        if duration is not None:
            # Same stage name may repeat (chunks): sum it.
            result["stages"][name] = result["stages"].get(name, 0.0) + duration
    return result


# ── worktree / builds ────────────────────────────────────────────────────────


def active_toolchain() -> str | None:
    """Resolve the toolchain configured for this repo (worktrees need it explicitly)."""
    try:
        out = subprocess.run(
            ["rustup", "show", "active-toolchain"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except Exception:
        return None
    return out.split()[0] if out else None


def cargo_env() -> dict[str, str]:
    env = os.environ.copy()
    toolchain = active_toolchain()
    if toolchain:
        env["RUSTUP_TOOLCHAIN"] = toolchain
    return env


def build_baseline(ref: str, worktree: Path) -> Path:
    if worktree.exists():
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(worktree)],
            cwd=REPO_ROOT,
            capture_output=True,
            check=False,
        )
        shutil.rmtree(worktree, ignore_errors=True)
    worktree.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(worktree), ref],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
    )
    print(f"[baseline] building {ref} in {worktree} (may take several minutes)…")
    proc = subprocess.run(
        ["cargo", "build", "--release", "-p", "bsl-indexer", "--features", "enrichment"],
        cwd=worktree,
        env=cargo_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if proc.returncode != 0:
        sys.stdout.buffer.write(proc.stdout[-4000:])
        raise RuntimeError(f"baseline build failed (ref={ref})")
    binary = worktree / "target" / "release" / binary_name()
    if not binary.is_file():
        raise RuntimeError(f"baseline binary not found: {binary}")
    return binary


def build_current() -> Path:
    print("[current] building working tree…")
    proc = subprocess.run(
        ["cargo", "build", "--release", "-p", "bsl-indexer", "--features", "enrichment"],
        cwd=REPO_ROOT,
        env=cargo_env(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if proc.returncode != 0:
        sys.stdout.buffer.write(proc.stdout[-4000:])
        raise RuntimeError("current build failed")
    return REPO_ROOT / "target" / "release" / binary_name()


# ── CLI benchmark ────────────────────────────────────────────────────────────


def index_env() -> dict[str, str]:
    env = os.environ.copy()
    env["RUST_LOG"] = "warn,code_index_core=debug,bsl_extension=debug"
    return env


def run_cli(binary: Path, repo: Path, fresh: bool, timeout: float) -> dict[str, Any]:
    db = repo / ".code-index"
    if fresh:
        shutil.rmtree(db, ignore_errors=True)
    started = time.monotonic()
    code, text = run_capture(
        [str(binary), "index", str(repo)], REPO_ROOT, index_env(), timeout
    )
    wall = time.monotonic() - started
    parsed = parse_log(text)
    parsed["wall_s"] = wall
    parsed["exit_code"] = code
    return parsed


# ── daemon readiness ─────────────────────────────────────────────────────────


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def load_acceptance():
    spec = importlib.util.spec_from_file_location(
        "acc", REPO_ROOT / "tests" / "mcp_full_acceptance.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)  # type: ignore[union-attr]
    return module


def stop_process(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    # taskkill /T on Windows: a serve process with an open MCP session may
    # ignore terminate while it waits on a blocking tool call.
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
            capture_output=True,
            check=False,
        )
    else:
        proc.terminate()
    try:
        proc.wait(timeout=20)
    except subprocess.TimeoutExpired:
        proc.kill()


def daemon_readiness(
    binary: Path, repo: Path, timeout: float, acc: Any
) -> dict[str, Any]:
    """Fresh daemon: seconds until non-search tools answer and until search works.

    Milestones are probed via MCP:
    * `get_function` — gated only by the path status: as soon as the core index
      is Ready it answers (even with an empty/missing symbol), so this is the
      "tools ready" moment;
    * `search_function` — additionally gated by ``fts_build_pending``: answers
      with a result list only after the full-text index is built.
    """
    # Kill leftovers of a previous scenario: they hold the same repository.
    subprocess.run(
        ["taskkill", "/F", "/IM", binary_name()],
        capture_output=True,
        check=False,
    )
    time.sleep(2)

    home = Path(tempfile.mkdtemp(prefix="perf-daemon-"))
    shutil.rmtree(repo / ".code-index", ignore_errors=True)
    (home / "daemon.toml").write_text(
        "[daemon]\n"
        'http_host = "127.0.0.1"\n'
        "http_port = 0\n"
        'log_level = "info"\n\n'
        "[[paths]]\n"
        f"path = {json.dumps(str(repo))}\n"
        'alias = "perf"\n'
        'language = "bsl"\n',
        encoding="utf-8",
        newline="\n",
    )
    env = os.environ.copy()
    env["CODE_INDEX_HOME"] = str(home)
    env["RUST_LOG"] = "info"

    daemon = None
    serve = None
    result: dict[str, Any] = {}
    samples: list[dict[str, Any]] = []
    try:
        started = time.monotonic()
        daemon_log = (home / "daemon.log").open("wb")
        daemon = subprocess.Popen(
            [str(binary), "daemon", "run"],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=daemon_log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline and not (home / "daemon.json").is_file():
            time.sleep(0.5)

        port = free_port()
        serve_log = (home / "serve.log").open("wb")
        serve = subprocess.Popen(
            [
                str(binary),
                "serve",
                "--transport",
                "http",
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--config",
                str(home / "daemon.toml"),
            ],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=serve_log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                    break
            except OSError:
                time.sleep(0.3)
        url = f"http://127.0.0.1:{port}/mcp"
        session = acc.mcp_connect(url)

        def call(name: str, args: dict, rid: int) -> Any:
            value, _, error = acc.mcp_call(url, session, name, args, rid)
            if error:
                return None
            return acc.unwrap(value)

        def classify(value: Any) -> str:
            """Tool answer classes: gating statuses vs a real answer.

            A real answer is a list or a dict (``{"result": …}`` wrapper,
            location lists, hints); only transport errors and the structured
            unavailable/indexing statuses keep the milestone waiting.
            """
            if isinstance(value, list):
                return "ready"
            if isinstance(value, dict):
                status = value.get("status")
                if status in ("indexing", "not_started", "error", "daemon_offline"):
                    return str(status)
                error = value.get("error")
                if isinstance(error, str) and error:
                    # ``{"error": …}`` без статуса — реальный сбой вызова, а не
                    # ответ: засчитывать его за достижение рубежа нельзя.
                    return "error"
                return "ready"
            return "other"

        def probe(name: str, args: dict, deadline: float) -> None:
            rid = 100
            last_kind = ""
            while time.monotonic() < deadline:
                value = call(name, args, rid)
                rid += 1
                kind = classify(value)
                samples.append(
                    {
                        "t": round(time.monotonic() - started, 1),
                        "tool": name,
                        "kind": kind,
                        "head": str(value)[:120],
                    }
                )
                if kind != last_kind:
                    # Progress line: gating statuses change rarely, so this is
                    # one line per transition rather than per poll.
                    print(
                        f"    t={time.monotonic() - started:6.1f}s {name}: {kind}",
                        flush=True,
                    )
                    last_kind = kind
                if kind == "ready":
                    return
                time.sleep(2)

        # tools ready: gated only by path status (core index done)
        probe(
            "get_function",
            {"repo": "perf", "name": "ОбработкаПроведения"},
            time.monotonic() + timeout,
        )
        result["tools_ready_s"] = round(time.monotonic() - started, 1)
        # search ready: gated additionally by the deferred FTS build
        probe(
            "search_function",
            {"repo": "perf", "query": "ОбработкаПроведения"},
            time.monotonic() + timeout,
        )
        result["search_ready_s"] = round(time.monotonic() - started, 1)
        result["samples"] = samples
        return result
    finally:
        stop_process(serve)
        stop_process(daemon)
        shutil.rmtree(home, ignore_errors=True)


# ── reporting ────────────────────────────────────────────────────────────────


def median_of(runs: list[dict[str, Any]], key: str) -> float | None:
    values = [r[key] for r in runs if key in r]
    return statistics.median(values) if values else None


def merge_stage_medians(runs: list[dict[str, Any]]) -> dict[str, float]:
    keys = {name for run in runs for name in run.get("stages", {})}
    merged = {}
    for name in keys:
        values = [run["stages"][name] for run in runs if name in run.get("stages", {})]
        if values:
            merged[name] = statistics.median(values)
    return merged


def fmt(value: float | None, unit: str = "s") -> str:
    return "—" if value is None else f"{value:.1f}{unit}"


def print_table(title: str, rows: list[tuple[str, str, str, str]]) -> None:
    width = max((len(r[0]) for r in rows), default=10)
    print(f"\n== {title}")
    header = f"{'metric':<{width}}  {'baseline':>12}  {'current':>12}  {'delta':>10}"
    print(header)
    print("-" * len(header))
    for name, base, cur, delta in rows:
        print(f"{name:<{width}}  {base:>12}  {cur:>12}  {delta:>10}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="tests/cf", help="repository to index")
    parser.add_argument("--baseline", default="v1.5.1", help="baseline git ref")
    parser.add_argument("--runs", type=int, default=2, help="fresh runs per version")
    parser.add_argument("--daemon", action="store_true", help="also compare daemon readiness")
    parser.add_argument(
        "--daemon-only",
        action="store_true",
        help="skip CLI runs, compare daemon readiness only",
    )
    parser.add_argument("--skip-build", action="store_true", help="reuse existing binaries")
    parser.add_argument("--keep-worktree", action="store_true")
    parser.add_argument("--json", default="", help="write raw report to this JSON file")
    parser.add_argument("--timeout", type=float, default=3600.0)
    args = parser.parse_args()

    repo = (REPO_ROOT / args.repo).resolve()
    if not repo.is_dir():
        print(f"repo not found: {repo}", file=sys.stderr)
        return 2

    worktree = REPO_ROOT / "target" / "perf" / f"baseline-{args.baseline}".replace(
        ":", "_"
    )
    baseline_bin = (
        worktree / "target" / "release" / binary_name()
        if args.skip_build
        else build_baseline(args.baseline, worktree)
    )
    if args.skip_build and not baseline_bin.is_file():
        print(f"baseline binary missing: {baseline_bin}", file=sys.stderr)
        return 2
    current_bin = (
        REPO_ROOT / "target" / "release" / binary_name() if args.skip_build else build_current()
    )

    report: dict[str, Any] = {
        "repo": str(repo),
        "baseline_ref": args.baseline,
        "runs": args.runs,
    }

    if args.daemon_only:
        args.daemon = True
    else:
        fresh: dict[str, list[dict[str, Any]]] = {"baseline": [], "current": []}
        repeat: dict[str, list[dict[str, Any]]] = {"baseline": [], "current": []}
        for version, binary in (("baseline", baseline_bin), ("current", current_bin)):
            for i in range(args.runs):
                print(f"[{version}] fresh run {i + 1}/{args.runs}…")
                parsed = run_cli(binary, repo, fresh=True, timeout=args.timeout)
                parsed["label"] = f"fresh{i + 1}"
                fresh[version].append(parsed)
                print(
                    f"  total={parsed.get('total_s', float('nan')):.1f}s "
                    f"core={parsed.get('core_s', float('nan')):.1f}s "
                    f"extras={parsed.get('extras_s', float('nan')):.1f}s"
                )
                print(f"[{version}] repeat run {i + 1}/{args.runs} (no changes)…")
                parsed_repeat = run_cli(binary, repo, fresh=False, timeout=args.timeout)
                parsed_repeat["label"] = f"repeat{i + 1}"
                repeat[version].append(parsed_repeat)
                print(f"  total={parsed_repeat.get('total_s', float('nan')):.1f}s")

        rows: list[tuple[str, str, str, str]] = []

        def pair(name: str, key: str) -> None:
            base = median_of(fresh["baseline"], key)
            cur = median_of(fresh["current"], key)
            delta = (
                f"{(cur - base) / base * 100:+.1f}%"
                if base not in (None, 0) and cur is not None
                else "—"
            )
            rows.append((name, fmt(base), fmt(cur), delta))

        pair("fresh total", "total_s")
        pair("fresh core", "core_s")
        pair("fresh extras", "extras_s")
        base_rep = median_of(repeat["baseline"], "total_s")
        cur_rep = median_of(repeat["current"], "total_s")
        rows.append(
            (
                "repeat (no changes)",
                fmt(base_rep),
                fmt(cur_rep),
                f"{(cur_rep - base_rep) / base_rep * 100:+.1f}%"
                if base_rep and cur_rep
                else "—",
            )
        )

        stage_base = merge_stage_medians(fresh["baseline"])
        stage_cur = merge_stage_medians(fresh["current"])
        for name in stage_base:
            if name not in stage_cur:
                continue
            base, cur = stage_base[name], stage_cur[name]
            rows.append(
                (f"  {name}", fmt(base), fmt(cur), f"{(cur - base) / base * 100:+.1f}%")
            )

        print_table(f"CLI index: {repo.name} vs {args.baseline}", rows)
        report["fresh"] = fresh
        report["repeat"] = repeat

    if args.daemon:
        acc = load_acceptance()
        daemon_rows: list[tuple[str, str, str, str]] = []
        for version, binary in (("baseline", baseline_bin), ("current", current_bin)):
            print(f"[{version}] daemon readiness…")
            result = daemon_readiness(binary, repo, args.timeout, acc)
            report.setdefault("daemon", {})[version] = result
            print(
                f"  tools_ready={result.get('tools_ready_s')}s "
                f"search_ready={result.get('search_ready_s')}s",
                flush=True,
            )
        base = report["daemon"].get("baseline", {})
        cur = report["daemon"].get("current", {})
        for key, label in (
            ("tools_ready_s", "tools ready (non-search MCP)"),
            ("search_ready_s", "search_function ready"),
        ):
            b, c = base.get(key), cur.get(key)
            delta = f"{(c - b) / b * 100:+.1f}%" if b and c else "—"
            daemon_rows.append((label, fmt(b), fmt(c), delta))
        print_table(f"Daemon readiness vs {args.baseline}", daemon_rows)

    if args.json:
        Path(args.json).write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        print(f"\nreport: {args.json}")

    if not args.keep_worktree:
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(worktree)],
            cwd=REPO_ROOT,
            capture_output=True,
            check=False,
        )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
