//! PreToolUse-хук Claude Code: indexed-path guard (нативный, Rust).
//!
//! Перенаправляет нативные Read/Grep/Glob/Bash по индексированным каталогам
//! code-index в mcp__code-index__*. В отличие от старого indexed_path_guard.py,
//! блокирует ТОЛЬКО доказуемо-отдаваемые через MCP файлы — без дед-эндов на
//! неиндексированных (бинари, файлы без расширения, исключённые каталоги,
//! oversize, свежие правки).
//!
//! Контракт (язык-агностичный):
//!   stdin  — JSON {tool_name, tool_input, cwd}
//!   stdout — JSON {"hookSpecificOutput": {"hookEventName":"PreToolUse",
//!                  "permissionDecision":"deny"|"allow", "permissionDecisionReason":?}}
//!            либо НИЧЕГО (воздержаться → решает следующий хук / штатный flow).
//!   exit   — всегда 0 (решение передаётся через stdout). Fail-safe: при любой
//!            ошибке — воздержаться (ничего в stdout).
//!
//! Источники истины об индексированных путях (в бинарнике путей нет — всё из
//! конфига хука code-index-guard.toml рядом с exe / env CODE_INDEX_GUARD_CONFIG),
//! два необязательных раздела:
//!   1. daemon_toml — путь к daemon.toml индексатора; из него берутся репо
//!      (env CODE_INDEX_DAEMON_TOML переопределяет). Может отсутствовать.
//!   2. [[local]] — репо, добавленные вручную (path + alias). Может отсутствовать.
//!
//! Ключ events_db включает запись решений хука в базу событий SQLite;
//! без него события не записываются.
//!
//! Итоговый список баз — сумма обоих с дедупликацией по пути: репо из daemon.toml
//! приоритетны, [[local]] добавляет лишь те пути, которых в daemon.toml нет.
//! Пусто в обоих → индексированных путей нет, хук ничего не перехватывает.
//!
//! Проверка членства:
//!   - локальный <repo>/.code-index/index.db есть → точный SQL по files +
//!     text_files/file_contents + проверка свежести (mtime файла на диске >
//!     mtime в индексе → индекс отстал, воздержаться, пустить нативный Read);
//!   - нет index.db → эвристика по диску (расширение/размер/mtime/exclude).

mod journal;

use std::io::Read as _;

use rusqlite::OptionalExtension;
use serde_json::Value;

// Список неиндексируемых каталогов переехал в `excluded_from_index`: там он сравнивается ПО
// СЕГМЕНТАМ пути и дополняется исключениями самого проекта из `.code-index/config.json`.
// Прежняя проверка подстрокой (`"/.venv/"`) требовала ведущего слеша и не срабатывала на
// относительных путях вида `.venv/Lib`, которые агент пишет чаще всего.

/// Бинарные / не-индексируемые расширения. Их нет в `files`; в Bash/Glob —
/// маркер «трогаем не индексированное» → воздержаться.
const BINARY_EXT: &[&str] = &[
    ".epf", ".erf", ".cf", ".cfe", ".png", ".jpg", ".jpeg", ".gif", ".ico", ".bmp", ".zip", ".7z",
    ".gz", ".rar", ".exe", ".dll", ".bin", ".pdf", ".mxl", ".grs",
];

/// Bash-утилиты ПОИСКА/СПИСКА по индексированному пути (repo-wide redirect в
/// grep_code/list_files — без пер-файлового дед-энда). Одиночные ридеры
/// (cat/head/tail/wc/file/stat) НЕ включены: их редирект в read_file требует
/// пер-файловой проверки членства, иначе дед-энд на неиндексированном файле.
const BASH_READ_UTILS: &[&str] = &["grep ", "rg ", "find ", "ls "];

/// Командлеты PowerShell того же назначения, что BASH_READ_UTILS, вместе с алиасами:
/// Select-String (sls), Get-ChildItem (gci/ls/dir), findstr. Пер-файловые ридеры
/// (Get-Content/gc/cat/type) сюда НЕ входят — они в decide_ps_file_read.
const PS_READ_UTILS: &[&str] = &[
    "select-string ",
    "sls ",
    "get-childitem ",
    "gci ",
    "ls ",
    "dir ",
    "findstr ",
];

enum Decision {
    Deny(String),
    Allow,
}

/// Префикс имён инструментов code-index в этом клиенте (`--mcp-prefix`). У Codex они
/// называются `mcp__code_index__read_file`, у Claude Code — `read_file`. Файл
/// code-index-guard.toml общий для обоих клиентов, поэтому префикс задаётся ключом
/// командной строки в СВОЕЙ строке запуска. Ключа нет → пустая строка.
static MCP_PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Имя инструмента code-index так, как его видит модель в этой сессии.
fn tool(name: &str) -> String {
    format!(
        "{}{}",
        MCP_PREFIX.get().map(String::as_str).unwrap_or(""),
        name
    )
}

fn main() {
    let started = std::time::Instant::now();
    // Ключи командной строки в ЛЮБОМ порядке: --list-roots (режим SessionStart-хука) и
    // --mcp-prefix <префикс> (как названы инструменты code-index у этого клиента).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut list_roots = false;
    let mut prefix = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list-roots" => list_roots = true,
            "--mcp-prefix" => {
                i += 1;
                prefix = args.get(i).cloned().unwrap_or_default();
            }
            _ => {}
        }
        i += 1;
    }
    let _ = MCP_PREFIX.set(prefix);
    // Режим --list-roots: печать списка индексированных корней для SessionStart-хука.
    // stdin не читаем, решений PreToolUse не принимаем.
    if list_roots {
        emit_roots(started);
        return;
    }
    // Любая ошибка/паника внутри run() → воздержаться (exit 0, пустой stdout).
    let _ = run(started);
}

/// Печатает список индексированных корней как additionalContext SessionStart-хука:
/// модель видит границы индекса ДО первого вызова и не теряет ход на deny.
///
/// ⚠️ Список обязан приходить из той же `indexed_paths()`, что решает про deny.
/// Второй источник (get_stats у serve) отдаёт и федеративные репо, которых хук
/// намеренно не сторожит, — снимок разошёлся бы с поведением.
/// Список пуст (нет конфига, нет daemon.toml) → пустой stdout: пустой контекст лучше ложного.
fn emit_roots(started: std::time::Instant) {
    let paths = indexed_paths();
    // Журнал событий: одна запись на запуск — этот режим отдаёт контекст SessionStart.
    // event=SessionStart задаём входом-hook_event_name: своего stdin у режима нет.
    let session_input = serde_json::json!({"hook_event_name": "SessionStart"});
    journal::record(
        events_db_path(&load_guard_config()).as_deref(),
        journal::Event {
            hook: "code-index-guard",
            decision: "inject",
            input: Some(&session_input),
            target: None,
            reason: None,
            details: Some(serde_json::json!({"roots": paths.len()})),
            started: Some(started),
        },
    );
    if paths.is_empty() {
        return;
    }
    // ⚠️ Перечисляем ДЕЙСТВИЯ, а не имена инструментов. «PowerShell заблокирован» соврало бы:
    // перехвачены только чтение/поиск/обход каталога, а запуск, сборка, git, копирование идут
    // свободно. Список действий заодно не устаревает при добавлении очередного инструмента.
    let mut text = format!(
        "Каталоги в индексе code-index. В них хук отклоняет ТОЛЬКО чтение файлов, поиск по \
         содержимому и обход каталогов нативными средствами: Read, Grep, Glob; Bash cat/head/tail/\
         grep/rg/find/ls; PowerShell Get-Content/Select-String/Get-ChildItem/findstr. \
         Вместо них сразу бери инструменты code-index (repo = алиас справа): {}, {}, \
         {}, {}, {}. Всё прочее по этим путям не затронуто — запуск, сборка, \
         git, копирование, правка файлов (Edit/Write). Путей, которых нет в списке, запрет не касается.\n\n",
        tool("read_file"),
        tool("grep_code"),
        tool("grep_text"),
        tool("list_files"),
        tool("get_function"),
    );
    for (path, alias) in &paths {
        text.push_str(path);
        if !alias.is_empty() {
            text.push_str(" → ");
            text.push_str(alias);
        }
        text.push('\n');
    }
    let mut hs = serde_json::Map::new();
    hs.insert("hookEventName".into(), Value::String("SessionStart".into()));
    hs.insert("additionalContext".into(), Value::String(text));
    let mut root = serde_json::Map::new();
    root.insert("hookSpecificOutput".into(), Value::Object(hs));
    println!("{}", Value::Object(root));
}

