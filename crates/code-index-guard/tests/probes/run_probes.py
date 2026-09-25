"""Прогон набора проб через code-index-guard: решение хука на реальных вызовах.

    python tests/probes/run_probes.py [путь-к-exe] [путь-к-code-index-guard.toml]

Зачем прибор. Гард отказывает в поиске, и цена ошибки платится в обе стороны: лишний отказ ломает
законную работу (фильтр вывода через трубу, git-история, дотфайлы, исключённые пути), а пропуск
возвращает обход правила. На глаз это не проверяется.

Набор различений и устройство прибора — от Романа (комплект от 27.08.2026); пути и часть случаев
переписаны под эту машину. Ценность набора не в путях, а в РАЗЛИЧЕНИЯХ, которые гард обязан делать.

Поля пробы: n, want, tool, cwd и cmd либо input — обязательные. Необязательные: args (список
дополнительных ключей запуска exe, напр. --mcp-prefix) и contains (подстрока, обязанная быть в stdout
при сошедшемся вердикте: так проверяется ТЕКСТ отказа, а не только решение).
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROBES = HERE / "probes.json"
PATHS = Path(os.environ.get("GUARD_PROBE_PATHS") or HERE / "paths.json")

# Плейсхолдеры: те, что ДОЛЖНЫ быть в daemon.toml, и те, которых там быть НЕ должно.
IN_DAEMON = ("REPO1", "REPO2", "REPO3")
NOT_IN_DAEMON = ("NOINDEX", "NOREPO")

# Что именно должно лежать в REPO1: набор различает индексированное и неиндексированное
# на конкретных файлах, и без них половина ожиданий недостижима. Годится любой Rust-репозиторий
# из daemon.toml, собранный хотя бы раз (нужен target/release).
REPO1_REQUIRED = (
    ("Cargo.toml", "file", "индексированный файл: пробы Read/Get-Content останутся без цели"),
    ("Cargo.lock", "file", "НЕиндексированный файл рядом: не проверить «пропуск» на неиндексированном"),
    (".gitignore", "file", "дотфайл: пробы «grep по дотфайлу» и «Grep-инструмент по дотфайлу» недостижимы"),
    ("src/main.rs", "file", "исходник в подкаталоге: пробы grep/Select-String по src/ недостижимы"),
    ("target/release", "dir", "исключённый из индекса каталог: пробы про target/ недостижимы"),
)


def unescape_toml(s: str) -> str:
    """Разэкранирование строки TOML: удвоенный слеш становится одинарным."""
    out, i = [], 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s) and s[i + 1] in ("\\", '"', "/"):
            out.append(s[i + 1])
            i += 2
            continue
        out.append(s[i])
        i += 1
    return "".join(out)


def daemon_prefixes(daemon_toml: Path) -> list[str] | None:
    """Нормализованные пути репозиториев из daemon.toml; None — файл не прочитан."""
    try:
        text = daemon_toml.read_text(encoding="utf-8")
    except OSError:
        return None
    found = re.findall(r'^\s*path\s*=\s*"((?:[^"\\]|\\.)*)"', text, re.MULTILINE)
    return [unescape_toml(p).replace("\\", "/").rstrip("/").lower() for p in found]


def check_preconditions(paths: dict[str, str], daemon_toml: Path | None) -> list[str]:
    """Условия, при которых ожидания набора вообще осмысленны."""
    bad: list[str] = []
    for k in IN_DAEMON + NOT_IN_DAEMON:
        if not paths.get(k):
            bad.append(f"{k}: не задан в {PATHS.name}")
    if bad:
        return bad

    for k in IN_DAEMON:
        if not Path(paths[k]).is_dir():
            bad.append(f"{k} = {paths[k]}: каталога нет на диске")
    # ⚠️ Часть проб адресует КОНКРЕТНЫЕ файлы внутри REPO1 (индексированный исходник,
    # неиндексированный lock-файл, дотфайл, исключённый каталог сборки). Нет их — ожидания
    # недостижимы, и прогон покажет расхождения, которых в поведении гарда нет. Называем
    # вслух, чего не хватает: молчаливое «X» уводит разбор в сторону (случай 10.09.2026,
    # где отсутствие .gitignore в REPO1 дало два ложных расхождения).
    for rel, kind, why in REPO1_REQUIRED:
        target = Path(paths["REPO1"]) / rel
        ok = target.is_dir() if kind == "dir" else target.is_file()
        if not ok:
            bad.append(f'REPO1 = {paths["REPO1"]}: нет {kind} "{rel}" — {why}')

    if not Path(paths["NOINDEX"]).is_dir():
        bad.append(f'NOINDEX = {paths["NOINDEX"]}: каталог должен СУЩЕСТВОВАТЬ, но не быть в daemon.toml')
    if Path(paths["NOREPO"]).is_dir():
        bad.append(f'NOREPO = {paths["NOREPO"]}: каталога быть НЕ должно, а он есть')

    if daemon_toml is None:
        return bad
    prefixes = daemon_prefixes(daemon_toml)
    if prefixes is None:
        bad.append(f"daemon.toml не прочитан: {daemon_toml}")
        return bad
    for k in IN_DAEMON:
        if paths[k].replace("\\", "/").rstrip("/").lower() not in prefixes:
            bad.append(f"{k} = {paths[k]}: нет в daemon.toml — ожидание ОТКАЗ недостижимо")
    # Проба «cd .. выводит из репо» осмысленна, только если РОДИТЕЛЬ REPO1 не индексирован:
    # иначе после `cd ..` цель остаётся под индексированным префиксом и пропуска не будет.
    parent = os.path.dirname(os.path.normpath(paths["REPO1"])).replace("\\", "/").lower()
    if any(parent == p or parent.startswith(p + "/") for p in prefixes):
        bad.append(
            f'родитель REPO1 = {paths["REPO1"]} индексирован — '
            f'проба "cd .. выводит из репо" недостижима'
        )
    for k in NOT_IN_DAEMON:
        if paths[k].replace("\\", "/").rstrip("/").lower() in prefixes:
            bad.append(f"{k} = {paths[k]}: ЕСТЬ в daemon.toml — ожидание «пропуск» недостижимо")
    return bad


def substitute(probe: dict, paths: dict[str, str]) -> dict:
    """Плейсхолдеры → пути: в cwd нативной формой Windows, внутри команд — как записаны."""
    out = json.loads(json.dumps(probe))
    for k, v in paths.items():
        if k.startswith("_"):
            continue
        token = "{" + k + "}"
        out["cwd"] = out["cwd"].replace(token, os.path.normpath(v))
        if "cmd" in out:
            out["cmd"] = out["cmd"].replace(token, v)
        if "input" in out:
            out["input"] = json.loads(json.dumps(out["input"]).replace(token, v))
    return out


def check_list_roots(exe: Path, env: dict[str, str], paths: dict[str, str]) -> list[str]:
    """Режим --list-roots: впрыск списка корней в контекст сессии. Возвращает список претензий."""
    bad: list[str] = []

    r = subprocess.run([str(exe), "--list-roots"], input=b"", capture_output=True, env=env)
    out = r.stdout.decode("utf-8", "replace").strip()
    if r.returncode != 0:
        bad.append(f"--list-roots: код возврата {r.returncode}, ждали 0")
    try:
        data = json.loads(out)
    except json.JSONDecodeError as e:
        bad.append(f"--list-roots: вывод не разбирается как JSON ({e}); получено: {out[:200]!r}")
        return bad
    hs = data.get("hookSpecificOutput") or {}
    if hs.get("hookEventName") != "SessionStart":
        bad.append(f'--list-roots: hookEventName = {hs.get("hookEventName")!r}, ждали "SessionStart"')
    ctx = hs.get("additionalContext") or ""
    if not ctx.strip():
        bad.append("--list-roots: additionalContext пуст")
    known = paths["REPO1"].replace("\\", "/").rstrip("/").lower()
    if known not in ctx.lower():
        bad.append(f"--list-roots: в списке нет известного пути {known}")

    # Конфига нет → пустой stdout и код 0 (пустой контекст лучше ложного).
    env_no_cfg = dict(env)
    env_no_cfg["CODE_INDEX_GUARD_CONFIG"] = str(HERE / "net-takogo-konfiga.toml")
    env_no_cfg.pop("CODE_INDEX_DAEMON_TOML", None)  # иначе daemon.toml возьмётся мимо конфига
    r2 = subprocess.run([str(exe), "--list-roots"], input=b"", capture_output=True, env=env_no_cfg)
    if r2.returncode != 0:
        bad.append(f"--list-roots без конфига: код возврата {r2.returncode}, ждали 0")
    if r2.stdout.decode("utf-8", "replace").strip():
        bad.append("--list-roots без конфига: stdout не пуст")
    return bad


def main(argv: list[str]) -> int:
    exe = Path(argv[1]) if len(argv) > 1 else Path("target/release/code-index-guard.exe")
    if not exe.exists():
        print(f"не найден бинарь: {exe}")
        return 2

    # ⚠️ БЕЗ КОНФИГА ГАРД НЕ ПЕРЕХВАТЫВАЕТ НИЧЕГО, и прогон покажет ложное «почти всё сошлось» —
    # это отсутствие настройки, а не свойство гарда. Условие называем вслух.
    env = dict(os.environ)
    if len(argv) > 2:
        env["CODE_INDEX_GUARD_CONFIG"] = str(Path(argv[2]))
    cfg = Path(env.get("CODE_INDEX_GUARD_CONFIG") or str(exe.parent / "code-index-guard.toml"))
    if not cfg.exists():
        print(f"⚠️ конфига нет: {cfg}")
        print("   Гард без него молчит на всём. Укажите его вторым аргументом либо положите "
              "code-index-guard.toml рядом с exe (образец — code-index-guard.toml.example).")
        return 2
    print(f"конфиг: {cfg}")

    if not PATHS.exists():
        print(f"⚠️ нет файла путей: {PATHS}")
        print("   Скопируйте paths.example.json в paths.json и впишите пути своей машины.")
        return 2
    paths = json.loads(PATHS.read_text(encoding="utf-8"))
    m = re.search(r'^\s*daemon_toml\s*=\s*"((?:[^"\\]|\\.)*)"',
                  cfg.read_text(encoding="utf-8"), re.MULTILINE)
    daemon_toml = Path(unescape_toml(m.group(1))) if m else None

    bad = check_preconditions(paths, daemon_toml)
    if bad:
        print(f"⚠️ окружение не отвечает набору — поправьте {PATHS.name}:")
        for b in bad:
            print(f"   • {b}")
        print("   Ожидания (want) без этих условий недостижимы, и счёт ничего не докажет.")
        return 2

    # База событий хука: пробы не должны писать в боевую. Гард идёт с временной копией
    # конфига без ключа events_db (без него события не записываются); копию удаляем на выходе.
    fd, cfg_copy_name = tempfile.mkstemp(prefix="code-index-guard-probes-", suffix=".toml")
    os.close(fd)
    cfg_copy = Path(cfg_copy_name)
    cfg_copy.write_text(
        re.sub(r"(?m)^\s*events_db\s*=.*$", "", cfg.read_text(encoding="utf-8")), encoding="utf-8"
    )
    env["CODE_INDEX_GUARD_CONFIG"] = str(cfg_copy)

    probes = json.loads(PROBES.read_text(encoding="utf-8"))
    ok = 0
    for raw in probes:
        p = substitute(raw, paths)
        ti = p.get("input") or {"command": p["cmd"]}
        payload = json.dumps({"cwd": p["cwd"], "tool_name": p["tool"], "tool_input": ti})
        # args — необязательные ключи командной строки пробы (напр. --mcp-prefix у Codex).
        r = subprocess.run(
            [str(exe), *p.get("args", [])], input=payload.encode("utf-8"), capture_output=True, env=env
        )
        out = r.stdout.decode("utf-8", "replace")
        if '"deny"' in out:
            verdict = "ОТКАЗ"
        elif '"allow"' in out:
            verdict = "РАЗРЕШЕНО"
        elif not out.strip():
            verdict = "пропуск"
        else:
            verdict = "ДРУГОЕ"
        if verdict != p["want"]:
            print(f'X {p["n"]:<48} ждали {p["want"]:<10} получили {verdict}')
            continue
        # contains — подстрока, обязанная быть в выводе при СОШЕДШЕМСЯ вердикте: так
        # проверяется текст отказа (напр. префикс имён инструментов), а не только решение.
        need = p.get("contains")
        if need and need not in out:
            print(f'X {p["n"]:<48} вердикт {verdict} сошёлся, но в выводе нет {need!r}')
            continue
        ok += 1
    print(f"--- сошлось {ok} из {len(probes)} ---")

    roots_bad = check_list_roots(exe, env, paths)
    for b in roots_bad:
        print(f"X {b}")
    print(f"--- режим --list-roots: {'сошёлся' if not roots_bad else 'НЕ сошёлся'} ---")

    cfg_copy.unlink(missing_ok=True)

    return 0 if ok == len(probes) and not roots_bad else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
