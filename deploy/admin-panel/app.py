#!/usr/bin/env python3
"""Веб-панель администрирования баз code-index / bsl-indexer.

Показывает базы из `daemon.toml`, их статус (по `/health` демона), размер исходников
и индекса; позволяет добавить базу (git clone, ZIP с выгрузкой, готовый каталог),
сделать `git pull` и удалить базу.

Только стандартная библиотека Python 3.11+. Собственной авторизации нет —
панель рассчитана на работу за обратным прокси (см. README.md).

Настройки — переменные окружения:
    CODE_INDEX_HOME        каталог демона: daemon.toml, daemon.json, daemon.log (обязательно)
    ADMIN_REPOS_ROOT       корень, в котором панель создаёт и удаляет базы (по умолчанию /srv/repos)
    ADMIN_HOST, ADMIN_PORT адрес панели (по умолчанию 127.0.0.1:8080)
    ADMIN_MCP_URL          адрес MCP для блока подключения (по умолчанию пусто — блок без URL)
    ADMIN_MCP_TOKEN_FILE   файл с токеном для заголовка Authorization (необязательно)
    ADMIN_SERVE_UNIT       systemd-юнит serve; если задан — перезапускается после изменения списка баз
"""
import json
import os
import re
import shutil
import subprocess
import threading
import time
import tomllib
import urllib.request
import zipfile
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HOME = os.environ.get("CODE_INDEX_HOME") or ""
CONFIG = os.path.join(HOME, "daemon.toml")
DAEMON_JSON = os.path.join(HOME, "daemon.json")
LOG = os.path.join(HOME, "daemon.log")
META = os.path.join(HOME, "admin-meta.json")
REPOS = os.path.realpath(os.environ.get("ADMIN_REPOS_ROOT", "/srv/repos"))
TMP = os.path.join(REPOS, ".admin-tmp")
LISTEN = (os.environ.get("ADMIN_HOST", "127.0.0.1"), int(os.environ.get("ADMIN_PORT", "8080")))
MCP_URL = os.environ.get("ADMIN_MCP_URL", "")
MCP_TOKEN_FILE = os.environ.get("ADMIN_MCP_TOKEN_FILE", "")
SERVE_UNIT = os.environ.get("ADMIN_SERVE_UNIT", "")
ALIAS_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,39}$")
SIZE_TTL = 300  # секунд между пересчётами размеров

lock = threading.RLock()
jobs = {}  # alias -> {"state", "message", "started"} — фоновые операции
size_cache = {}  # alias -> (время, байты исходников, байты индекса, файлов)
sizing = set()


# ---------- daemon.toml и служебные данные ----------

def config_paths():
    with open(CONFIG, "rb") as f:
        return tomllib.load(f).get("paths", []) or []


def write_paths(paths):
    """Переписывает только секции [[paths]]; всё, что стоит до первой из них, сохраняется."""
    with open(CONFIG, encoding="utf-8") as f:
        text = f.read()
    parts = [text.split("[[paths]]", 1)[0].rstrip() + "\n"]
    for p in paths:
        parts.append("\n[[paths]]\n")
        parts.append(f'path = {json.dumps(p["path"], ensure_ascii=False)}\n')
        parts.append(f'alias = "{p["alias"]}"\n')
        for key, value in p.items():
            if key not in ("path", "alias"):
                parts.append(f"{key} = {toml_value(value)}\n")
    tmp = CONFIG + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write("".join(parts))
    os.replace(tmp, CONFIG)


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    return json.dumps(str(value), ensure_ascii=False)


def load_meta():
    try:
        with open(META, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_meta(meta):
    tmp = META + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=1)
    os.replace(tmp, META)


def restart_serve(any_paths):
    """serve читает список баз при старте — после изменения его нужно перезапустить."""
    if SERVE_UNIT:
        action = "restart" if any_paths else "stop"
        subprocess.run(["sudo", "-n", "systemctl", action, SERVE_UNIT], check=False)


def add_path(alias, path, git):
    with lock:
        paths = [p for p in config_paths() if p.get("alias") != alias]
        entry = {"path": path, "alias": alias}
        if git:
            entry["git"] = True
        paths.append(entry)
        write_paths(paths)
    restart_serve(True)


