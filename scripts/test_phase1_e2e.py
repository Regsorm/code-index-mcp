"""
E2E smoke-тест для code-index v0.7.0 (Phase 1).

Что делает:
  1. Подключается к MCP-серверу (по умолчанию http://127.0.0.1:8013/mcp).
  2. Через get_stats без repo получает список всех подключённых репо
     (локальных + remote через federation).
  3. Для каждого репо параллельно гоняет матрицу tool-ов:
       — health-набор (get_stats(repo))
       — Phase 1 (list_files, stat_file, read_file, grep_text)
       — symbol-поиски (search_function/class/text, find_symbol, get_function/class)
       — граф вызовов (get_callers/callees, get_imports)
       — grep_body с/без context_lines
       — path_glob фильтры в search_function/grep_body
  4. Адаптивные аргументы: путь и имена символов берутся из самого репо
     (list_files → first file, search_function → first hit).
  5. Печатает per-repo матрицу PASS/SKIP/FAIL с компактными деталями
     и финальную сводку.

Запуск:
    python scripts/test_phase1_e2e.py
    python scripts/test_phase1_e2e.py --url http://127.0.0.1:8013/mcp
    python scripts/test_phase1_e2e.py --only ut,bp1            # подмножество репо
    python scripts/test_phase1_e2e.py --workers 4             # parallel repos

Зависимости — только stdlib (urllib, json, ThreadPoolExecutor).
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import Any

DEFAULT_URL = "http://127.0.0.1:8013/mcp"
TIMEOUT_S = 20

# Включаем UTF-8 для stdout — на Windows console по умолчанию cp1251.
try:
    sys.stdout.reconfigure(encoding="utf-8")  # type: ignore[attr-defined]
except Exception:
    pass

OK = "OK  "
SKIP = "skip"
FAIL = "FAIL"


# ─── MCP HTTP client ────────────────────────────────────────────────────────


class MCPError(Exception):
    pass


class MCPClient:
    """Один streamable-HTTP клиент = одна сессия. Не thread-safe — на каждый
    воркер заводится свой клиент через `from_url`."""

    def __init__(self, url: str, session_id: str):
        self.url = url
        self.session_id = session_id

    @classmethod
    def from_url(cls, url: str) -> "MCPClient":
        # initialize: получаем mcp-session-id из заголовков
        body = json.dumps(
            {
                "jsonrpc": "2.0",
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "phase1-e2e", "version": "1"},
                },
                "id": 1,
            }
        ).encode("utf-8")
        req = urllib.request.Request(
            url,
            data=body,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "Accept": "application/json, text/event-stream",
            },
        )
        with urllib.request.urlopen(req, timeout=TIMEOUT_S) as resp:
            sid = resp.headers.get("mcp-session-id")
            if not sid:
                raise MCPError("сервер не вернул mcp-session-id")
            # Прочитать тело, чтобы соединение могло закрыться корректно.
            resp.read()
        client = cls(url, sid)
        # Отправить notifications/initialized (без id, без ответа).
        client._post(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {},
                }
            ).encode("utf-8"),
            expect_response=False,
        )
        return client

    def _post(self, body: bytes, expect_response: bool = True) -> bytes:
        req = urllib.request.Request(
            self.url,
            data=body,
            method="POST",
            headers={
                "Content-Type": "application/json",
                "Accept": "application/json, text/event-stream",
                "mcp-session-id": self.session_id,
            },
        )
        with urllib.request.urlopen(req, timeout=TIMEOUT_S) as resp:
            data = resp.read()
        if not expect_response:
            return b""
        return data

    def call(self, tool: str, args: dict, request_id: int = 100) -> Any:
        """Вызов tool. Распаковывает SSE и возвращает либо JSON-разобранный
        первый аргумент result.content[0].text (типичный формат code-index),
        либо весь result, если структура иная.
        Бросает MCPError на network-ошибки и protocol error."""
        body = json.dumps(
            {
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": tool, "arguments": args},
                "id": request_id,
            }
        ).encode("utf-8")
        try:
            raw = self._post(body)
        except urllib.error.HTTPError as e:
            raise MCPError(f"HTTP {e.code}: {e.reason}")
        except urllib.error.URLError as e:
            raise MCPError(f"URL: {e.reason}")
        # Парсим SSE. Каждая запись — `data: ...\n`. Берём первую строку
        # data:, в которой есть JSON-RPC result или error.
        for line in raw.decode("utf-8", errors="replace").splitlines():
            line = line.strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload:
                continue
            try:
                doc = json.loads(payload)
            except json.JSONDecodeError:
                continue
            if "error" in doc:
                raise MCPError(f"jsonrpc error: {doc['error']}")
            result = doc.get("result")
            if result is None:
                continue
            content = result.get("content")
            if isinstance(content, list) and content:
                first = content[0]
                if isinstance(first, dict) and "text" in first:
                    text = first["text"]
                    try:
                        return json.loads(text)
                    except json.JSONDecodeError:
                        return text
            return result
        raise MCPError("пустой SSE-ответ от сервера")


# ─── Тестовые проверки ──────────────────────────────────────────────────────


@dataclass
class TestCase:
    name: str          # короткое имя теста, попадает в матрицу
    status: str        # OK / SKIP / FAIL
    duration_ms: int
    detail: str = ""   # короткий текст для FAIL/SKIP/информативный для OK


@dataclass
class RepoReport:
    alias: str
    location: str  # local|remote
    cases: list[TestCase] = field(default_factory=list)

    def add(self, name: str, status: str, t_ms: int, detail: str = ""):
        self.cases.append(TestCase(name, status, t_ms, detail))

    @property
    def stats(self) -> tuple[int, int, int]:
        ok = sum(1 for c in self.cases if c.status == OK)
        skip = sum(1 for c in self.cases if c.status == SKIP)
        fail = sum(1 for c in self.cases if c.status == FAIL)
        return ok, skip, fail


def _has_unavailable_status(value: Any) -> str | None:
    """Если ответ tool-а содержит явно unavailable-статус (federation_error,
    daemon_offline, indexing, ...), вернуть его текст. Иначе None."""
    if isinstance(value, dict):
        st = value.get("status")
        if st in {"federation_error", "daemon_offline", "indexing", "error"}:
            return f"status={st}: {value.get('message') or value.get('error') or ''}"
        if "error" in value and isinstance(value["error"], str):
            return f"error: {value['error']}"
    return None


def timed_call(client: MCPClient, tool: str, args: dict) -> tuple[Any, int, str | None]:
    """Возвращает (response, duration_ms, error_or_none).
    Только сетевые/протокольные ошибки попадают в error_or_none — content-уровневые
    (например `{"error": ...}` от tool-а) приходят как обычный response."""
    t0 = time.monotonic()
    try:
        r = client.call(tool, args)
    except MCPError as e:
        dt = int((time.monotonic() - t0) * 1000)
        return None, dt, str(e)
    dt = int((time.monotonic() - t0) * 1000)
    return r, dt, None


def run_repo(url: str, alias: str, location: str) -> RepoReport:
    rep = RepoReport(alias=alias, location=location)
    try:
        client = MCPClient.from_url(url)
    except Exception as e:
        rep.add("init", FAIL, 0, f"MCP init: {e}")
        return rep

    # 1. get_stats(repo) — должен быть «zero» dict {repo, db, daemon, path}
    r, dt, err = timed_call(client, "get_stats", {"repo": alias})
    if err:
        rep.add("get_stats", FAIL, dt, err)
        return rep
    bad = _has_unavailable_status(r)
    if bad:
        rep.add("get_stats", FAIL, dt, bad)
        # дальше не идём — репо недоступен
        return rep
    db = r.get("db") if isinstance(r, dict) else None
    if not isinstance(db, dict):
        rep.add("get_stats", FAIL, dt, "нет db в ответе")
        return rep
    n_files = int(db.get("total_files", 0))
    n_funcs = int(db.get("total_functions", 0))
    n_classes = int(db.get("total_classes", 0))
    n_text = int(db.get("total_text_files", 0))
    rep.add(
        "get_stats",
        OK,
        dt,
        f"files={n_files} funcs={n_funcs} cls={n_classes} text={n_text}",
    )
    if n_files == 0:
        # Пустой/неинициализированный — остальное skip
        for t in (
            "list_files", "stat_file", "read_file_text", "read_file_code",
            "grep_text", "search_function", "search_function+glob", "search_class",
            "search_text", "find_symbol", "get_function", "get_class", "get_imports",
            "get_callers", "get_callees", "get_file_summary",
            "grep_body", "grep_body+ctx", "grep_body+glob",
        ):
            rep.add(t, SKIP, 0, "репо пустое (total_files=0)")
        return rep

    # 2. list_files(limit=10) — берём кандидата на чтение
    r, dt, err = timed_call(client, "list_files", {"repo": alias, "limit": 10})
    if err:
        rep.add("list_files", FAIL, dt, err)
        files = []
    elif _has_unavailable_status(r):
        rep.add("list_files", FAIL, dt, _has_unavailable_status(r))
        files = []
    elif not isinstance(r, list):
        rep.add("list_files", FAIL, dt, f"ожидался array, получен {type(r).__name__}")
        files = []
    else:
        files = r
        sample_paths = ", ".join(f["path"] for f in files[:2])
        rep.add("list_files", OK, dt, f"got {len(files)} | {sample_paths}")

    # Разделим на text и code-кандидатов через stat_file (узнаем category).
    sample_text_path: str | None = None
    sample_code_path: str | None = None

    # 3. stat_file для первого файла + поиск text/code-кандидатов
    if files:
        first = files[0]
        path = first["path"]
        r, dt, err = timed_call(client, "stat_file", {"repo": alias, "path": path})
        if err:
            rep.add("stat_file", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("stat_file", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, dict) or not r.get("exists"):
            rep.add("stat_file", FAIL, dt, f"exists=False для известного файла {path}")
        else:
            cat = r.get("category")
            req = ["language", "size", "lines_total", "content_hash", "indexed_at"]
            missing = [k for k in req if k not in r]
            if missing:
                rep.add("stat_file", FAIL, dt, f"нет полей {missing}")
            else:
                rep.add("stat_file", OK, dt, f"category={cat} lines={r.get('lines_total')}")
        # Найти кандидатов text + code
        for f in files:
            sr, _, _ = timed_call(client, "stat_file", {"repo": alias, "path": f["path"]})
            if isinstance(sr, dict):
                c = sr.get("category")
                if c == "text" and sample_text_path is None:
                    sample_text_path = f["path"]
                elif c == "code" and sample_code_path is None:
                    sample_code_path = f["path"]
            if sample_text_path and sample_code_path:
                break
    else:
        rep.add("stat_file", SKIP, 0, "list_files пуст")

    # 4. read_file для text-файла (должно быть содержимое)
    if sample_text_path:
        r, dt, err = timed_call(
            client,
            "read_file",
            {"repo": alias, "path": sample_text_path, "line_start": 1, "line_end": 3},
        )
        if err:
            rep.add("read_file_text", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("read_file_text", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, dict):
            rep.add("read_file_text", FAIL, dt, f"ожидался dict, получен {type(r).__name__}")
        else:
            cat = r.get("category")
            content = r.get("content", "")
            lines = r.get("lines_returned", 0)
            if cat != "text":
                rep.add(
                    "read_file_text",
                    FAIL,
                    dt,
                    f"ожидалась category=text, получено {cat}",
                )
            elif lines == 0 or not content:
                rep.add("read_file_text", FAIL, dt, "пустой content для text-файла")
            else:
                rep.add(
                    "read_file_text",
                    OK,
                    dt,
                    f"lines={lines}, len={len(content)}",
                )
    else:
        rep.add("read_file_text", SKIP, 0, "нет text-файлов в первой партии")

    # 5. read_file для code-файла (Phase 1: category=code, content="")
    if sample_code_path:
        r, dt, err = timed_call(
            client,
            "read_file",
            {"repo": alias, "path": sample_code_path},
        )
        if err:
            rep.add("read_file_code", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("read_file_code", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, dict):
            rep.add("read_file_code", FAIL, dt, f"ожидался dict, получен {type(r).__name__}")
        else:
            cat = r.get("category")
            content = r.get("content", None)
            if cat != "code":
                rep.add(
                    "read_file_code",
                    FAIL,
                    dt,
                    f"ожидалась category=code, получено {cat}",
                )
            elif content != "":
                rep.add(
                    "read_file_code",
                    FAIL,
                    dt,
                    "Phase 1: для code должна быть пустая строка",
                )
            else:
                rep.add("read_file_code", OK, dt, "category=code, content=''")
    else:
        rep.add("read_file_code", SKIP, 0, "нет code-файлов в первой партии")

    # 6. grep_text — простой regex по text-кандидату
    if sample_text_path and n_text > 0:
        r, dt, err = timed_call(
            client,
            "grep_text",
            {
                "repo": alias,
                "regex": ".+",
                "path_glob": sample_text_path,
                "limit": 3,
                "context_lines": 1,
            },
        )
        if err:
            rep.add("grep_text", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("grep_text", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("grep_text", FAIL, dt, f"ожидался array, получен {type(r).__name__}")
        elif not r:
            rep.add("grep_text", FAIL, dt, "regex .+ не нашёл ни одной строки")
        else:
            first = r[0]
            req = ["path", "line", "content", "context"]
            missing = [k for k in req if k not in first]
            if missing:
                rep.add("grep_text", FAIL, dt, f"нет полей {missing}")
            elif not isinstance(first["context"], list) or not first["context"]:
                rep.add("grep_text", FAIL, dt, "context_lines=1 должен дать context")
            else:
                rep.add(
                    "grep_text", OK, dt,
                    f"hits={len(r)}, ctx@first={len(first['context'])}",
                )
    else:
        rep.add("grep_text", SKIP, 0, "нет text-файлов")

    # 7. search_function — общая FTS, query="*" не работает в FTS5, берём токен
    sample_func_name: str | None = None
    sample_func_path: str | None = None
    r, dt, err = timed_call(
        client, "search_function", {"repo": alias, "query": "init", "limit": 5}
    )
    if err:
        rep.add("search_function", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("search_function", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("search_function", FAIL, dt, f"ожидался array, получен {type(r).__name__}")
    else:
        rep.add("search_function", OK, dt, f"hits={len(r)}")
        if r and isinstance(r[0], dict):
            sample_func_name = r[0].get("name") or r[0].get("qualified_name")

    # Узнать path функции через get_file_summary невозможно по name — пропустим.
    # Для path_glob используем "*<sample_func_path_part>" — упрощённо.

    # 8. search_function + path_glob — ограничим поиск extensions/* на VM,
    # cn/* на cnotedb и т.п. Используем эвристику: glob `*<delim>*`,
    # где delim — какой-то частый сегмент пути из first list_files.
    glob_for_search = None
    if files:
        first_path = files[0]["path"]
        # Берём первый сегмент если есть слеш, иначе пропускаем.
        if "/" in first_path:
            seg = first_path.split("/", 1)[0]
            glob_for_search = f"{seg}/**"
    if glob_for_search:
        r, dt, err = timed_call(
            client,
            "search_function",
            {"repo": alias, "query": "init", "limit": 5, "path_glob": glob_for_search},
        )
        if err:
            rep.add("search_function+glob", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("search_function+glob", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("search_function+glob", FAIL, dt, f"non-array")
        else:
            rep.add(
                "search_function+glob",
                OK,
                dt,
                f"glob={glob_for_search} hits={len(r)}",
            )
    else:
        rep.add("search_function+glob", SKIP, 0, "не подобран glob")

    # 9. search_class
    r, dt, err = timed_call(
        client, "search_class", {"repo": alias, "query": "Test", "limit": 3}
    )
    if err:
        rep.add("search_class", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("search_class", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("search_class", FAIL, dt, "non-array")
    else:
        rep.add("search_class", OK, dt, f"hits={len(r)}")

    # 10. search_text
    r, dt, err = timed_call(
        client, "search_text", {"repo": alias, "query": "TODO", "limit": 3}
    )
    if err:
        rep.add("search_text", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("search_text", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("search_text", FAIL, dt, "non-array")
    else:
        rep.add("search_text", OK, dt, f"hits={len(r)}")

    # 11. find_symbol по имени из search_function
    if sample_func_name:
        r, dt, err = timed_call(
            client, "find_symbol", {"repo": alias, "name": sample_func_name}
        )
        if err:
            rep.add("find_symbol", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("find_symbol", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, dict):
            rep.add("find_symbol", FAIL, dt, "non-dict")
        else:
            req = ["functions", "classes", "variables", "imports"]
            missing = [k for k in req if k not in r]
            if missing:
                rep.add("find_symbol", FAIL, dt, f"нет полей {missing}")
            else:
                cnt = sum(len(r[k]) for k in req if isinstance(r[k], list))
                rep.add("find_symbol", OK, dt, f"name={sample_func_name!r} total={cnt}")
    else:
        rep.add("find_symbol", SKIP, 0, "нет sample_func_name")

    # 12. get_function — точное имя
    if sample_func_name:
        r, dt, err = timed_call(
            client, "get_function", {"repo": alias, "name": sample_func_name}
        )
        if err:
            rep.add("get_function", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("get_function", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("get_function", FAIL, dt, "non-array")
        else:
            rep.add("get_function", OK, dt, f"hits={len(r)}")
    else:
        rep.add("get_function", SKIP, 0, "нет sample_func_name")

    # 13. get_class — попробуем стандартное «init»-имя класса
    r, dt, err = timed_call(
        client, "get_class", {"repo": alias, "name": "Test"}
    )
    if err:
        rep.add("get_class", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("get_class", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("get_class", FAIL, dt, "non-array")
    else:
        rep.add("get_class", OK, dt, f"hits={len(r)}")

    # 14. get_callers / get_callees
    if sample_func_name:
        r, dt, err = timed_call(
            client, "get_callers", {"repo": alias, "function_name": sample_func_name}
        )
        if err:
            rep.add("get_callers", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("get_callers", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("get_callers", FAIL, dt, "non-array")
        else:
            rep.add("get_callers", OK, dt, f"hits={len(r)}")

        r, dt, err = timed_call(
            client, "get_callees", {"repo": alias, "function_name": sample_func_name}
        )
        if err:
            rep.add("get_callees", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("get_callees", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("get_callees", FAIL, dt, "non-array")
        else:
            rep.add("get_callees", OK, dt, f"hits={len(r)}")
    else:
        rep.add("get_callers", SKIP, 0, "нет sample_func_name")
        rep.add("get_callees", SKIP, 0, "нет sample_func_name")

    # 15. get_imports — по модулю «os» (типичный Python-импорт)
    r, dt, err = timed_call(
        client, "get_imports", {"repo": alias, "module": "os"}
    )
    if err:
        rep.add("get_imports", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("get_imports", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("get_imports", FAIL, dt, "non-array")
    else:
        rep.add("get_imports", OK, dt, f"hits={len(r)}")

    # 16. get_file_summary — по первому файлу
    if files:
        r, dt, err = timed_call(
            client, "get_file_summary", {"repo": alias, "path": files[0]["path"]}
        )
        if err:
            rep.add("get_file_summary", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("get_file_summary", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, dict):
            rep.add("get_file_summary", FAIL, dt, "non-dict")
        else:
            funcs = len(r.get("functions", []))
            cls = len(r.get("classes", []))
            rep.add("get_file_summary", OK, dt, f"funcs={funcs} cls={cls}")
    else:
        rep.add("get_file_summary", SKIP, 0, "list_files пуст")

    # 17. grep_body (без context, без glob) — short word
    r, dt, err = timed_call(
        client, "grep_body", {"repo": alias, "pattern": "return", "limit": 3}
    )
    if err:
        rep.add("grep_body", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("grep_body", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("grep_body", FAIL, dt, "non-array")
    else:
        rep.add("grep_body", OK, dt, f"hits={len(r)}")

    # 18. grep_body + context_lines=2
    r, dt, err = timed_call(
        client,
        "grep_body",
        {"repo": alias, "pattern": "return", "limit": 1, "context_lines": 2},
    )
    if err:
        rep.add("grep_body+ctx", FAIL, dt, err)
    elif _has_unavailable_status(r):
        rep.add("grep_body+ctx", FAIL, dt, _has_unavailable_status(r))
    elif not isinstance(r, list):
        rep.add("grep_body+ctx", FAIL, dt, "non-array")
    elif not r:
        rep.add("grep_body+ctx", SKIP, dt, "нет матчей на 'return'")
    else:
        first = r[0]
        if not isinstance(first.get("context"), list):
            rep.add("grep_body+ctx", FAIL, dt, "context отсутствует или не list")
        elif not first["context"]:
            rep.add("grep_body+ctx", FAIL, dt, "context пуст при context_lines=2")
        else:
            rep.add(
                "grep_body+ctx", OK, dt,
                f"hits={len(r)} ctx={len(first['context'])}",
            )

    # 19. grep_body + path_glob
    if glob_for_search:
        r, dt, err = timed_call(
            client,
            "grep_body",
            {
                "repo": alias,
                "pattern": "return",
                "path_glob": glob_for_search,
                "limit": 3,
            },
        )
        if err:
            rep.add("grep_body+glob", FAIL, dt, err)
        elif _has_unavailable_status(r):
            rep.add("grep_body+glob", FAIL, dt, _has_unavailable_status(r))
        elif not isinstance(r, list):
            rep.add("grep_body+glob", FAIL, dt, "non-array")
        else:
            rep.add("grep_body+glob", OK, dt, f"hits={len(r)}")
    else:
        rep.add("grep_body+glob", SKIP, 0, "не подобран glob")

    return rep


# ─── Печать отчёта ──────────────────────────────────────────────────────────


def print_report(reports: list[RepoReport]) -> None:
    # Выводим репо в порядке local → remote, потом по алиасу.
    reports.sort(key=lambda r: (r.location != "local", r.alias))

    # Сначала per-repo детальная таблица.
    for rep in reports:
        ok, skip, fail = rep.stats
        loc = rep.location.upper()
        print(f"\n=== {rep.alias} [{loc}] — {ok}{OK} / {skip}{SKIP} / {fail}{FAIL} ===")
        for c in rep.cases:
            line = f"  {c.status} {c.name:<22} {c.duration_ms:>5}ms  {c.detail}"
            print(line)

    # Финальная сводка.
    total_ok = sum(r.stats[0] for r in reports)
    total_skip = sum(r.stats[1] for r in reports)
    total_fail = sum(r.stats[2] for r in reports)
    print("\n" + "═" * 70)
    print(
        f"ИТОГО по {len(reports)} репо: "
        f"{total_ok}{OK}  {total_skip}{SKIP}  {total_fail}{FAIL}"
    )
    if total_fail:
        print("\nFAIL:")
        for rep in reports:
            for c in rep.cases:
                if c.status == FAIL:
                    print(f"  [{rep.alias}] {c.name}: {c.detail}")
    print("═" * 70)


# ─── main ───────────────────────────────────────────────────────────────────


def discover_repos(url: str) -> list[tuple[str, str]]:
    """Возвращает [(alias, location), ...]. location = local|remote.
    Берёт через get_stats() (без repo): для local — есть ключ `db`, для
    remote — ключ `status` ("ok") и `ip` (если включена федерация).
    Полагаемся на тот факт, что `get_stats` возвращает {repos: [...]}."""
    client = MCPClient.from_url(url)
    r, _, err = timed_call(client, "get_stats", {})
    if err:
        raise SystemExit(f"get_stats упал: {err}")
    if not isinstance(r, dict) or "repos" not in r:
        raise SystemExit(f"get_stats без repo вернул неожиданное: {type(r).__name__}")
    out: list[tuple[str, str]] = []
    for entry in r["repos"]:
        alias = entry.get("repo")
        if not alias:
            continue
        # Эвристика: если в записи есть `db` — локально (полный stats),
        # если `status` явно «unreachable»/«parse_error» — remote, недоступен.
        if "db" in entry:
            loc = "local"
        else:
            loc = "remote"
        out.append((alias, loc))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=DEFAULT_URL)
    ap.add_argument(
        "--only",
        default="",
        help="через запятую — только эти алиасы (по умолчанию все из get_stats)",
    )
    ap.add_argument(
        "--workers", type=int, default=4,
        help="параллельных репо одновременно (default 4)",
    )
    args = ap.parse_args()

    print(f"MCP URL: {args.url}")
    print("Discover repos…")
    t0 = time.monotonic()
    pairs = discover_repos(args.url)
    print(f"  found {len(pairs)} repos: " + ", ".join(a for a, _ in pairs))

    if args.only:
        wanted = {x.strip() for x in args.only.split(",")}
        pairs = [(a, l) for a, l in pairs if a in wanted]
        print(f"  filtered to: " + ", ".join(a for a, _ in pairs))

    print(f"\nRunning tests with {args.workers} workers…")
    reports: list[RepoReport] = []
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futs = {pool.submit(run_repo, args.url, a, l): a for a, l in pairs}
        for fut in as_completed(futs):
            rep = fut.result()
            reports.append(rep)

    elapsed = time.monotonic() - t0
    print_report(reports)
    print(f"Total elapsed: {elapsed:.1f}s")

    # Exit code
    n_fail = sum(r.stats[2] for r in reports)
    sys.exit(1 if n_fail else 0)


if __name__ == "__main__":
    main()