fn run(started: std::time::Instant) -> Option<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let payload: Value = serde_json::from_str(&input).ok()?;
    let tool = payload.get("tool_name")?.as_str()?;
    let ti = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = payload.get("cwd").and_then(|v| v.as_str());

    let decision = match tool {
        "Read" => decide_read(&ti),
        "Grep" => decide_grep(&ti, cwd),
        "Glob" => decide_glob(&ti, cwd),
        // Codex CLI на Windows присылает команды PowerShell под именем «Bash»: если
        // разбор как Bash решения не дал, та же команда разбирается как PowerShell.
        // Тела heredoc'ов — данные, а не команды: вырезаются до разбора.
        "Bash" => decide_bash(&ti, cwd).or_else(|| {
            let cmd = ti.get("command").and_then(Value::as_str).unwrap_or("");
            decide_powershell(&serde_json::json!({ "command": strip_heredocs(cmd) }), cwd)
        }),
        "PowerShell" => decide_powershell(&ti, cwd),
        _ => None,
    };

    // Журнал событий: одно решение на запуск — то же, что уходит в emit(),
    // а воздержание (пустой stdout) записывается как skip.
    let target = target_text(tool, &ti);
    journal::record(
        events_db_path(&load_guard_config()).as_deref(),
        journal::Event {
            hook: "code-index-guard",
            decision: match &decision {
                Some(Decision::Deny(_)) => "deny",
                Some(Decision::Allow) => "allow",
                None => "skip",
            },
            input: Some(&payload),
            target: Some(target.as_str()),
            reason: match &decision {
                Some(Decision::Deny(r)) => Some(r.as_str()),
                _ => None,
            },
            details: target_repo(tool, &ti, cwd).map(|alias| serde_json::json!({"repo": alias})),
            started: Some(started),
        },
    );

    if let Some(d) = decision {
        emit(d);
    }
    Some(())
}

/// Цель решения для журнала: путь файла (Read), образец и путь (Grep/Glob),
/// первые 200 символов команды (Bash/PowerShell).
fn target_text(tool: &str, ti: &Value) -> String {
    let field = |k: &str| ti.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Read" => field("file_path").to_string(),
        "Grep" | "Glob" => [field("pattern"), field("path")]
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        "Bash" | "PowerShell" => field("command").chars().take(200).collect(),
        _ => String::new(),
    }
}

/// Алиас индексированного репозитория по цели решения — если он в вызове есть.
fn target_repo(tool: &str, ti: &Value, cwd: Option<&str>) -> Option<String> {
    let paths = indexed_paths();
    if paths.is_empty() {
        return None;
    }
    let field = |k: &str| ti.get(k).and_then(Value::as_str).unwrap_or("");
    let candidates: [&str; 3] = match tool {
        "Read" => [field("file_path"), "", ""],
        "Grep" => [field("path"), cwd.unwrap_or(""), ""],
        "Glob" => [field("path"), field("pattern"), cwd.unwrap_or("")],
        _ => ["", "", ""],
    };
    if let Some((alias, _, _)) = candidates.iter().find_map(|c| match_indexed(c, &paths)) {
        return Some(alias);
    }
    // Bash/PowerShell: путь внутри команды, поэтому ищем индексированный корень в ней
    let norm_lower = normalize_msys_drives(&field("command").replace('\\', "/")).to_lowercase();
    let mut sorted: Vec<&(String, String)> = paths.iter().collect();
    sorted.sort_by_key(|запись| std::cmp::Reverse(запись.0.len()));
    sorted
        .into_iter()
        .find(|(prefix, _)| norm_lower.contains(prefix.as_str()))
        .map(|(_, alias)| alias.clone())
}

fn emit(d: Decision) {
    let (decision, reason) = match d {
        Decision::Deny(r) => ("deny", Some(r)),
        Decision::Allow => ("allow", None),
    };
    let mut hs = serde_json::Map::new();
    hs.insert("hookEventName".into(), Value::String("PreToolUse".into()));
    hs.insert("permissionDecision".into(), Value::String(decision.into()));
    if let Some(r) = reason {
        hs.insert("permissionDecisionReason".into(), Value::String(r));
    }
    let mut root = serde_json::Map::new();
    root.insert("hookSpecificOutput".into(), Value::Object(hs));
    println!("{}", Value::Object(root));
}

// ---------------------------------------------------------------------------
// Read — основная ветка с точной проверкой членства
// ---------------------------------------------------------------------------

fn decide_read(ti: &Value) -> Option<Decision> {
    let fp = ti.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    if fp.is_empty() {
        return None;
    }
    let paths = indexed_paths();
    let (alias, rel, root) = match_indexed(fp, &paths)?;

    // Ритуальный Read РОВНО одной первой строки для снятия гейта Edit — пропускаем.
    // Любое другое чтение (даже 2-5 строк, и строка из середины файла) идёт через
    // индекс/кеш. Гейту Edit достаточно Read(offset=1, limit=1).
    let limit = ti.get("limit").and_then(|v| v.as_i64());
    let offset = ti.get("offset").and_then(|v| v.as_i64());
    if limit == Some(1) && matches!(offset, None | Some(0) | Some(1)) {
        return Some(Decision::Allow);
    }

    if !read_is_blockable(&root, &rel) {
        return None; // не доказано, что MCP отдаст → воздержаться (нативный Read)
    }

    let (read_file, get_function, get_file_summary) = (
        tool("read_file"),
        tool("get_function"),
        tool("get_file_summary"),
    );
    Some(Decision::Deny(format!(
        "Read запрещён на индексированном пути (repo='{alias}'). Файл в индексе — \
         MCP отдаёт его в 3-10× дешевле по токенам. Возьми инструменты code-index \
         (как назван сервер в этой сессии — видно по списку инструментов): \
         {read_file}(repo=\"{alias}\", path=\"{rel}\", line_start=N, line_end=M). \
         Функцию целиком: {get_function}(repo=\"{alias}\", function_name=\"...\"). \
         Карту файла: {get_file_summary}(repo=\"{alias}\", path=\"{rel}\"). \
         Для гейта Edit допустим Read(file_path=\"{fp}\", offset=1, limit=1)."
    )))
}

/// true → файл доказуемо отдаётся через MCP (можно блокировать нативный Read).
/// Блокируем ТОЛЬКО при наличии локального index.db — иначе (индекс на удалённой
/// ноде либо путь ещё не проиндексирован) проверить членство нельзя →
/// воздерживаемся. Это закрывает дед-энд на локальных-только файлах под
/// префиксом без локального индекса (напр. служебный файл, которого в индексе нет).
fn read_is_blockable(root: &str, rel: &str) -> bool {
    let db = format!("{root}/.code-index/index.db");
    if !std::path::Path::new(&db).exists() {
        return false;
    }
    let (serve, idx_mtime) = match serveability(&db, rel) {
        Some(x) => x,
        None => return false,
    };
    if !matches!(serve, Serve::Serveable) {
        return false;
    }
    // Проверка свежести. Если файл на диске новее записи в индексе — наблюдатель
    // ещё не догнал (окно ~debounce 1.5 с), MCP отдаст устаревшее содержимое.
    // Воздерживаемся: пусть нативный Read прочитает актуальный файл. mtime в
    // индексе хранится в unix-секундах (та же база, что у файла на диске).
    if let (Some(idx), Some(disk)) = (idx_mtime, file_mtime_secs(&format!("{root}/{rel}"))) {
        if disk > idx {
            return false;
        }
    }
    true
}

/// mtime файла на диске в unix-секундах — для сравнения с `files.mtime` индекса.
/// None → файла нет или он недоступен (тогда свежесть не проверяем).
fn file_mtime_secs(path: &str) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let secs = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(secs as i64)
}

enum Serve {
    /// Есть text_contents-строка ИЛИ file_contents с oversize=0 → read_file отдаст.
    Serveable,
    /// Нет строки в files, либо oversize=1, либо «голая» строка → MCP не отдаст.
    NotServeable,
}