def remove_path(alias):
    with lock:
        paths = [p for p in config_paths() if p.get("alias") != alias]
        write_paths(paths)
    restart_serve(bool(paths))


def inside_repos(path):
    real = os.path.realpath(path)
    return real != REPOS and real.startswith(REPOS + os.sep)


def mask_url(url):
    return re.sub(r"//[^/@]+@", "//***@", url or "")


def now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


# ---------- сведения для панели ----------

def daemon_health():
    try:
        with open(DAEMON_JSON, encoding="utf-8") as f:
            info = json.load(f)
        url = f'http://{info.get("http_host", "127.0.0.1")}:{info["http_port"]}/health'
        with urllib.request.urlopen(url, timeout=3) as r:
            return json.load(r)
    except Exception as e:  # noqa: BLE001 — причина показывается в панели
        return {"status": "unavailable", "error": str(e), "paths": []}


def dir_size(path):
    """(байты исходников без .git и .code-index, байты .code-index, число файлов исходников)."""
    src = idx = files = 0
    for root, _dirs, names in os.walk(path):
        top = os.path.relpath(root, path).split(os.sep, 1)[0]
        for name in names:
            try:
                size = os.lstat(os.path.join(root, name)).st_size
            except OSError:
                continue
            if top == ".code-index":
                idx += size
            elif top != ".git":
                src += size
                files += 1
    return src, idx, files


def sizes(alias, path):
    """Размеры из кэша; пересчёт идёт в фоне — обход крупной выгрузки занимает десятки секунд."""
    cached = size_cache.get(alias)
    if (not cached or time.time() - cached[0] > SIZE_TTL) and alias not in sizing:
        sizing.add(alias)

        def work():
            try:
                result = dir_size(path) if os.path.isdir(path) else (0, 0, 0)
                size_cache[alias] = (time.time(), *result)
            finally:
                sizing.discard(alias)
        threading.Thread(target=work, daemon=True).start()
    return cached[1:] if cached else (None, None, None)


def git_head(path):
    if not os.path.isdir(os.path.join(path, ".git")):
        return None
    r = subprocess.run(["git", "-C", path, "log", "-1", "--format=%h %cs %s"],
                       capture_output=True, text=True, timeout=10)
    return r.stdout.strip() or None


def meminfo():
    data = {}
    with open("/proc/meminfo") as f:
        for line in f:
            key, value = line.split(":", 1)
            data[key] = int(value.split()[0]) * 1024
    return {"total": data.get("MemTotal", 0), "available": data.get("MemAvailable", 0)}


def mcp_token():
    if not MCP_TOKEN_FILE:
        return ""
    try:
        with open(MCP_TOKEN_FILE, encoding="utf-8") as f:
            return f.read().strip()
    except OSError:
        return ""


def overview():
    health = daemon_health()
    by_path = {p.get("path"): p for p in health.get("paths", [])}
    meta = load_meta()
    repos, known = [], set()
    for p in config_paths():
        alias, path = p.get("alias"), p.get("path")
        known.add(alias)
        src, idx, files = sizes(alias, path)
        m = meta.get(alias, {})
        repos.append({
            "alias": alias, "path": path, "git": os.path.isdir(os.path.join(path, ".git")),
            "status": by_path.get(path, {}).get("status", "unknown"), "health": by_path.get(path, {}),
            "src_bytes": src, "index_bytes": idx, "files": files,
            "source": m.get("source", "folder"), "url": mask_url(m.get("url")),
            "branch": m.get("branch"), "head": git_head(path), "job": jobs.get(alias),
        })
    for alias, job in jobs.items():
        if alias not in known:
            m = meta.get(alias, {})
            repos.append({"alias": alias, "path": os.path.join(REPOS, alias), "status": "pending",
                          "job": job, "source": m.get("source"), "url": mask_url(m.get("url"))})
    du = shutil.disk_usage(REPOS)
    return {
        "daemon": {k: health.get(k) for k in ("status", "version", "uptime_sec", "error")},
        "disk": {"total": du.total, "used": du.used, "free": du.free},
        "memory": meminfo(), "repos": repos, "repos_root": REPOS,
        "mcp": {"url": MCP_URL, "token": mcp_token()},
    }


# ---------- фоновые операции ----------

