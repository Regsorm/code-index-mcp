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

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10: tomllib появился только в 3.11
    import tomli as tomllib

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
MAX_JSON_BODY = 1_000_000  # потолок тела POST, байт
MAX_ZIP_ENTRIES = 500_000  # потолок числа файлов в ZIP
ZIP_RESERVE_BYTES = 64 * 1024 * 1024  # запас свободного места при распаковке

lock = threading.RLock()
jobs = {}  # alias -> {"state", "message", "started"} — фоновые операции
size_cache = {}  # alias -> (время, байты исходников, байты индекса, файлов)
sizing = set()

# Строки заголовков TOML. Ищем по началу строки: значения и многострочные
# строки проверяются ещё и повторным разбором результата перед записью.
PATH_HEADER_RE = re.compile(r"(?m)^[ \t]*\[\[[ \t]*paths[ \t]*\]\][ \t]*(?:#.*)?\r?$")
TOP_HEADER_RE = re.compile(r"(?m)^[ \t]*\[[^\r\n]*\][ \t]*(?:#.*)?\r?$")


# ---------- daemon.toml и служебные данные ----------

def config_text():
    with open(CONFIG, encoding="utf-8") as f:
        return f.read()


def config_paths():
    return tomllib.loads(config_text()).get("paths", []) or []


def effective_alias(entry):
    """Алиас записи: явный либо вычисленный так же, как PathEntry::effective_alias."""
    alias = entry.get("alias")
    if alias:
        return str(alias)
    path = str(entry.get("path") or "")
    name = re.split(r"[\\/]", path.rstrip("\\/"))[-1] if path else ""
    # Rust использует to_ascii_lowercase: кириллицу не трогаем.
    name = "".join(chr(ord(c) + 32) if "A" <= c <= "Z" else c for c in name)
    return name.replace(" ", "_") or "default"


def toml_string(value):
    return json.dumps(str(value), ensure_ascii=False)


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    return toml_string(value)


def render_path_block(entry):
    parts = ["[[paths]]\n", f"path = {toml_string(entry['path'])}\n"]
    if entry.get("alias"):
        parts.append(f"alias = {toml_string(entry['alias'])}\n")
    for key, value in entry.items():
        if key not in ("path", "alias"):
            parts.append(f"{key} = {toml_value(value)}\n")
    return "".join(parts)


def path_blocks(text):
    """Границы блоков [[paths]]: от заголовка до следующего заголовка TOML."""
    headers = list(PATH_HEADER_RE.finditer(text))
    all_headers = list(TOP_HEADER_RE.finditer(text))
    blocks = []
    for h in headers:
        nxt = next((x for x in all_headers if x.start() >= h.end()), None)
        blocks.append((h.start(), nxt.start() if nxt else len(text)))
    return blocks


def write_paths(paths):
    """Переписывает только блоки [[paths]], сохраняя остальной текст файла.

    Перед заменой результат разбирается заново: список баз должен совпасть,
    а все прочие секции — остаться прежними. Иначе файл не трогаем.
    """
    text = config_text()
    old = tomllib.loads(text)
    old_entries = old.get("paths", []) or []
    blocks = path_blocks(text)
    if len(blocks) != len(old_entries):
        raise RuntimeError("daemon.toml: не удалось разобрать секции [[paths]] — запись отменена")
    old_aliases = [effective_alias(p) for p in old_entries]
    if len(set(old_aliases)) != len(old_aliases):
        raise RuntimeError("daemon.toml: повторяющиеся алиасы баз — запись отменена")

    wanted = {}
    for p in paths:
        alias = effective_alias(p)
        if alias in wanted:
            raise RuntimeError(f"daemon.toml: алиас «{alias}» указан дважды — запись отменена")
        wanted[alias] = p

    expected = []
    for p in paths:
        alias = effective_alias(p)
        base = next((e for e in old_entries if effective_alias(e) == alias), None)
        if base is None:
            expected.append(p)
        else:
            merged = dict(base)
            merged.update(p)
            expected.append(merged)

    out = []
    cursor = 0
    used = set()
    for (start, end), entry in zip(blocks, old_entries):
        alias = effective_alias(entry)
        out.append(text[cursor:start])
        if alias in wanted:
            new_entry = dict(entry)
            new_entry.update(wanted[alias])
            if new_entry == entry:
                out.append(text[start:end])
            else:
                out.append(render_path_block(new_entry))
                out.append("\n")
            used.add(alias)
        cursor = end
    out.append(text[cursor:])

    additions = [p for p in paths if effective_alias(p) not in used]
    if additions:
        tail = "".join(out)
        if tail and not tail.endswith("\n"):
            tail += "\n"
        if tail and not tail.endswith("\n\n"):
            tail += "\n"
        tail += "\n".join(render_path_block(p) for p in additions)
        out = [tail]

    new_text = "".join(out)
    if not new_text.endswith("\n"):
        new_text += "\n"
    try:
        parsed = tomllib.loads(new_text)
    except tomllib.TOMLDecodeError as e:
        raise RuntimeError(f"daemon.toml: после записи получился невалидный TOML: {e}") from e
    if parsed.get("paths", []) != expected:
        raise RuntimeError("daemon.toml: список баз после записи не совпал с ожидаемым — запись отменена")
    for key in set(old) | set(parsed):
        if key != "paths" and old.get(key) != parsed.get(key):
            raise RuntimeError(f"daemon.toml: секция «{key}» изменилась бы — запись отменена")

    tmp = CONFIG + ".tmp"
    try:
        mode = os.stat(CONFIG).st_mode & 0o777
    except OSError:
        mode = 0o644
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(new_text)
    os.chmod(tmp, mode)
    os.replace(tmp, CONFIG)