/// Возвращает (отдаётся ли через MCP, mtime записи в индексе в unix-секундах).
/// mtime нужен вызывающему для проверки свежести (диск новее индекса → отстал).
fn serveability(db: &str, rel: &str) -> Option<(Serve, Option<i64>)> {
    let uri = format!("file:{db}?mode=ro&immutable=0");
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    // Схема индекса: v5 хранит текст в text_contents, v4 — в text_files.
    // Без выбора правильной таблицы запрос упал бы «no such table» на
    // немигрированной БД → .ok()?=None → guard молча перестал бы блокировать
    // нативный Read (симметрично исходному багу text_files→text_contents).
    let has_text_contents = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='text_contents'",
            [],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some();
    let text_subq = if has_text_contents {
        "(SELECT 1 FROM text_contents t WHERE t.file_id = f.id LIMIT 1)"
    } else {
        "(SELECT 1 FROM text_files t WHERE t.file_id = f.id LIMIT 1)"
    };
    let sql = format!(
        "SELECT {text_subq}, \
                (SELECT c.oversize FROM file_contents c WHERE c.file_id = f.id LIMIT 1), \
                f.mtime \
         FROM files f WHERE f.path = ?1 COLLATE NOCASE LIMIT 1"
    );
    let row = match conn
        .query_row(&sql, rusqlite::params![rel], |r| {
            let is_text: Option<i64> = r.get(0)?;
            let oversize: Option<i64> = r.get(1)?;
            let mtime: Option<i64> = r.get(2)?;
            Ok((is_text, oversize, mtime))
        })
        .optional()
    {
        Ok(row) => row,
        // Отличаем «строки нет» (легитимный None) от реальной SQL/схема-ошибки:
        // вторую логируем, чтобы рассинхрон схемы был виден сразу.
        Err(e) => {
            eprintln!("[code-index-guard] serveability query failed on {db}: {e}");
            return None;
        }
    };
    let serve = match row {
        None => Serve::NotServeable,               // не в индексе
        Some((Some(1), _, _)) => Serve::Serveable, // текстовый файл → read_file отдаст
        Some((_, Some(0), _)) => Serve::Serveable, // код с сохранённым контентом
        _ => Serve::NotServeable,                  // oversize=1 / голая строка / прочее
    };
    let idx_mtime = row.and_then(|(_, _, m)| m);
    Some((serve, idx_mtime))
}

// ---------------------------------------------------------------------------
// Grep / Glob — по всему репо. Bash — grep/rg/find/ls репо-широко +
// пер-файловое cat/head/tail (перенаправляется в read_file)
// ---------------------------------------------------------------------------

fn decide_grep(ti: &Value, cwd: Option<&str>) -> Option<Decision> {
    let paths = indexed_paths();
    let path = ti.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let glob = ti.get("glob").and_then(|v| v.as_str()).unwrap_or("");
    let m = if !path.is_empty() {
        let norm = path.replace('\\', "/");
        if !std::path::Path::new(&norm).exists() {
            return None; // явный путь не существует → не блокируем
        }
        match_indexed(path, &paths)
    } else {
        // path не задан → Grep идёт от cwd сессии
        match_indexed(cwd.unwrap_or(""), &paths)
    };
    let (alias, rel, root) = m?;
    // Исключения проекта смотрим И по пути, И по образцу: `Grep(glob=".env")` из корня репо
    // указывает на неиндексируемый файл ровно так же, как `Grep(path=".../.env")`.
    if excluded_from_index(&root, &rel) || (!glob.is_empty() && excluded_from_index(&root, glob)) {
        return None;
    }
    Some(Decision::Deny(format!(
        "Grep запрещён на индексированном пути (repo='{alias}'). Используй инструменты code-index: \
         {}(repo=\"{alias}\", regex=\"...\", language=\"...\") по коду или \
         {}(repo=\"{alias}\", regex=\"...\", path_glob=\"...\") по yaml/md/json.",
        tool("grep_code"),
        tool("grep_text")
    )))
}

fn decide_glob(ti: &Value, cwd: Option<&str>) -> Option<Decision> {
    let path = ti.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let pattern = ti.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    // Glob по бинарному расширению — list_files его не вернёт (не индексируется) → воздержаться.
    let pl = pattern.to_lowercase();
    if BINARY_EXT.iter().any(|e| pl.ends_with(e)) {
        return None;
    }
    let paths = indexed_paths();
    // ⚠️ cwd — кандидат ВСЕГДА, когда явного пути нет. Прежде он добавлялся лишь если пусты ОБА
    // поля, и самый частый вызов агента — `Glob(pattern="services/**/*.py")` из корня репо —
    // проходил мимо: относительный образец путём не является, а cwd не смотрели.
    let mut candidates: Vec<&str> = vec![path, pattern];
    if path.is_empty() {
        candidates.push(cwd.unwrap_or(""));
    }
    for c in candidates {
        if c.is_empty() {
            continue;
        }
        if let Some((alias, rel, root)) = match_indexed(c, &paths) {
            let target = if rel.is_empty() { pattern } else { &rel };
            if excluded_from_index(&root, target)
                || (!pattern.is_empty() && excluded_from_index(&root, pattern))
            {
                return None;
            }
            return Some(Decision::Deny(format!(
                "Glob запрещён на индексированном пути (repo='{alias}'). Используй инструмент \
                 code-index {}(repo=\"{alias}\", pattern=\"...\", language=\"...\").",
                tool("list_files")
            )));
        }
    }
    None
}

fn decide_bash(ti: &Value, cwd: Option<&str>) -> Option<Decision> {
    let cmd = ti.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if cmd.is_empty() {
        return None;
    }
    let lower = normalize_msys_drives(&cmd.replace('\\', "/").to_lowercase());

    // Пер-файловое чтение: cat / head / tail <файл> по индексированному файлу →
    // перенаправить в read_file (с той же проверкой serveable+свежесть, что у Read).
    if let Some(d) = decide_bash_file_read(cmd, &lower, cwd) {
        return Some(d);
    }

    // ⚠️ ПОИСКОВОЕ ЗВЕНО ИЩЕТСЯ ПО ВСЕЙ ЦЕПОЧКЕ, а не только в её начале. Самая частая форма у
    // агента — `cd <репо> && grep -rn "x" src/`: команда начинается с `cd`, и проверка
    // `starts_with` пропускала её целиком. Заодно цепочка даёт рабочий каталог: `cd` меняет базу
    // для последующих звеньев.
    //
    // Звено ПОСЛЕ одиночной трубы не смотрим вовсе: там фильтруется чужой вывод, а не файлы
    // (`git log | grep`, `pytest | grep`), и запрет на это ломает законную работу.
    let mut base: String = cwd.unwrap_or("").to_string();
    let home = home_dir();
    let mut link: Option<String> = None;
    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for part in split_chain(&strip_heredocs(&lower)) {
        let part = expand_vars(part.trim(), &vars);
        if part.is_empty() {
            continue;
        }
        // `root="/c/tmp"` — не команда, а значение для следующих звеньев.
        if let Some((name, value)) = parse_assignment(&part, false) {
            vars.insert(name, value);
            continue;
        }
        // Смена каталога: cd / pushd / set-location / sl / push-location / popd.
        // Неизвестный каталог (Some("")) обнуляет базу — дальше гард воздерживается,
        // а не судит по прежнему cwd.
        if let Some(b) = dir_change(&part, &base, home.as_deref()) {
            base = b;
            continue;
        }
        // git grep / git log -S — история и содержимое коммитов, индексом не покрыты.
        if part.starts_with("git ") {
            continue;
        }
        if BASH_READ_UTILS.iter().any(|u| part.starts_with(u)) {
            // `ls` перехватывается в любом виде — список каталога заменяет list_files.
            // Единственное исключение: `ls -la <файл>` по СУЩЕСТВУЮЩИМ файлам — это взгляд
            // на размер и дату, list_files такого не даёт, и запрет был бы дед-эндом.
            if part.starts_with("ls ") && ls_targets_are_files(&part, &base) {
                continue;
            }
            link = Some(part);
            break;
        }
    }
    redirect_for_link(&link?, &base, "Bash-чтение")
}

