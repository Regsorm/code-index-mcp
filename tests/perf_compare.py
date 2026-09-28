"""Reproducible performance comparison: working tree vs a baseline ref from the repo.

Builds both binaries (current tree and a baseline git ref, default ``v1.5.1``),
runs a full fresh index on the same repository for each of them several times,
parses the stage breakdown from the logs and prints a side-by-side table.

With ``--daemon`` it additionally starts daemon+serve for both versions and
measures readiness milestones and the first-edit latency via MCP:

* time until non-search tools answer (``get_function`` stops reporting "indexing");
* time until ``search_function`` returns a result list (full-text search ready);
* time from editing one ``.bsl`` file to the call graph serving the new edge
  (path status is ready again and ``find_path_bsl`` sees the new pair).

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
import secrets
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


def index_in_use(db_dir: Path) -> bool:
    """Держит ли базу индекса другой процесс (рабочий демон, выдача, прошлый замер)."""
    if os.name == "nt":
        # Windows не переименовывает каталог, в котором открыт файл.
        probe = db_dir.with_name(db_dir.name + ".perf-probe")
        try:
            db_dir.rename(probe)
        except OSError:
            return True
        probe.rename(db_dir)
        return False
    # Linux: ищем открытые дескрипторы на файлы каталога. Процессы чужих
    # пользователей (демон в контейнере) не видны — отсюда совет про копию.
    target = str(db_dir.resolve()) + os.sep
    for fd_dir in Path("/proc").glob("[0-9]*/fd"):
        try:
            for fd in fd_dir.iterdir():
                if os.readlink(fd).startswith(target):
                    return True
        except OSError:
            continue
    return False


def wipe_index(repo: Path) -> None:
    """Удалить `.code-index` для замера с нуля — только если его никто не держит."""
    db = repo / ".code-index"
    if not db.exists():
        return
    if index_in_use(db):
        raise SystemExit(
            f"refusing to wipe {db}: the index is open by another process "
            "(a running code-index daemon?). Run the benchmark on a copy of the repository."
        )
    shutil.rmtree(db)


def run_cli(binary: Path, repo: Path, fresh: bool, timeout: float) -> dict[str, Any]:
    if fresh:
        wipe_index(repo)
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


def binary_version(binary: Path) -> str | None:
    """First line of ``binary --version``; ``None`` when it cannot be read."""
    try:
        out = subprocess.run(
            [str(binary), "--version"], capture_output=True, text=True, timeout=30
        )
    except Exception:  # noqa: BLE001 - провенанс не должен ронять замер
        return None
    if out.returncode != 0:
        return None
    lines = out.stdout.strip().splitlines()
    return lines[0] if lines else None


def git_rev(ref: str) -> str | None:
    """Short commit hash of ``ref``; ``+dirty`` when the tree has changes."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short", ref],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if out.returncode != 0 or not out.stdout.strip():
            return None
        rev = out.stdout.strip()
    except Exception:  # noqa: BLE001 - провенанс не должен ронять замер
        return None
    # Сбой проверки дерева не должен терять уже полученный хэш.
    try:
        status = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except Exception:  # noqa: BLE001
        return rev
    return rev + ("+dirty" if status.stdout.strip() else "")