def load_meta():
    try:
        with open(META, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_meta(meta):
    tmp = META + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=1)
    os.chmod(tmp, 0o600)
    os.replace(tmp, META)


def restart_serve(any_paths):
    """serve читает список баз при старте — после изменения его нужно перезапустить."""
    if not SERVE_UNIT:
        return None
    action = "restart" if any_paths else "stop"
    try:
        r = subprocess.run(["sudo", "-n", "systemctl", action, SERVE_UNIT],
                           capture_output=True, text=True, timeout=60)
    except Exception as e:  # noqa: BLE001 — причина показывается в панели
        return f"serve не перезапущен: {e}"
    if r.returncode != 0:
        detail = (r.stderr or r.stdout).strip() or f"код {r.returncode}"
        return f"serve не перезапущен: {detail}"
    return None


def add_path(alias, path, git):
    with lock:
        old_paths = config_paths()
        existing = next((p for p in old_paths if effective_alias(p) == alias), None)
        paths = [p for p in old_paths if effective_alias(p) != alias]
        entry = dict(existing) if existing else {}
        entry.update({"path": path, "alias": alias})
        if git:
            entry["git"] = True
        paths.append(entry)
        write_paths(paths)
    return restart_serve(True)


def remove_path(alias):
    with lock:
        paths = [p for p in config_paths() if effective_alias(p) != alias]
        write_paths(paths)
    return restart_serve(bool(paths))


def inside_repos(path):
    real = os.path.realpath(path)
    return real != REPOS and real.startswith(REPOS + os.sep)


def has_symlink_component(path):
    """Есть ли символьная ссылка на пути от REPOS до каталога базы."""
    cur = os.path.abspath(path)
    root = os.path.abspath(REPOS)
    while True:
        if os.path.islink(cur):
            return True
        if os.path.normcase(cur) == os.path.normcase(root):
            return False
        parent = os.path.dirname(cur)
        if parent == cur:
            return False
        cur = parent