/// Простое присваивание в начале звена: `root="/c/tmp"` (bash) или `$root = "C:/tmp"`
/// (PowerShell) → ("root", "/c/tmp"). Составное значение (подстановка, конвейер, несколько
/// слов без кавычек) не разбираем — гадать о цели хуже, чем воздержаться.
///
/// ⚠️ Без этого `root=<путь вне индекса>; ls "$root"` выглядел командой БЕЗ пути, цель
/// подменялась каталогом сессии, и гард отказывал по чужому репо (сообщено 10.09.2026).
fn parse_assignment(part: &str, ps: bool) -> Option<(String, String)> {
    let s = part.trim();
    let s = if ps { s.strip_prefix('$')? } else { s };
    let (name, value) = s.split_once('=')?;
    // В bash пробел перед `=` означает не присваивание, а команду с аргументами.
    if !ps && name.ends_with(char::is_whitespace) {
        return None;
    }
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let raw = value.trim();
    let quoted = raw.len() > 1
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')));
    let value = if quoted {
        raw[1..raw.len() - 1].to_string()
    } else if raw.is_empty() || raw.contains(char::is_whitespace) {
        return None;
    } else {
        raw.to_string()
    };
    if value.contains('$') || value.contains('`') {
        return None; // значение само собрано из подстановок — цель неизвестна
    }
    Some((name.to_string(), value))
}

/// Звено смены каталога → новый базовый каталог. None — звено не смена каталога.
/// Some("") — каталог стал неизвестен (popd, cd -, нераскрытая $-подстановка):
/// дальше воздерживаемся, а не судим по прежнему cwd.
///
/// `part` уже в нижнем регистре, `\` заменены на `/`, MSYS-диски приведены (так его
/// отдают циклы звеньев). Окружение функция не читает: домашний каталог передаёт
/// вызывающий, иначе модульные тесты зависели бы от переменных окружения машины.
fn dir_change(part: &str, base: &str, home: Option<&str>) -> Option<String> {
    let base = base.replace('\\', "/");
    let trimmed = base.trim_end_matches('/');
    // Корень `/` не должен схлопнуться в пустую строку: пустая база — это «каталог неизвестен».
    let base: &str = if trimmed.is_empty() && !base.is_empty() {
        "/"
    } else {
        trimmed
    };
    let (cmd, arg) = match part.split_once(char::is_whitespace) {
        Some((c, rest)) => (c, rest.trim()),
        None => (part, ""),
    };
    // Совпадение по слову целиком: `cdx foo` — не смена каталога.
    let (ps_no_arg, is_pop) = match cmd {
        "cd" | "pushd" => (false, false),
        // PowerShell без аргумента каталог не меняет: `sl` печатает текущий.
        "set-location" | "sl" | "push-location" => (true, false),
        "popd" | "pop-location" => (false, true),
        _ => return None,
    };
    if is_pop {
        return Some(String::new()); // куда вернулись — неизвестно
    }
    let arg = arg.trim().trim_matches('"').trim_matches('\'');
    // Ведущие ключи: у Set-Location это -Path/-LiteralPath, у cmd-шного cd — /d.
    let arg = ["-path ", "-literalpath ", "/d "]
        .iter()
        .find_map(|k| arg.strip_prefix(k))
        .unwrap_or(arg)
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    if arg.is_empty() {
        // Голый cd в bash уводит в домашний каталог, в PowerShell — никуда.
        return Some(if ps_no_arg {
            base.to_string()
        } else {
            home.unwrap_or("").to_string()
        });
    }
    // `cd -` и нераскрытая подстановка: цель неизвестна.
    if arg == "-" || arg.contains('$') {
        return Some(String::new());
    }
    if arg == "~" || arg.starts_with("~/") {
        let home = match home {
            Some(h) => h,
            None => return Some(String::new()), // домашнего каталога не знаем
        };
        let rest = arg.trim_start_matches('~').trim_start_matches('/');
        let abs = if rest.is_empty() {
            home.to_string()
        } else {
            format!("{}/{}", home.trim_end_matches('/'), rest)
        };
        return Some(normalize_dir(&abs));
    }
    let abs = if arg.contains(":/") || arg.starts_with('/') {
        arg.to_string()
    } else {
        if base.is_empty() {
            return Some(String::new()); // относительный от неизвестного каталога
        }
        format!("{base}/{arg}")
    };
    Some(normalize_dir(&abs))
}

/// Нормализация пути: сегменты `.` и пустые (кроме корня) выбрасываются, `..` снимает
/// предыдущий сегмент, но не выше корня (`c:` либо ведущего `/`). Хвостового `/` нет.
fn normalize_dir(path: &str) -> String {
    let (root, rest) = if let Some(r) = path.strip_prefix('/') {
        ("/", r)
    } else if path.len() >= 2 && path.as_bytes()[1] == b':' {
        (&path[..2], path[2..].trim_start_matches('/'))
    } else {
        ("", path)
    };
    let mut segs: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    let joined = segs.join("/");
    match root {
        "" => joined,
        "/" if joined.is_empty() => "/".to_string(),
        "/" => format!("/{joined}"),
        r if joined.is_empty() => r.to_string(),
        r => format!("{r}/{joined}"),
    }
}

/// Домашний каталог для раскрытия `cd ~`: USERPROFILE, иначе HOME, приведённый так же,
/// как сама команда (нижний регистр, `\`→`/`, MSYS-диски). Читает окружение только он.
fn home_dir() -> Option<String> {
    let raw = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))?;
    let norm = normalize_msys_drives(&raw.replace('\\', "/"));
    Some(norm.trim_end_matches('/').to_lowercase())
}

/// Подставляет known-переменные в звено: `$root` и `${root}`. Длинные имена первыми,
/// иначе `$a` испортил бы `$abc`. Неизвестные подстановки остаются как есть — по ним
/// вызывающий воздерживается.
fn expand_vars(text: &str, vars: &std::collections::HashMap<String, String>) -> String {
    if vars.is_empty() || !text.contains('$') {
        return text.to_string();
    }
    let mut names: Vec<&String> = vars.keys().collect();
    names.sort_by_key(|имя| std::cmp::Reverse(имя.len()));
    let mut out = text.to_string();
    for name in names {
        let value = &vars[name];
        out = out
            .replace(&format!("${{{name}}}"), value)
            .replace(&format!("${name}"), value);
    }
    out
}

/// Общий хвост redirect-веток Bash и PowerShell: найти индексированный корень по
/// звену команды (или по рабочему каталогу для bare-команды), отсеять исключения
/// проекта и выдать Deny с подсказкой про MCP. `tool_label` — начало текста отказа.
fn redirect_for_link(link: &str, base: &str, tool_label: &str) -> Option<Decision> {
    // Нераскрытая подстановка ($var из окружения, $(...), обратные кавычки) — цель звена
    // неизвестна. Воздерживаемся: подставить вместо неё каталог сессии значит отказать по
    // репозиторию, к которому команда может не иметь отношения.
    if link.contains('$') || link.contains('`') {
        return None;
    }
    // Команда трогает бинарь → воздержаться (его в индексе нет, дед-энд недопустим).
    if BINARY_EXT.iter().any(|e| link.contains(e)) {
        return None;
    }
    let paths = indexed_paths();
    let mut sorted = paths.clone();
    sorted.sort_by_key(|запись| std::cmp::Reverse(запись.0.len()));

    let mut alias: Option<String> = None;
    let mut matched_root: Option<String> = None;
    for (prefix, a) in &sorted {
        if cmd_contains_indexed_prefix(link, prefix) {
            alias = Some(a.clone());
            matched_root = Some(prefix.clone());
            break;
        }
    }
    // cwd-fallback только для истинно bare-команд (без явного пути-аргумента).
    // Раньше проверялось лишь отсутствие ':/' (Windows-диск), из-за чего
    // 'ls ~/.claude/...' и 'ls /tmp' из индексированного cwd ложно блокировались —
    // их цель явно вне репо. Теперь учитываем '~/' и Unix-absolute '/...' тоже.
    if alias.is_none() && !has_explicit_path(link) {
        if let Some((a, _, root)) = match_indexed(base, &paths) {
            alias = Some(a);
            matched_root = Some(root);
        }
    }
    let alias = alias?;
    // Исключения проекта — по КАЖДОМУ слову звена: цель может быть и путём, и маской.
    if let Some(root) = matched_root {
        for word in link.split_whitespace().skip(1) {
            if word.starts_with('-') {
                continue;
            }
            let clean = word.trim_matches('"').trim_matches('\'');
            if clean.is_empty() {
                continue;
            }
            // Абсолютный путь приводим к относительному ОТ ЕГО СОБСТВЕННОГО корня: склейка
            // root + "c:/repo/file" даёт заведомую чепуху, неиндексированный файл выглядит
            // неисключённым, и гард ложно отказывает (найдено живым дымом 27.08.2026).
            let (check_root, check_rel) = match match_indexed(clean, &paths) {
                Some((_, rel, matched)) => (matched, rel),
                None => (root.clone(), clean.to_string()),
            };
            if !check_rel.is_empty() && excluded_from_index(&check_root, &check_rel) {
                return None;
            }
        }
    }
    Some(Decision::Deny(format!(
        "{tool_label} по индексированному пути (repo='{alias}') запрещено. На индексированных \
         файлах MCP дешевле в 3-10×: инструменты code-index {}/{}/{}/\
         {}(repo=\"{alias}\", ...). Список репо — {}().",
        tool("grep_code"),
        tool("grep_text"),
        tool("read_file"),
        tool("list_files"),
        tool("get_stats")
    )))
}