def stop_process(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    # taskkill /T on Windows: a serve process with an open MCP session may
    # ignore terminate while it waits on a blocking tool call.
    if os.name == "nt":
        try:
            subprocess.run(
                ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                capture_output=True,
                check=False,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError):
            # CreateProcess может отказать (антивирус, лимиты) или taskkill
            # зависнуть — не роняем прогон: ниже процесс добивается по PID.
            pass
    else:
        proc.terminate()
    try:
        proc.wait(timeout=20)
    except subprocess.TimeoutExpired:
        try:
            proc.kill()
        except OSError:
            pass


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
    # Процессы по имени образа не гасим: под `taskkill /IM` попадают и рабочие
    # демон с выдачей. Свои процессы останавливает `stop_process` по PID, а
    # остаток прошлого прогона, держащий репозиторий, поймает `wipe_index`.
    # Режим хранилища не подменяем: `auto` остаётся как есть. К открытию
    # extras-гейта in-memory база уже сброшена и переоткрыта на диске, поэтому
    # метрика первой правки меряется в обоих режимах (см. `first_edit_scenario`).
    wipe_index(repo)
    home = Path(tempfile.mkdtemp(prefix="perf-daemon-"))
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
    # На Windows `daemon run` сам себя детлешит (`DETACHED_PROCESS`) и выходит:
    # Popen остаётся лишь launcher'ом, убивать по его PID некого, а detached-клон
    # держит `.code-index` и переживает `stop_process` — следующий прогон тогда
    # честно отказывается стирать занятый индекс. Флаг оставляет демон прямым
    # потомком, и `stop_process` снимает ровно СВОЙ процесс (замечание к PR #12:
    # чужие `bsl-indexer` по имени образа не гасим).
    env["CODE_INDEX_DAEMON_DETACHED"] = "1"

    daemon = None
    serve = None
    daemon_log = None
    serve_log = None
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

        def call(name: str, args: dict, rid: int, call_timeout: float = 30.0) -> Any:
            value, _, error = acc.mcp_call(url, session, name, args, rid, call_timeout)
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
                if status in (
                    "indexing",
                    "not_started",
                    "error",
                    "daemon_offline",
                    "unknown_repo",
                    "federation_error",
                ):
                    return str(status)
                error = value.get("error")
                if isinstance(error, str) and error:
                    # ``{"error": …}`` без статуса — реальный сбой вызова, а не
                    # ответ: засчитывать его за достижение рубежа нельзя.
                    return "error"
                return "ready"
            return "other"

        def probe(name: str, args: dict, deadline: float) -> bool:
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
                    return True
                time.sleep(2)
            print(f"    t={time.monotonic() - started:6.1f}s {name}: timeout", flush=True)
            return False

        # tools ready: gated only by path status (core index done)
        tools_ok = probe(
            "get_function",
            {"repo": "perf", "name": "ОбработкаПроведения"},
            time.monotonic() + timeout,
        )
        result["tools_ready_s"] = round(time.monotonic() - started, 1) if tools_ok else None
        # search ready: gated additionally by the deferred FTS build
        search_ok = probe(
            "search_function",
            {"repo": "perf", "query": "ОбработкаПроведения"},
            time.monotonic() + timeout,
        )
        result["search_ready_s"] = round(time.monotonic() - started, 1) if search_ok else None
        if not tools_ok or not search_ok:
            result["timed_out"] = [
                name
                for name, ok in (("get_function", tools_ok), ("search_function", search_ok))
                if not ok
            ]
        # First edit after start: the watch path must make new code visible in
        # the call graph — see `first_edit_scenario`. A harness bug here must not
        # sink the whole run, so the scenario is guarded and reported as an error.
        partial: dict[str, Any] = {}
        try:
            result.update(
                first_edit_scenario(
                    repo, call, started, timeout, samples, home / "daemon.log", partial
                )
            )
        except Exception as exc:  # noqa: BLE001 - диагностика важнее строгости
            # Частичный результат сценария не теряем: target/restored/storage_mode
            # остаются в отчёте, даже если сценарий упал на середине.
            result.update(partial)
            result.setdefault("first_edit_s", None)
            result["first_edit_error"] = str(exc)
        if result.get("first_edit_timed_out"):
            result.setdefault("timed_out", []).append("first_edit")
        if result.get("first_edit_s") is None:
            reason = (
                result.get("first_edit_skip")
                or result.get("first_edit_error")
                or "timeout"
            )
            if result.get("first_edit_skip"):
                # Ожидаемый пропуск (in-memory база, нет .bsl) — без хвоста лога.
                print(f"    first edit metric skipped: {reason}", flush=True)
            else:
                print(f"    first edit metric failed: {reason}", flush=True)
                tail = read_log_tail(home / "daemon.log", 15)
                if tail:
                    print("    daemon.log tail:", flush=True)
                    print(tail, flush=True)
        result["samples"] = samples
        return result
    finally:
        stop_process(serve)
        stop_process(daemon)
        # Хендлы журналов держат каталог на Windows: без закрытия `rmtree`
        # молча ничего не удаляет (накопилось 29 каталогов perf-daemon-*).
        for handle in (serve_log, daemon_log):
            if handle is not None:
                try:
                    handle.close()
                except OSError:
                    pass
        shutil.rmtree(home, ignore_errors=True)


def read_log_tail(path: Path, lines: int) -> str:
    """Last lines of a log file, indented for printing; empty on read failure."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ""
    return "\n".join(f"      {line}" for line in text[-lines:])


def daemon_storage_mode(log_path: Path) -> str | None:
    """Storage mode the daemon planned for the new database (``disk``/``memory``).

    Read from the log line `новая база — режим хранилища: …` (v1.8.0+). The
    benchmark leaves `auto` as is and does not skip either mode: by the time the
    extras gate opens, an in-memory database has already been flushed and
    reopened on disk, so the first edit is measured on the same disk-backed
    service. The field is informational.
    """
    try:
        text = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    match = re.search(r"новая база — режим хранилища: (на диске|в оперативной памяти)", text)
    if not match:
        return None
    return "disk" if match.group(1) == "на диске" else "memory"


def log_size(path: Path) -> int:
    """Current byte size of a log file (0 when it does not exist yet)."""
    try:
        return path.stat().st_size
    except OSError:
        return 0


def read_log_slice(path: Path, offset: int) -> str:
    """Log text written after ``offset``; empty on read failure."""
    try:
        with path.open("rb") as handle:
            handle.seek(offset)
            return handle.read().decode("utf-8", errors="replace")
    except OSError:
        return ""


def wait_for_log_marker(log_path: Path, marker: str, deadline: float) -> bool:
    """Wait until ``marker`` appears in the log; ``False`` on timeout."""
    while time.monotonic() < deadline:
        try:
            if marker in log_path.read_text(encoding="utf-8", errors="replace"):
                return True
        except OSError:
            pass
        time.sleep(0.5)
    return False


def graph_phase_seconds(text: str) -> float | None:
    """Seconds of the FIRST ``этап N  граф вызовов`` block in a log slice.

    The slice starts right before the marker write, so the first block is the
    batch that ADDED the marker; the restore batch is logged later and is not in
    the slice. The stage number is positional, so it is not pinned.
    This is the phase that regressed in 1.8.0 (full scan of ``proc_call_graph``
    on every edit); the wall-clock MCP milestone is dominated by the watcher
    scheduler, so the phase time is the precise regression signal.
    """
    stage_re = re.compile(r"этап\s+\d+\s+граф вызовов\s")
    for line in text.splitlines():
        if not stage_re.search(line):
            continue
        duration = parse_duration(line)
        if duration is not None:
            return duration
    return None


def strip_perf_edit_markers(raw: bytes) -> bytes:
    """Remove ``PerfEdit...`` procedures left in the file by an aborted run.

    The scenario repairs the target before measuring and verifies the bytes
    after restore, so a killed run cannot turn into the "original" baseline.
    """
    text = raw.decode("utf-8", errors="surrogateescape")
    # Удаляем ровно ту пару процедур, которую дописывает сценарий
    # (PerfEditCaller<8 hex> + PerfEditCallee<8 hex>), и не трогаем остальные
    # байты файла — прежняя версия нормализовала хвост переводами строк.
    marker = re.compile(
        r"\n\nПроцедура (PerfEditCaller[0-9a-f]{8})\(\) Экспорт\r?\n"
        r"    (PerfEditCallee[0-9a-f]{8})\(\);\r?\n"
        r"КонецПроцедуры\r?\n\r?\n"
        r"Процедура \2\(\) Экспорт\r?\n"
        r"КонецПроцедуры\r?\n"
    )
    return marker.sub("", text).encode("utf-8", errors="surrogateescape")


def function_present(value: Any, name: str) -> bool:
    """Is the procedure ``name`` present in a ``get_function`` answer?"""
    if isinstance(value, list):
        return any(isinstance(x, dict) and x.get("name") == name for x in value)
    if isinstance(value, dict):
        inner = value.get("result")
        if isinstance(inner, list):
            return any(isinstance(x, dict) and x.get("name") == name for x in inner)
        # Без списка в ``result`` присутствие не подтвердить: подстрочный поиск
        # по JSON ловил имя в тексте ошибки.
        return False
    return False


def repo_path_status(health: Any, alias: str) -> str | None:
    """Path status of one repo alias from a ``health`` answer."""
    if not isinstance(health, dict):
        return None
    for item in health.get("repos") or []:
        if isinstance(item, dict) and item.get("repo") == alias:
            status = (item.get("path_status") or {}).get("status")
            return str(status) if status else None
    return None


def first_edit_scenario(
    repo: Path,
    call: Any,
    started: float,
    timeout: float,
    samples: list[dict[str, Any]],
    log_path: Path,
    out: dict[str, Any],
) -> dict[str, Any]:
    """First edit after daemon start: seconds until the graph serves the new edge.

    One ``.bsl`` file gets a unique caller/callee pair appended; the milestone is
    the moment both hold: the path status is ``ready`` again (in 1.8.x this covers
    ``reindexing_extras``, i.e. the add-on rebuild) and ``find_path_bsl`` returns
    the new pair. The graph phase of the ADD batch is parsed from the log slice
    taken before the marker write. This is the scenario that regressed in 1.8.0,
    when the partial call-type index stopped serving the layer deletes and the
    first edit waited for a full scan of ``proc_call_graph`` on a cold database.
    """
    candidates: list[tuple[str, Path]] = []
    # `out` живёт у вызывающего: частичный результат переживает исключение.
    result = out
    mode = daemon_storage_mode(log_path)
    if mode is not None:
        result["first_edit_storage_mode"] = mode
    # Режим хранилища не подменяем и не блокируем замер: к моменту открытия
    # extras-гейта in-memory база уже сброшена и переоткрыта на диске
    # (прогрессивный Ready для памяти не выставляется), так что правка меряется
    # по той же дисковой базе, что и обычно.
    # A folder is declared ready for non-search tools before the add-on layer is
    # finished (progressive readiness), and an edit in that window is not seen by
    # the watcher at all (verified: the marker never reached functions/calls/
    # proc_call_graph). Wait for the extras gate to open, then measure the first
    # edit of a fully ready folder.
    probe = "PerfProbeNoSuchProcedure"
    extras_deadline = time.monotonic() + min(timeout, 1200.0)
    rid = 400
    unavailable = 0
    while time.monotonic() < extras_deadline:
        value = call(
            "find_path_bsl",
            {"repo": "perf", "from": probe, "to": probe, "_poll": rid},
            rid,
        )
        rid += 1
        if isinstance(value, dict) and "found" in value:
            result["first_edit_extras_ready_s"] = round(time.monotonic() - started, 1)
            break
        # Отличаем «слой ещё строится» от «демон не отвечает»: у второго своя
        # причина отказа, и ждать его двадцать минут бессмысленно. Транспортный
        # None (RPC/сеть) — тоже недоступность, а не ожидание.
        if value is None or (
            isinstance(value, dict)
            and (
                value.get("error")
                or value.get("status")
                in ("daemon_offline", "unknown_repo", "error", "not_started")
            )
        ):
            unavailable += 1
            if unavailable >= 15:  # ~30 секунд подряд
                result["first_edit_s"] = None
                result["first_edit_skip"] = "daemon unavailable"
                result["first_edit_extras_ready_s"] = None
                return result
        else:
            # Гейт ещё строится — это ожидание, а не отказ.
            unavailable = 0
        time.sleep(2)
    else:
        result["first_edit_s"] = None
        result["first_edit_skip"] = "extras layer not ready"
        result["first_edit_extras_ready_s"] = None
        return result

    # Слежение включается после первичной сборки, примерно на открытии гейта.
    # Ждём строку журнала: иначе правка может быть записана до старта watcher'а
    # и потеряна, а сводка первичной сборки — попасть в срез до offset.
    watcher_deadline = time.monotonic() + min(timeout, 60.0)
    result["first_edit_watcher_ready"] = wait_for_log_marker(
        log_path, "слежение за файлами включено", watcher_deadline
    )
    if not result["first_edit_watcher_ready"]:
        if result.get("first_edit_storage_mode") is not None:
            # Современная версия пишет и строку режима, и строку слежения:
            # если слежения нет — watcher не поднялся, правка потерялась бы.
            result["first_edit_s"] = None
            result["first_edit_skip"] = "watcher not started"
            return result
        # Легаси-версии (до 1.8.0) строки о слежении не пишут, а Ready там
        # выставляется прямо перед create_watcher — ждать нечего, меряем.
        result["first_edit_watcher_legacy"] = True

    for dirpath, dirnames, filenames in os.walk(repo):
        dirnames[:] = [d for d in dirnames if d != ".code-index"]
        for name in filenames:
            if not name.lower().endswith(".bsl"):
                continue
            path = Path(dirpath) / name
            try:
                size = path.stat().st_size
            except OSError:
                continue
            if 0 < size < 200_000:
                candidates.append((str(path.relative_to(repo)), path))
    if not candidates:
        result["first_edit_s"] = None
        result["first_edit_skip"] = "no .bsl under 200 KB"
        return result
    candidates.sort()
    target = candidates[0][1]

    hex_id = secrets.token_hex(4)
    caller = f"PerfEditCaller{hex_id}"
    callee = f"PerfEditCallee{hex_id}"
    result["first_edit_target"] = str(target.relative_to(repo))
    # Прерванный прогон мог оставить свои процедуры: чиним файл до замера,
    # иначе «оригинал» уже содержал бы чужой маркер, и restore закреплял бы его.
    raw = target.read_bytes()
    original = strip_perf_edit_markers(raw)
    if b"PerfEdit" in original:
        # Обрезанный жёстким завершением маркер безопасно не разобрать — не
        # меряем на заведомо грязном файле.
        result["first_edit_s"] = None
        result["first_edit_skip"] = "unclean PerfEdit leftovers in target file"
        return result
    if original != raw:
        target.write_bytes(original)
        result["first_edit_repaired"] = True
    addition = (
        f"\n\nПроцедура {caller}() Экспорт\n"
        f"    {callee}();\n"
        "КонецПроцедуры\n\n"
        f"Процедура {callee}() Экспорт\n"
        "КонецПроцедуры\n"
    ).encode()

    result["first_edit_pair"] = f"{caller}->{callee}"
    deadline = time.monotonic() + min(timeout, 900.0)
    path_s: float | None = None
    core_s: float | None = None
    busy_seen = False
    health_missing = 0
    try:
        log_offset = log_size(log_path)
        target.write_bytes(original + addition)
        edit_started = time.monotonic()
        while time.monotonic() < deadline:
            status = repo_path_status(call("health", {}, rid), "perf")
            rid += 1
            if status is None:
                # health не ответил/сменил формат — считаем отдельно, иначе
                # молча уйдём в 900-секундный таймаут.
                health_missing += 1
            value = call(
                "find_path_bsl",
                {
                    "repo": "perf",
                    "from": caller,
                    "to": callee,
                    "max_depth": 2,
                    # Unique extra arg: the response cache (15 s TTL for answers
                    # without file deps) hashes all args, so a stale
                    # ``found: false`` otherwise hides the batch finish.
                    "_poll": rid,
                },
                rid,
            )
            rid += 1
            core_value = call(
                "get_function", {"repo": "perf", "name": caller, "_poll": rid}, rid
            )
            rid += 1
            graph_ok = isinstance(value, dict) and value.get("found") is True
            core_ok = function_present(core_value, caller)
            if graph_ok and path_s is None:
                path_s = time.monotonic() - edit_started
            if core_ok and core_s is None:
                core_s = time.monotonic() - edit_started
            if status not in (None, "ready"):
                busy_seen = True
            samples.append(
                {
                    "t": round(time.monotonic() - started, 1),
                    "tool": "first_edit",
                    "kind": (
                        "ready"
                        if (graph_ok and status == "ready")
                        else ("graph_wait" if status == "ready" else str(status))
                    ),
                    "core": core_ok,
                    "head": str(value)[:120],
                }
            )
            if graph_ok and status == "ready":
                result["first_edit_s"] = round(time.monotonic() - edit_started, 1)
                break
            time.sleep(1)
        else:
            result["first_edit_s"] = None
            result["first_edit_timed_out"] = True
        # Сводку батча журнал пишет при его завершении: в срезе от offset лежит
        # батч ДОБАВЛЕНИЯ (restore идёт позже), а номер этапа позиционный —
        # парсим первый блок «граф вызовов» и даём логу догнать.
        for _ in range(10):
            phase = graph_phase_seconds(read_log_slice(log_path, log_offset))
            if phase is not None:
                result["first_edit_graph_phase_s"] = phase
                break
            time.sleep(0.5)
        else:
            # Следа нет не просто так: фиксируем явный None, чтобы в отчёте
            # было видно, что фаза не распарсилась, а не потерялась.
            result["first_edit_graph_phase_s"] = None
    finally:
        try:
            target.write_bytes(original)
            restored = target.read_bytes() == original
            result["first_edit_restored"] = restored
            if not restored:
                result["first_edit_restore_error"] = "bytes differ after restore"
        except OSError as exc:
            result["first_edit_restored"] = False
            result["first_edit_restore_error"] = str(exc)

    result["first_edit_path_s"] = round(path_s, 1) if path_s is not None else None
    result["first_edit_core_s"] = round(core_s, 1) if core_s is not None else None
    result["first_edit_busy_seen"] = busy_seen
    result["first_edit_health_missing"] = health_missing

    # Best effort: let the watcher absorb the restore, so the next version starts
    # from an unmodified dump even if it does not wipe the database first.
    # Фактическая длительность пишется в отчёт: один MCP-вызов имеет свой
    # таймаут (call_timeout), поэтому кап 60 с — мягкий.
    settle_started = time.monotonic()
    settle_deadline = settle_started + 60
    while time.monotonic() < settle_deadline:
        value = call(
            "find_path_bsl",
            {"repo": "perf", "from": caller, "to": callee, "max_depth": 2, "_poll": rid},
            rid,
        )
        rid += 1
        if isinstance(value, dict) and value.get("found") is False:
            break
        time.sleep(1)
    result["first_edit_settle_s"] = round(time.monotonic() - settle_started, 1)
    return result


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
        "baseline_version": binary_version(baseline_bin),
        "current_version": binary_version(current_bin),
        "current_commit": git_rev("HEAD"),
        "runs": args.runs,
        "daemon_runs": 1 if (args.daemon or args.daemon_only) else 0,
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
            gp = result.get("first_edit_graph_phase_s")
            gp_text = f"{gp:.2f}s" if gp is not None else "—"
            print(
                f"  tools_ready={fmt(result.get('tools_ready_s'))} "
                f"search_ready={fmt(result.get('search_ready_s'))} "
                f"first_edit={fmt(result.get('first_edit_s'))} "
                f"path={fmt(result.get('first_edit_path_s'))} "
                f"graph_phase={gp_text}",
                flush=True,
            )
        base = report["daemon"].get("baseline", {})
        cur = report["daemon"].get("current", {})
        for key, label in (
            ("tools_ready_s", "tools ready (non-search MCP)"),
            ("search_ready_s", "search_function ready"),
            ("first_edit_s", "first edit (batch ready)"),
            ("first_edit_path_s", "first edit (graph serves edge)"),
            ("first_edit_graph_phase_s", "first edit: graph phase (log)"),
        ):
            b, c = base.get(key), cur.get(key)
            delta = f"{(c - b) / b * 100:+.1f}%" if b and c else "—"
            if key == "first_edit_graph_phase_s":
                # Фаза мала (десятки мс) — в таблице тоже два знака, как в строке версии.
                b_text = "—" if b is None else f"{b:.2f}s"
                c_text = "—" if c is None else f"{c:.2f}s"
            else:
                b_text, c_text = fmt(b), fmt(c)
            daemon_rows.append((label, b_text, c_text, delta))
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