def safe_repo_dir(path):
    """Каталог можно удалять: он внутри REPOS и не является ссылкой."""
    return inside_repos(path) and not has_symlink_component(path)


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
    try:
        r = subprocess.run(["git", "-C", path, "log", "-1", "--format=%h %cs %s"],
                           capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return None
    return r.stdout.strip() or None


def meminfo():
    data = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                key, value = line.split(":", 1)
                data[key] = int(value.split()[0]) * 1024
    except (OSError, ValueError):
        return {"total": 0, "available": 0}
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
    by_path = {
        os.path.realpath(str(p.get("path"))): p
        for p in health.get("paths", [])
        if p.get("path")
    }
    meta = load_meta()
    repos, known = [], set()
    for p in config_paths():
        alias = effective_alias(p)
        path = p.get("path")
        path_s = str(path) if path is not None else ""
        known.add(alias)
        src, idx, files = sizes(alias, path_s)
        m = meta.get(alias, {})
        h = by_path.get(os.path.realpath(path_s), {}) if path_s else {}
        repos.append({
            "alias": alias, "path": path_s, "git": bool(path_s) and os.path.isdir(os.path.join(path_s, ".git")),
            "status": h.get("status", "unknown"), "health": h,
            "src_bytes": src, "index_bytes": idx, "files": files,
            "source": m.get("source", "folder"), "url": mask_url(m.get("url")),
            "branch": m.get("branch"), "head": git_head(path_s) if path_s else None,
            "job": jobs.get(alias),
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
    started = now()
    with lock:
        jobs[alias] = {"state": "running", "message": message, "started": started}

    def wrapper():
        try:
            fn()
            with lock:
                jobs.pop(alias, None)
                size_cache.pop(alias, None)
        except Exception as e:  # noqa: BLE001
            with lock:
                jobs[alias] = {"state": "error", "message": str(e)[-2000:], "started": started}
    threading.Thread(target=wrapper, daemon=True).start()


def git_env():
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    # ssh не должен ждать подтверждения ключа хоста в фоновом задании.
    env.setdefault("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
    return env


def do_clone(alias, url, branch):
    dest = os.path.join(REPOS, alias)
    cmd = ["git", "clone", "--quiet"] + (["--branch", branch] if branch else []) + [url, dest]
    r = subprocess.run(cmd, capture_output=True, text=True, env=git_env(), timeout=6 * 3600)
    if r.returncode != 0:
        shutil.rmtree(dest, ignore_errors=True)
        raise RuntimeError(mask_url(r.stderr.strip()) or "git clone завершился с ошибкой")
    warning = add_path(alias, dest, True)
    if warning:
        raise RuntimeError(warning)


def do_pull(path):
    r = subprocess.run(["git", "-C", path, "pull", "--ff-only", "--quiet"],
                       capture_output=True, text=True, env=git_env(), timeout=3600)
    if r.returncode != 0:
        raise RuntimeError(mask_url(r.stderr.strip()) or "git pull завершился с ошибкой")


def check_zip_limits(z, free):
    """Потолки архива: число файлов и суммарный размер после распаковки."""
    infos = z.infolist()
    if len(infos) > MAX_ZIP_ENTRIES:
        raise RuntimeError(f"в архиве больше {MAX_ZIP_ENTRIES} файлов")
    total = sum(max(0, info.file_size) for info in infos)
    if total > max(0, free - ZIP_RESERVE_BYTES):
        raise RuntimeError(
            f"архив распакуется на {total} байт, свободно {free} — недостаточно места")
    return total


def do_unzip(alias, zpath):
    dest = os.path.join(REPOS, alias)
    try:
        with zipfile.ZipFile(zpath) as z:
            check_zip_limits(z, shutil.disk_usage(REPOS).free)
            root = os.path.realpath(dest)
            for info in z.infolist():
                target = os.path.realpath(os.path.join(dest, info.filename))
                if target != root and not target.startswith(root + os.sep):
                    raise RuntimeError(f"недопустимый путь в архиве: {info.filename}")
            z.extractall(dest)
        # Архив с одной корневой папкой (частый случай) — поднимаем её содержимое на уровень выше.
        shutil.rmtree(os.path.join(dest, "__MACOSX"), ignore_errors=True)
        entries = os.listdir(dest)
        if len(entries) == 1 and os.path.isdir(os.path.join(dest, entries[0])):
            inner = os.path.join(dest, entries[0])
            for e in os.listdir(inner):
                target = os.path.join(dest, e)
                if os.path.exists(target):
                    raise RuntimeError(f"в архиве конфликт путей: {e}")
                shutil.move(os.path.join(inner, e), target)
            os.rmdir(inner)
    except Exception:
        shutil.rmtree(dest, ignore_errors=True)
        raise
    finally:
        try:
            os.remove(zpath)
        except OSError:
            pass
    warning = add_path(alias, dest, False)
    if warning:
        raise RuntimeError(warning)


def do_delete(alias, path):
    warning = remove_path(alias)
    time.sleep(2)  # демону нужно время, чтобы отпустить индекс
    if safe_repo_dir(path):
        try:
            shutil.rmtree(os.path.realpath(path))
        except OSError as e:
            raise RuntimeError(f"каталог не удалён: {e}")
    elif inside_repos(path):
        raise RuntimeError("каталог базы — символьная ссылка; удаление отменено")
    meta = load_meta()
    meta.pop(alias, None)
    save_meta(meta)
    if warning:
        raise RuntimeError(warning)


# ---------- HTTP ----------

class Handler(BaseHTTPRequestHandler):
    server_version = "code-index-admin"

    def log_message(self, fmt, *args):
        pass

    def security_headers(self):
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header(
            "Content-Security-Policy",
            "default-src 'self'; style-src 'self' 'unsafe-inline'; "
            "script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
        )

    def send_json(self, data, code=200):
        body = json.dumps(data, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.security_headers()
        self.end_headers()
        self.wfile.write(body)

    def fail(self, message, code=400):
        self.send_json({"error": message}, code)

    def read_json(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n < 0 or n > MAX_JSON_BODY:
            raise ValueError("слишком большое тело запроса")
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        u = urlparse(self.path)
        if u.path in ("/", "/index.html"):
            with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "index.html"), "rb") as f:
                body = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.security_headers()
            self.end_headers()
            self.wfile.write(body)
        elif u.path == "/api/overview":
            self.send_json(overview())
        elif u.path == "/api/log":
            try:
                lines = int(parse_qs(u.query).get("lines", ["200"])[0])
            except (TypeError, ValueError):
                lines = 200
            lines = max(1, min(lines, 2000))
            try:
                r = subprocess.run(["tail", "-n", str(lines), LOG], capture_output=True, text=True)
            except OSError as e:
                return self.fail(f"журнал недоступен: {e}", 500)
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
        with lock:
            if any(effective_alias(p) == alias for p in config_paths()) or alias in jobs:
                return f"База «{alias}» уже есть"
        return None

    def do_POST(self):
        if not self.mutating_allowed():
            return
        u = urlparse(self.path)
        try:
            data = self.read_json()
        except (ValueError, TypeError):
            return self.fail("некорректный JSON")
        if not isinstance(data, dict):
            return self.fail("ожидается JSON-объект")
        alias = str(data.get("alias") or "").strip()
        meta = load_meta()

        if u.path == "/api/repos/git":
            url = str(data.get("url") or "").strip()
            branch = str(data.get("branch") or "").strip() or None
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
            if not os.path.isdir(path) or os.path.islink(path):
                return self.fail(f"Каталог {path} не найден или является ссылкой — скопируйте туда исходники")
            meta[alias] = {"source": "folder", "added": now()}
            save_meta(meta)
            try:
                warning = add_path(alias, path, os.path.isdir(os.path.join(path, ".git")))
            except Exception as e:  # noqa: BLE001 — ошибку показываем в панели
                meta.pop(alias, None)
                save_meta(meta)
                return self.fail(str(e), 500)
            resp = {"ok": True}
            if warning:
                resp["warning"] = warning
            return self.send_json(resp)

        repo = next((p for p in config_paths() if effective_alias(p) == alias), None)
        if u.path == "/api/repos/pull":
            if not repo or not os.path.isdir(os.path.join(repo["path"], ".git")):
                return self.fail("Это не git-база")
            run_job(alias, lambda: do_pull(repo["path"]), "git pull…")
            return self.send_json({"ok": True})

        if u.path == "/api/repos/delete":
            if repo:
                run_job(alias, lambda: do_delete(alias, repo["path"]), "удаление…")
            elif alias in jobs and jobs[alias]["state"] == "error":
                target = os.path.join(REPOS, alias)
                if not safe_repo_dir(target):
                    return self.fail("каталог ошибки вне REPOS или является ссылкой; удаление отменено")
                with lock:
                    jobs.pop(alias, None)
                shutil.rmtree(target, ignore_errors=True)
            else:
                return self.fail("База не найдена или операция ещё идёт", 404)
            return self.send_json({"ok": True})

        if u.path == "/api/jobs/clear":
            with lock:
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
        try:
            n = int(self.headers.get("Content-Length") or 0)
        except (TypeError, ValueError):
            return self.fail("некорректный Content-Length")
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
            try:
                os.remove(zpath)
            except OSError:
                pass
            return self.fail("загрузка прервана" if left else "это не ZIP-архив")
        meta = load_meta()
        meta[alias] = {"source": "zip", "added": now()}
        save_meta(meta)
        run_job(alias, lambda: do_unzip(alias, zpath), "распаковка…")
        self.send_json({"ok": True})


def main():
    if not HOME or not os.path.isfile(CONFIG):
        raise SystemExit("Задайте CODE_INDEX_HOME — каталог с daemon.toml")
    if not REPOS or REPOS == os.path.abspath(os.sep):
        raise SystemExit("ADMIN_REPOS_ROOT не должен быть корнем файловой системы")
    os.makedirs(REPOS, exist_ok=True)
    shutil.rmtree(TMP, ignore_errors=True)  # недокачанные архивы прошлого запуска
    print(f"code-index admin: http://{LISTEN[0]}:{LISTEN[1]}  (базы в {REPOS})", flush=True)
    ThreadingHTTPServer(LISTEN, Handler).serve_forever()


if __name__ == "__main__":
    main()