/// Пер-файловое чтение cat/head/tail по ОДНОМУ индексированному файлу → Deny с
/// подсказкой read_file. Возвращает None (воздержаться) на всём, что сложнее
/// простого чтения: пайпы/редиректы/подстановки/glob (| > < ` $ ; && || * ? ~),
/// несколько файлов, неизвестные флаги, -f (tail follow), -c (байты), а также
/// если файл не индексирован или индекс отстал (read_is_blockable=false).
fn decide_bash_file_read(cmd: &str, lower: &str, cwd: Option<&str>) -> Option<Decision> {
    let s = lower.trim_start();
    let util = if s.starts_with("cat ") {
        "cat"
    } else if s.starts_with("head ") {
        "head"
    } else if s.starts_with("tail ") {
        "tail"
    } else {
        return None;
    };

    // Любая составная конструкция — не наша область, воздержаться.
    if cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains('<')
        || cmd.contains('`')
        || cmd.contains('$')
        || cmd.contains(';')
        || cmd.contains('*')
        || cmd.contains('?')
        || cmd.contains('~')
        || cmd.contains("&&")
        || cmd.contains("||")
    {
        return None;
    }

    // toks[0] — утилита; дальше флаги и РОВНО один файл (в любом порядке).
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut file: Option<&str> = None;
    let mut n: i64 = 10; // умолчание head/tail
    let mut i = 1;
    while i < toks.len() {
        let t = toks[i];
        if let Some(rest) = t.strip_prefix('-') {
            // cat с любым флагом меняет вывод (-n нумерация, -A и т.п.) → воздержаться.
            if util == "cat" {
                return None;
            }
            if t == "-n" {
                i += 1;
                n = toks.get(i)?.parse().ok()?;
            } else if let Some(num) = rest.strip_prefix('n') {
                n = num.parse().ok()?; // -n20
            } else if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                n = rest.parse().ok()?; // -20 (сокращение)
            } else {
                return None; // -f / -F / -c / неизвестный флаг
            }
        } else {
            if file.is_some() {
                return None; // несколько файлов
            }
            file = Some(t);
        }
        i += 1;
    }
    let file = file?.trim_matches('"').trim_matches('\'');
    if n <= 0 {
        return None;
    }

    // Абсолютный путь — как есть; относительный — от cwd сессии.
    let abs = if file.contains(":/") || file.starts_with('/') {
        file.to_string()
    } else {
        format!("{}/{}", cwd?.replace('\\', "/").trim_end_matches('/'), file)
    };

    let paths = indexed_paths();
    let (alias, rel, root) = match_indexed(&abs, &paths)?;
    if !read_is_blockable(&root, &rel) {
        return None; // не индексирован / oversize / индекс отстал → воздержаться
    }

    let how = match util {
        "head" => {
            format!(
                "{}(repo=\"{alias}\", path=\"{rel}\", line_start=1, line_end={n})",
                tool("read_file")
            )
        }
        "tail" => format!(
            "{}(repo=\"{alias}\", path=\"{rel}\") для lines_total, затем \
             {}(repo=\"{alias}\", path=\"{rel}\", line_start=lines_total-{n}+1)",
            tool("stat_file"),
            tool("read_file")
        ),
        _ => format!("{}(repo=\"{alias}\", path=\"{rel}\")", tool("read_file")),
    };
    Some(Decision::Deny(format!(
        "Bash-чтение '{util}' по индексированному файлу (repo='{alias}') запрещено. \
         MCP отдаёт дешевле и точнее, инструментами code-index: {how}."
    )))
}

// ---------------------------------------------------------------------------
// PowerShell — отдельный инструмент харнесса, те же чтения другими именами
// ---------------------------------------------------------------------------

/// Тот же разбор, что у Bash, но для командлетов PowerShell: Get-Content/gc/cat/type —
/// пер-файлово, Select-String/sls/Get-ChildItem/gci/ls/dir/findstr — repo-wide redirect.
///
/// ⚠️ Без этой ветки PowerShell был обходом гарда целиком: `Get-Content <индексированный файл>`
/// проходил там, где идентичный `cat` отказывал.
fn decide_powershell(ti: &Value, cwd: Option<&str>) -> Option<Decision> {
    let cmd = ti.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if cmd.is_empty() {
        return None;
    }
    let cmd = strip_here_strings(cmd);
    let lower = normalize_msys_drives(&cmd.replace('\\', "/").to_lowercase());

    if let Some(d) = decide_ps_file_read(&cmd, &lower, cwd) {
        return Some(d);
    }

    // Цепочка звеньев: та же логика, что у Bash (обрыв на первой одиночной трубе —
    // правее фильтруется чужой вывод объектами, а не читаются файлы).
    let mut base: String = cwd.unwrap_or("").to_string();
    let home = home_dir();
    let mut link: Option<String> = None;
    let mut vars: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for part in split_chain(&lower) {
        let part = expand_vars(part.trim(), &vars);
        if part.is_empty() {
            continue;
        }
        // `$root = "C:/tmp"` — не команда, а значение для следующих звеньев.
        if let Some((name, value)) = parse_assignment(&part, true) {
            vars.insert(name, value);
            continue;
        }
        // Смена каталога: cd / Set-Location / sl / Push-Location / pushd / popd.
        // Та же общая функция, что у Bash: `..`, `.` и `~` раскрываются, а незнакомый
        // каталог (Some("")) обнуляет базу — дальше гард воздерживается.
        if let Some(b) = dir_change(&part, &base, home.as_deref()) {
            base = b;
            continue;
        }
        // git grep / git log -S — история, индексом не покрыта.
        if part.starts_with("git ") {
            continue;
        }
        if PS_READ_UTILS.iter().any(|u| part.starts_with(u)) {
            // Список каталога заменяет list_files, но взгляд на размер/дату конкретных
            // существующих файлов MCP не отдаёт — там запрет был бы дед-эндом.
            let is_listing = ["ls ", "dir ", "gci ", "get-childitem "]
                .iter()
                .any(|u| part.starts_with(u));
            if is_listing && ls_targets_are_files(&part, &base) {
                continue;
            }
            link = Some(part);
            break;
        }
    }
    redirect_for_link(&link?, &base, "PowerShell-чтение")
}