def run_job(alias, fn, message):
    jobs[alias] = {"state": "running", "message": message, "started": now()}

    def wrapper():
        try:
            fn()
            jobs.pop(alias, None)
            size_cache.pop(alias, None)
        except Exception as e:  # noqa: BLE001
            jobs[alias] = {"state": "error", "message": str(e)[-2000:], "started": jobs[alias]["started"]}
    threading.Thread(target=wrapper, daemon=True).start()


def git_env():
    return dict(os.environ, GIT_TERMINAL_PROMPT="0")


def do_clone(alias, url, branch):
    dest = os.path.join(REPOS, alias)
    cmd = ["git", "clone", "--quiet"] + (["--branch", branch] if branch else []) + [url, dest]
    r = subprocess.run(cmd, capture_output=True, text=True, env=git_env(), timeout=6 * 3600)
    if r.returncode != 0:
        shutil.rmtree(dest, ignore_errors=True)
        raise RuntimeError(mask_url(r.stderr.strip()) or "git clone завершился с ошибкой")
    add_path(alias, dest, True)


def do_pull(path):
    r = subprocess.run(["git", "-C", path, "pull", "--ff-only", "--quiet"],
                       capture_output=True, text=True, env=git_env(), timeout=3600)
    if r.returncode != 0:
        raise RuntimeError(mask_url(r.stderr.strip()) or "git pull завершился с ошибкой")


def do_unzip(alias, zpath):
    dest = os.path.join(REPOS, alias)
    try:
        with zipfile.ZipFile(zpath) as z:
            root = os.path.realpath(dest)
            for info in z.infolist():
                target = os.path.realpath(os.path.join(dest, info.filename))
                if target != root and not target.startswith(root + os.sep):
                    raise RuntimeError(f"недопустимый путь в архиве: {info.filename}")
            z.extractall(dest)
    except Exception:
        shutil.rmtree(dest, ignore_errors=True)
        raise
    finally:
        os.remove(zpath)
    # Архив с одной корневой папкой (частый случай) — поднимаем её содержимое на уровень выше.
    shutil.rmtree(os.path.join(dest, "__MACOSX"), ignore_errors=True)
    entries = os.listdir(dest)
    if len(entries) == 1 and os.path.isdir(os.path.join(dest, entries[0])):
        inner = os.path.join(dest, entries[0])
        for e in os.listdir(inner):
            shutil.move(os.path.join(inner, e), os.path.join(dest, e))
        os.rmdir(inner)
    add_path(alias, dest, False)


def do_delete(alias, path):
    remove_path(alias)
    time.sleep(2)  # демону нужно время, чтобы отпустить индекс
    if inside_repos(path):
        shutil.rmtree(os.path.realpath(path), ignore_errors=True)
    meta = load_meta()
    meta.pop(alias, None)
    save_meta(meta)


# ---------- HTTP ----------