/// Пер-файловое чтение Get-Content (алиасы gc/cat/type) по ОДНОМУ индексированному
/// файлу → Deny с подсказкой read_file. Воздерживается на всём, что сложнее простого
/// чтения: конвейеры, перенаправления, подстановки, маски, несколько файлов,
/// неизвестные флаги, а также когда файл не индексирован или индекс отстал.
fn decide_ps_file_read(cmd: &str, lower: &str, cwd: Option<&str>) -> Option<Decision> {
    let s = lower.trim_start();
    let util = ["get-content ", "gc ", "cat ", "type "]
        .iter()
        .find(|u| s.starts_with(**u))?
        .trim_end();

    // Любая составная конструкция — не наша область, воздержаться.
    // '$' покрывает переменные и подвыражения PowerShell, '`' — экранирование.
    if cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains('<')
        || cmd.contains('`')
        || cmd.contains('$')
        || cmd.contains(';')
        || cmd.contains('*')
        || cmd.contains('?')
        || cmd.contains('~')
        || cmd.contains("&&")
        || cmd.contains("||")
    {
        return None;
    }

    // toks[0] — командлет; дальше именованные параметры и РОВНО один файл.
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut file: Option<&str> = None;
    let mut head: Option<i64> = None; // -TotalCount N
    let mut tail: Option<i64> = None; // -Tail N
    let mut i = 1;
    while i < toks.len() {
        let t = toks[i];
        if t.starts_with('-') {
            match t.to_lowercase().as_str() {
                // Параметры со значением-путём: файл идёт следующим словом.
                "-path" | "-literalpath" => {
                    i += 1;
                    if file.is_some() {
                        return None; // несколько файлов
                    }
                    file = Some(toks.get(i)?);
                }
                "-totalcount" | "-first" | "-head" => {
                    i += 1;
                    head = Some(toks.get(i)?.parse().ok()?);
                }
                "-tail" | "-last" => {
                    i += 1;
                    tail = Some(toks.get(i)?.parse().ok()?);
                }
                // Вывод файла не меняют — читаем дальше.
                "-raw" | "-force" | "-nonewline" => {}
                "-encoding" => i += 1, // значение параметра пропускаем
                _ => return None,      // неизвестный параметр → воздержаться
            }
        } else {
            if file.is_some() {
                return None; // несколько файлов
            }
            file = Some(t);
        }
        i += 1;
    }
    let file = file?.trim_matches('"').trim_matches('\'');
    if head.is_some_and(|n| n <= 0) || tail.is_some_and(|n| n <= 0) {
        return None;
    }

    // Абсолютный путь — как есть; относительный — от cwd сессии.
    let abs = if file.contains(":/") || file.contains(":\\") || file.starts_with('/') {
        file.replace('\\', "/")
    } else {
        format!("{}/{}", cwd?.replace('\\', "/").trim_end_matches('/'), file)
    };

    let paths = indexed_paths();
    let (alias, rel, root) = match_indexed(&abs, &paths)?;
    if !read_is_blockable(&root, &rel) {
        return None; // не индексирован / oversize / индекс отстал → воздержаться
    }

    let how = match (head, tail) {
        (Some(n), _) => format!(
            "{}(repo=\"{alias}\", path=\"{rel}\", line_start=1, line_end={n})",
            tool("read_file")
        ),
        (_, Some(n)) => format!(
            "{}(repo=\"{alias}\", path=\"{rel}\") для lines_total, затем \
             {}(repo=\"{alias}\", path=\"{rel}\", line_start=lines_total-{n}+1)",
            tool("stat_file"),
            tool("read_file")
        ),
        _ => format!("{}(repo=\"{alias}\", path=\"{rel}\")", tool("read_file")),
    };
    Some(Decision::Deny(format!(
        "PowerShell-чтение '{util}' по индексированному файлу (repo='{alias}') запрещено. \
         MCP отдаёт дешевле и точнее, инструментами code-index: {how}."
    )))
}

/// Выбрасывает тела here-string'ов PowerShell (`@'` … `'@`, `@"` … `"@`): это ДАННЫЕ.
///
/// ⚠️ Ровно та же ловушка, что с heredoc'ами в Bash: сообщение коммита, начинающееся
/// со строки `ls ...` или `dir ...`, выглядело бы звеном чтения, и гард отказал бы в
/// законном `git commit -m @'...'@`.
fn strip_here_strings(cmd: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in cmd.lines() {
        let t = line.trim();
        if inside {
            // Закрывающая метка стоит в начале строки: '@ либо "@.
            if t.starts_with("'@") || t.starts_with("\"@") {
                inside = false;
            }
            continue;
        }
        if t.ends_with("@'") || t.ends_with("@\"") {
            inside = true;
            out.push(line); // сама команда с открытием — настоящая
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// true → у звена `ls` есть аргументы-цели и КАЖДАЯ из них — существующий файл.
/// Тогда это не обход каталога, а взгляд на размер и дату конкретных файлов: `list_files`
/// такого не отдаёт, и запрет был бы дед-эндом. Голый `ls`, каталог и маска целями-файлами
/// не считаются — они остаются перехваченными, как и прежде.
fn ls_targets_are_files(link: &str, base: &str) -> bool {
    let base = base.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let mut has_target = false;
    for word in link.split_whitespace().skip(1) {
        if word.starts_with('-') {
            continue;
        }
        let clean = word.trim_matches('"').trim_matches('\'');
        if clean.is_empty() || clean.contains('*') || clean.contains('?') {
            return false; // маска — это список
        }
        let abs = if clean.contains(":/") || clean.starts_with('/') {
            clean.to_string()
        } else {
            format!("{base}/{clean}")
        };
        if !std::path::Path::new(&abs).is_file() {
            return false; // каталог или несуществующий путь — список
        }
        has_target = true;
    }
    has_target
}

/// true → команда содержит явный путь-аргумент: Windows-диск (':/'), home ('~/')
/// или Unix-absolute токен ('/...'). Гейт cwd-fallback в decide_bash: если путь
/// задан явно, подменять его на индексированный cwd сессии нельзя (иначе
/// 'ls ~/.claude/...' и 'ls /tmp' из repo-cwd ложно блокировались).
fn has_explicit_path(lower: &str) -> bool {
    if lower.contains(":/") || lower.contains("~/") {
        return true;
    }
    lower.split_whitespace().any(|tok| tok.starts_with('/'))
}

/// true → `prefix` встречается в команде на границе пути (после него '/' либо
/// неалфанумерик/конец строки), а не как часть более длинного имени каталога.
/// Без этого 'code-index' ложно матчит 'code-index-guard', 'myapp' — 'myapp2'.
fn cmd_contains_indexed_prefix(lower: &str, prefix: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = lower[start..].find(prefix) {
        let after = start + pos + prefix.len();
        let boundary = match lower.as_bytes().get(after) {
            None => true, // конец строки
            Some(&b) => !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        };
        if boundary {
            return true;
        }
        start += pos + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Индексированные пути: парсинг daemon.toml + [[local]], матчинг префиксов
// ---------------------------------------------------------------------------

/// Конфиг хука (все поля опциональны):
///   - daemon_toml — путь к daemon.toml индексатора (репо оттуда);
///   - local       — репо, добавленные вручную (path + alias);
///   - events_db   — база событий хука (его решений).
///
/// Читается из code-index-guard.toml рядом с exe (или env CODE_INDEX_GUARD_CONFIG).
/// Файла/содержимого нет → индексированных путей нет, хук ничего не перехватывает.
#[derive(Default)]
struct GuardConfig {
    daemon_toml: Option<String>,
    local: Vec<(String, String)>,
    /// База событий хука; нет — события не записываются.
    events_db: Option<String>,
}

/// Путь к файлу конфига хука: env CODE_INDEX_GUARD_CONFIG → рядом с exe.
fn guard_config_path() -> Option<String> {
    if let Ok(p) = std::env::var("CODE_INDEX_GUARD_CONFIG") {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    Some(
        exe.parent()?
            .join("code-index-guard.toml")
            .to_string_lossy()
            .into_owned(),
    )
}

fn load_guard_config() -> GuardConfig {
    let content = guard_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    GuardConfig {
        daemon_toml: parse_top_level_string(&content, "daemon_toml"),
        local: parse_path_blocks(&content, "[[local]]"),
        events_db: parse_top_level_string(&content, "events_db"),
    }
}

/// Путь к базе событий хука — только ключ events_db конфига.
/// Ключа нет → None (события не записываются, дефолта нет).
fn events_db_path(cfg: &GuardConfig) -> Option<std::path::PathBuf> {
    cfg.events_db.as_ref().map(std::path::PathBuf::from)
}

/// Путь к daemon.toml: env CODE_INDEX_DAEMON_TOML → ключ daemon_toml конфига.
/// Нет ни того, ни другого → None (daemon.toml не читаем, дефолта нет).
fn daemon_toml_path(cfg: &GuardConfig) -> Option<String> {
    std::env::var("CODE_INDEX_DAEMON_TOML")
        .ok()
        .or_else(|| cfg.daemon_toml.clone())
}

/// Возвращает [(prefix_lower_noslash, alias)]. prefix — forward-slash, lower.
/// Список = репо из daemon.toml + [[local]] из конфига, с дедупликацией по пути:
/// daemon.toml приоритетен, [[local]] добавляет лишь отсутствующие в нём пути.
fn indexed_paths() -> Vec<(String, String)> {
    let cfg = load_guard_config();
    let mut v: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Репо из daemon.toml — приоритетны при совпадении пути.
    if let Some(path) = daemon_toml_path(&cfg) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            for (p, a) in parse_path_blocks(&content, "[[paths]]") {
                let norm = norm_prefix(&p);
                if !norm.is_empty() && seen.insert(norm.clone()) {
                    v.push((norm, a));
                }
            }
        }
    }
    // [[local]] — добавляем только пути, которых ещё нет (дедуп по префиксу).
    for (p, a) in &cfg.local {
        let norm = norm_prefix(p);
        if !norm.is_empty() && seen.insert(norm.clone()) {
            v.push((norm, a.clone()));
        }
    }
    v
}

fn norm_prefix(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// Выбрасывает тела heredoc'ов (`… <<'TAG'` … строка `TAG`): это ДАННЫЕ, а не команды.
///
/// ⚠️ Без этого строка сообщения коммита, начинающаяся с `ls ` или `grep `, выглядела поисковым
/// звеном, и гард отказывал в законном `git commit -F - <<'MSG'` (живой дым 27.08.2026).
/// Сама строка с `<<'TAG'` остаётся — она настоящая команда.
fn strip_heredocs(cmd: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut closing: Option<String> = None;
    for line in cmd.lines() {
        if let Some(tag) = &closing {
            if line.trim() == tag {
                closing = None;
            }
            continue;
        }
        if let Some(tag) = heredoc_tag(line) {
            closing = Some(tag);
        }
        out.push(line);
    }
    out.join("\n")
}

/// Тег heredoc из строки (`git commit -F - <<'msg'` → `msg`). None — heredoc'а в строке нет.
fn heredoc_tag(line: &str) -> Option<String> {
    let after = &line[line.find("<<")? + 2..];
    if after.starts_with('<') {
        return None; // `<<<` — here-string, тела у неё нет
    }
    let after = after.strip_prefix('-').unwrap_or(after).trim_start();
    let tag: String = if let Some(r) = after.strip_prefix('\'') {
        r.split('\'').next()?.to_string()
    } else if let Some(r) = after.strip_prefix('"') {
        r.split('"').next()?.to_string()
    } else {
        after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    (!tag.is_empty()).then_some(tag)
}

/// Разбивает команду на звенья по `&&`, `;`, переводу строки — и ОБРЫВАЕТ на первой одиночной
/// трубе: всё правее фильтрует чужой вывод, а не ищет по файлам.
///
/// ⚠️ Кавычки учитываются. Простое деление по `|` рвёт `grep -iE '^(PG|POSTGRES)' .env` по трубе
/// ВНУТРИ образца, и от команды остаётся огрызок `grep -iE '^(pg` — по такому «пути» гард
/// отказывал в законном поиске (проба 27.08.2026).
fn split_chain(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(k) = quote {
            if c == k {
                quote = None;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            cur.push(c);
            i += 1;
            continue;
        }
        let pair: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if pair == "&&" || pair == "||" {
            out.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        if c == ';' || c == '\n' {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        if c == '|' {
            // Одиночная труба: текущее звено закрываем, остальное отбрасываем целиком.
            out.push(std::mem::take(&mut cur));
            return out;
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur);
    out
}

/// Исключён ли путь из индекса — по встроенному списку И по настройкам САМОГО ПРОЕКТА.
///
/// ⚠️ Без второй половины гард выдаёт дед-энды ровно там, где обещал их не выдавать: в
/// `<repo>/.code-index/config.json` живут `exclude_dirs` и `exclude_file_patterns`, файлы оттуда
/// в индекс не попадают, и MCP их не отдаст. У нас туда внесены отчёты прогонов
/// (`*_run_*.json`) — их читают каждый круг, и запрет останавливал работу. Пять ложных отказов из
/// 24 проб 27.08.2026 были именно такими: `.env`, `.venv/Lib`, `tests/…_run_2026-08-20.json`.
///
/// Сравнение — ПО СЕГМЕНТАМ, а не подстрокой: встроенный список задан как `/.venv/`, а в командах
/// путь обычно относительный (`.venv/Lib`), и подстрока с ведущим слешем не находилась.
fn excluded_from_index(root: &str, rel: &str) -> bool {
    let rel_l = rel.replace('\\', "/").to_lowercase();
    let segments: Vec<&str> = rel_l.split('/').filter(|s| !s.is_empty()).collect();

    // Встроенные — то, что walker code-index не обходит никогда.
    const NEVER_INDEXED: &[&str] = &[
        ".git",
        "node_modules",
        ".code-index",
        "target",
        "__pycache__",
        ".venv",
    ];
    if segments.iter().any(|s| NEVER_INDEXED.contains(s)) {
        return true;
    }

    // Конкретный существующий файл — спрашиваем ФАКТ у индекса, а не гадаем по расширению.
    // Так отсекаются дотфайлы (`.env`), файлы без расширения (`Dockerfile`), `.conf`, `.svg` и
    // всё прочее, чего walker не берёт: списка этих типов снаружи нет — он зашит в индексаторе.
    if !rel_l.contains('*') && !rel_l.contains('?') {
        let full = format!(
            "{}/{}",
            root.replace('\\', "/").trim_end_matches('/'),
            rel_l
        );
        if std::path::Path::new(&full).is_file() && !read_is_blockable(root, rel) {
            return true;
        }
    }

    let cfg_path = format!("{}/.code-index/config.json", root.replace('\\', "/"));
    let Ok(text) = std::fs::read_to_string(&cfg_path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    if let Some(dirs) = cfg.get("exclude_dirs").and_then(|v| v.as_array()) {
        for d in dirs.iter().filter_map(|v| v.as_str()) {
            let d = d.to_lowercase();
            if segments.iter().any(|s| *s == d) {
                return true;
            }
        }
    }
    if let Some(pats) = cfg.get("exclude_file_patterns").and_then(|v| v.as_array()) {
        // Образец сверяем с КАЖДЫМ сегментом: цель бывает и файлом, и маской (`tests/*_run_*.json`).
        for p in pats.iter().filter_map(|v| v.as_str()) {
            let p = p.to_lowercase();
            if segments.iter().any(|s| glob_match(&p, s)) {
                return true;
            }
        }
    }
    false
}

/// Простейший glob: `*` — любой отрезок, остальное буквально. Достаточно для `*_run_*.json`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
            continue;
        }
        match text[pos..].find(part) {
            Some(k) => pos += k + part.len(),
            None => return false,
        }
    }
    if let Some(last) = parts.last() {
        if !last.is_empty() && !text.ends_with(last) {
            return false;
        }
    }
    true
}

/// Преобразует MSYS/Git-Bash диск-пути '/c/...' в Windows-форму 'c:/...', чтобы
/// матчинг префиксов срабатывал и для путей вида '/c/loom/...'. Иначе grep/ls/find/
/// cat с таким путём проскакивают мимо guard — строка '/c/loom' не содержит 'c:/loom'.
/// Преобразование — только на границе пути (начало строки или после не-алфанумерика).
fn normalize_msys_drives(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let boundary = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        if boundary
            && chars[i] == '/'
            && i + 2 < chars.len()
            && chars[i + 1].is_ascii_alphabetic()
            && chars[i + 2] == '/'
        {
            out.push(chars[i + 1]);
            out.push(':');
            out.push('/');
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Если raw попадает в индексированный префикс → (alias, rel_forward_slash, root_orig_case).
fn match_indexed(raw: &str, paths: &[(String, String)]) -> Option<(String, String, String)> {
    if raw.is_empty() {
        return None;
    }
    let norm = normalize_msys_drives(&raw.replace('\\', "/"));
    let norm_lower = norm.to_lowercase();
    // Длинные префиксы первыми: C:/Projects/app/core выигрывает над C:/Projects/app.
    let mut sorted: Vec<&(String, String)> = paths.iter().collect();
    sorted.sort_by_key(|запись| std::cmp::Reverse(запись.0.len()));
    for (prefix, alias) in sorted {
        let matched = norm_lower == *prefix || norm_lower.starts_with(&format!("{prefix}/"));
        if matched {
            // Слайс по байтовой длине префикса. Безопасно через get() — на случай
            // если lowercase сместил байтовую границу (для ASCII/кириллицы не смещает).
            let root = norm.get(..prefix.len()).unwrap_or(&norm).to_string();
            let rel = norm
                .get(prefix.len()..)
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string();
            return Some((alias.clone(), rel, root));
        }
    }
    None
}

/// Парсит секции `header` (напр. "[[paths]]" или "[[local]]") в
/// [(path, alias)]. alias по умолчанию — последний сегмент path.
fn parse_path_blocks(content: &str, header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for block in content.split(header).skip(1) {
        let mut path: Option<String> = None;
        let mut alias: Option<String> = None;
        for line in block.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                break; // следующая секция — конец блока [[paths]]
            }
            if let Some(v) = kv(l, "path") {
                if path.is_none() {
                    path = Some(v);
                }
            } else if let Some(v) = kv(l, "alias") {
                if alias.is_none() {
                    alias = Some(v);
                }
            }
        }
        if let Some(p) = path {
            let a = alias.unwrap_or_else(|| {
                p.replace('\\', "/")
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_lowercase()
                    .replace(' ', "_")
            });
            out.push((p, a));
        }
    }
    out
}

/// Парсит строку `key = "value"`. Возвращает value без кавычек и БЕЗ TOML-экранирования.
///
/// ⚠️ Разэкранирование обязательно, а не украшение. Пути в daemon.toml пишутся по стандарту —
/// `path = "C:\\projects\\myrepo"`, — и сырая строка после `replace('\\', "/")` превращалась
/// в `c://projects//myrepo`. Ни один префикс не совпадал, и гард молча выключался на ВСЕХ
/// репозиториях демона: 13 из 13 проходили без проверки. Найдено 27.08.2026 минимальным примером
/// — тот же путь с одинарным `\` или с `/` блокировался, с `\\` пропускался.
fn kv(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    // Закрывающая кавычка — первая НЕэкранированная.
    let bytes = rest.as_bytes();
    let mut end = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => {
                end = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    Some(unescape_toml(&rest[..end?]))
}

/// Базовое TOML-разэкранирование basic strings: `\\` → `\`, `\"` → `"`, `\/` → `/`.
/// Прочие последовательности оставляем как есть — в путях они не встречаются, а гадать вредно.
fn unescape_toml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('/') => out.push('/'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Первое значение top-level ключа `key = "value"` в TOML-подобном тексте.
/// Закомментированные (`# key = ...`) и вложенные в секции строки игнорируются
/// естественно: kv требует, чтобы строка начиналась с самого ключа.
fn parse_top_level_string(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| kv(line.trim(), key))
}

#[cfg(test)]
mod msys_tests {
    use super::normalize_msys_drives;

    #[test]
    fn converts_msys_drive() {
        assert_eq!(normalize_msys_drives("/c/loom/foo"), "c:/loom/foo");
        assert_eq!(
            normalize_msys_drives("grep x /c/loom/crates"),
            "grep x c:/loom/crates"
        );
    }

    #[test]
    fn leaves_windows_and_unix_paths() {
        assert_eq!(normalize_msys_drives("c:/loom/foo"), "c:/loom/foo");
        assert_eq!(normalize_msys_drives("/tmp/foo"), "/tmp/foo");
        assert_eq!(normalize_msys_drives("/home/user"), "/home/user");
        assert_eq!(normalize_msys_drives("abc/c/def"), "abc/c/def");
    }
}

#[cfg(test)]
mod dir_change_tests {
    use super::{dir_change, normalize_msys_drives, tool};

    const BASE: &str = "c:/repo/sub";
    const HOME: &str = "c:/users/u";

    #[test]
    fn relative_paths_join_base() {
        assert_eq!(
            dir_change("cd src", BASE, None).as_deref(),
            Some("c:/repo/sub/src")
        );
        assert_eq!(
            dir_change("cd src", "c:/repo", None).as_deref(),
            Some("c:/repo/src")
        );
        assert_eq!(
            dir_change("pushd src", "c:/repo", None).as_deref(),
            Some("c:/repo/src")
        );
        // Кавычки вокруг аргумента снимаются.
        assert_eq!(
            dir_change("cd \"src\"", "c:/repo", None).as_deref(),
            Some("c:/repo/src")
        );
        assert_eq!(
            dir_change("cd 'src'", "c:/repo", None).as_deref(),
            Some("c:/repo/src")
        );
    }

    #[test]
    fn dot_dot_is_expanded() {
        assert_eq!(dir_change("cd ..", BASE, None).as_deref(), Some("c:/repo"));
        assert_eq!(
            dir_change("cd ../x", BASE, None).as_deref(),
            Some("c:/repo/x")
        );
        assert_eq!(
            dir_change("cd ./src", "c:/repo", None).as_deref(),
            Some("c:/repo/src")
        );
        assert_eq!(
            dir_change("cd src/..", "c:/repo", None).as_deref(),
            Some("c:/repo")
        );
    }

    #[test]
    fn dot_dot_does_not_climb_above_root() {
        assert_eq!(
            dir_change("cd ../..", "c:/repo", None).as_deref(),
            Some("c:")
        );
        assert_eq!(dir_change("cd ..", "c:", None).as_deref(), Some("c:"));
        assert_eq!(dir_change("cd ..", "/", None).as_deref(), Some("/"));
    }

    #[test]
    fn absolute_paths() {
        assert_eq!(
            dir_change("cd c:/temp", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        assert_eq!(
            dir_change("cd /tmp", "c:/repo", None).as_deref(),
            Some("/tmp")
        );
        assert_eq!(
            dir_change("cd \"c:/temp\"", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        // MSYS-форма приходит сюда уже приведённой циклом звеньев: /c/temp → c:/temp.
        let msys = normalize_msys_drives("/c/temp");
        assert_eq!(msys, "c:/temp");
        assert_eq!(
            dir_change(&format!("cd {msys}"), "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
    }

    #[test]
    fn powershell_forms_and_path_keys() {
        assert_eq!(
            dir_change("sl c:/temp", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        assert_eq!(
            dir_change("push-location c:/temp", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        assert_eq!(
            dir_change("set-location -path c:/temp", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        assert_eq!(
            dir_change("set-location -literalpath \"c:/temp\"", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
        // cmd-шный /d: `cd /d C:\Temp`.
        assert_eq!(
            dir_change("cd /d c:/temp", "c:/repo", None).as_deref(),
            Some("c:/temp")
        );
    }

    #[test]
    fn home_is_expanded() {
        assert_eq!(
            dir_change("cd ~", BASE, Some(HOME)).as_deref(),
            Some("c:/users/u")
        );
        assert_eq!(
            dir_change("cd ~/p", BASE, Some(HOME)).as_deref(),
            Some("c:/users/u/p")
        );
        // Домашний каталог неизвестен (нет USERPROFILE/HOME) → воздержаться.
        assert_eq!(dir_change("cd ~", BASE, None).as_deref(), Some(""));
        assert_eq!(dir_change("cd ~/p", BASE, None).as_deref(), Some(""));
    }

    #[test]
    fn bare_dir_change() {
        // Голый cd в bash уводит домой, в PowerShell каталог не меняет.
        assert_eq!(
            dir_change("cd", BASE, Some(HOME)).as_deref(),
            Some("c:/users/u")
        );
        assert_eq!(dir_change("cd", BASE, None).as_deref(), Some(""));
        assert_eq!(
            dir_change("set-location", BASE, Some(HOME)).as_deref(),
            Some(BASE)
        );
        assert_eq!(dir_change("sl", BASE, Some(HOME)).as_deref(), Some(BASE));
    }

    #[test]
    fn unknown_directory_abstains() {
        assert_eq!(dir_change("cd -", BASE, None).as_deref(), Some(""));
        assert_eq!(dir_change("cd $d", BASE, None).as_deref(), Some(""));
        assert_eq!(dir_change("popd", BASE, None).as_deref(), Some(""));
        assert_eq!(dir_change("pop-location", BASE, None).as_deref(), Some(""));
        // Относительный аргумент от неизвестного каталога — цель неизвестна.
        assert_eq!(dir_change("cd src", "", None).as_deref(), Some(""));
    }

    #[test]
    fn not_a_directory_change() {
        // Совпадение по слову целиком: `cdx foo` — не смена каталога.
        assert_eq!(dir_change("cdx foo", BASE, None), None);
        assert_eq!(dir_change("pushd2 foo", BASE, None), None);
        assert_eq!(dir_change("ls src", BASE, None), None);
        assert_eq!(dir_change("grep -rn x src", BASE, None), None);
    }

    #[test]
    fn tool_without_prefix_is_bare_name() {
        // Без --mcp-prefix тексты для модели обязаны остаться прежними побайтно.
        assert_eq!(tool("read_file"), "read_file");
    }
}