class Handler(BaseHTTPRequestHandler):
    server_version = "code-index-admin"

    def log_message(self, fmt, *args):
        pass

    def send_json(self, data, code=200):
        body = json.dumps(data, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def fail(self, message, code=400):
        self.send_json({"error": message}, code)

    def read_json(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        u = urlparse(self.path)
        if u.path in ("/", "/index.html"):
            with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "index.html"), "rb") as f:
                body = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif u.path == "/api/overview":
            self.send_json(overview())
        elif u.path == "/api/log":
            lines = min(int(parse_qs(u.query).get("lines", ["200"])[0]), 2000)
            r = subprocess.run(["tail", "-n", str(lines), LOG], capture_output=True, text=True)
            self.send_json({"log": r.stdout})
        else:
            self.fail("не найдено", 404)

    def mutating_allowed(self):
        # Защита от CSRF: обычная HTML-форма не может выставить собственный заголовок.
        if self.headers.get("X-Admin") != "1":
            self.fail("forbidden", 403)
            return False
        return True

    def new_alias_error(self, alias):
        if not ALIAS_RE.match(alias or ""):
            return "Алиас: латиница в нижнем регистре, цифры, «-» и «_», до 40 символов"
        if any(p.get("alias") == alias for p in config_paths()) or alias in jobs:
            return f"База «{alias}» уже есть"
        return None

    def do_POST(self):
        if not self.mutating_allowed():
            return
        u = urlparse(self.path)
        try:
            data = self.read_json()
        except ValueError:
            return self.fail("некорректный JSON")
        alias = (data.get("alias") or "").strip()
        meta = load_meta()

        if u.path == "/api/repos/git":
            url = (data.get("url") or "").strip()
            branch = (data.get("branch") or "").strip() or None
            err = self.new_alias_error(alias)
            if err:
                return self.fail(err)
            if not re.match(r"^(https?://|git@|ssh://)", url):
                return self.fail("Укажите URL git-репозитория (https:// или ssh)")
            if os.path.exists(os.path.join(REPOS, alias)):
                return self.fail(f"Каталог {alias} в {REPOS} уже существует")
            meta[alias] = {"source": "git", "url": url, "branch": branch, "added": now()}
            save_meta(meta)
            run_job(alias, lambda: do_clone(alias, url, branch), "клонирование…")
            return self.send_json({"ok": True})

        if u.path == "/api/repos/folder":
            err = self.new_alias_error(alias)
            if err:
                return self.fail(err)
            path = os.path.join(REPOS, alias)
            if not os.path.isdir(path):
                return self.fail(f"Каталог {path} не найден — сначала скопируйте туда исходники")
            meta[alias] = {"source": "folder", "added": now()}
            save_meta(meta)
            add_path(alias, path, os.path.isdir(os.path.join(path, ".git")))
            return self.send_json({"ok": True})

        repo = next((p for p in config_paths() if p.get("alias") == alias), None)
        if u.path == "/api/repos/pull":
            if not repo or not os.path.isdir(os.path.join(repo["path"], ".git")):
                return self.fail("Это не git-база")
            run_job(alias, lambda: do_pull(repo["path"]), "git pull…")
            return self.send_json({"ok": True})

        if u.path == "/api/repos/delete":
            if repo:
                run_job(alias, lambda: do_delete(alias, repo["path"]), "удаление…")
            elif alias in jobs and jobs[alias]["state"] == "error":
                jobs.pop(alias, None)
                shutil.rmtree(os.path.join(REPOS, alias), ignore_errors=True)
            else:
                return self.fail("База не найдена или операция ещё идёт", 404)
            return self.send_json({"ok": True})

        if u.path == "/api/jobs/clear":
            jobs.pop(alias, None)
            return self.send_json({"ok": True})

        self.fail("не найдено", 404)

    def do_PUT(self):
        """Загрузка ZIP: тело запроса — сам архив, алиас — в строке запроса."""
        if not self.mutating_allowed():
            return
        u = urlparse(self.path)
        if u.path != "/api/repos/zip":
            return self.fail("не найдено", 404)
        alias = parse_qs(u.query).get("alias", [""])[0].strip()
        err = self.new_alias_error(alias)
        if err:
            return self.fail(err)
        if os.path.exists(os.path.join(REPOS, alias)):
            return self.fail(f"Каталог {alias} в {REPOS} уже существует")
        n = int(self.headers.get("Content-Length") or 0)
        if n <= 0:
            return self.fail("пустой файл")
        if n * 4 > shutil.disk_usage(REPOS).free:
            return self.fail("Недостаточно места на диске для распаковки")
        os.makedirs(TMP, exist_ok=True)
        zpath = os.path.join(TMP, alias + ".zip")
        left = n
        with open(zpath, "wb") as f:
            while left:
                chunk = self.rfile.read(min(left, 1 << 20))
                if not chunk:
                    break
                f.write(chunk)
                left -= len(chunk)
        if left or not zipfile.is_zipfile(zpath):
            os.remove(zpath)
            return self.fail("загрузка прервана" if left else "это не ZIP-архив")
        meta = load_meta()
        meta[alias] = {"source": "zip", "added": now()}
        save_meta(meta)
        run_job(alias, lambda: do_unzip(alias, zpath), "распаковка…")
        self.send_json({"ok": True})


def main():
    if not HOME or not os.path.isfile(CONFIG):
        raise SystemExit("Задайте CODE_INDEX_HOME — каталог с daemon.toml")
    os.makedirs(REPOS, exist_ok=True)
    shutil.rmtree(TMP, ignore_errors=True)  # недокачанные архивы прошлого запуска
    print(f"code-index admin: http://{LISTEN[0]}:{LISTEN[1]}  (базы в {REPOS})", flush=True)
    ThreadingHTTPServer(LISTEN, Handler).serve_forever()


if __name__ == "__main__":
    main()
