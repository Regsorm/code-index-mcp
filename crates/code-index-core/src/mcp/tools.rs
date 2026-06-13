// Реализации MCP-инструментов (v0.5+): read-only, с проверкой статуса папки у демона.
//
// Multi-repo: каждая функция принимает `&RepoEntry` (конкретный репозиторий, выбранный
// через `resolve_repo` в mod.rs по параметру `repo`). Диагностические инструменты
// `get_stats` и `health` принимают весь `&CodeIndexServer`, чтобы собрать сводку
// по всем подключённым репо.
//
// Перед каждым data-tool функция спрашивает у демона статус `root_path` этого репо.
// Если папка не `Ready` — возвращается `ToolUnavailable` JSON, и реальный запрос
// к БД не выполняется.

use super::{CodeIndexServer, RepoEntry};
use crate::daemon_core::client;
use crate::daemon_core::ipc::{PathStatus, ToolUnavailable};
use crate::storage::models::{ClassRecord, FunctionRecord};

/// Soft-cap: число строк в одном `read_file` (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_LINES: usize = 5_000;
/// Soft-cap: размер ответа `read_file` в байтах (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_BYTES: usize = 500 * 1024;
/// Hard-cap: абсолютный максимум для `read_file`, даже с line_start/line_end.
pub(crate) const READ_FILE_HARD_CAP_BYTES: usize = 2 * 1024 * 1024;
/// Hard-cap: суммарный размер ответа grep_text/grep_body.
pub(crate) const GREP_TOTAL_BYTES_CAP: usize = 1 * 1024 * 1024;
/// Default-limit grep_text если path_glob и language не заданы.
pub(crate) const GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT: usize = 100;
/// Default-limit grep_code по числу совпадений, если `limit` не передан.
/// Занижен до 100 (раньше было 500 с фильтром): по статистике использования
/// модель сама задаёт limit ~20-40, а 500 раздувал ответ вдвое против нативного
/// Grep (head_limit 250). При обрезке в ответе выставляется `truncated=true`.
pub(crate) const GREP_CODE_DEFAULT_LIMIT: usize = 100;

/// Default-cap на размер графа вызовов (get_callers/get_callees) и списков
/// импортов (get_imports). «Горячая» утилита вызывается из десятков тысяч мест →
/// без капа ответ раздувается на мегабайты (упор в лимит MCP-результата). При
/// обрезке в ответ добавляется {truncated, total, limit}. Перекрывается limit=.
pub(crate) const CALL_GRAPH_DEFAULT_LIMIT: usize = 200;
pub(crate) const IMPORTS_DEFAULT_LIMIT: usize = 200;

// ── Подсказки на пустой результат ────────────────────────────────────────────
//
// По статистике прогона на УТ-11 (анализ сырых транскриптов): при пустом ответе
// без подсказки модель повторяет тот же неподходящий вызов ×3, не понимая, что
// инструмент не тот. Поле `hint` рядом с `result` направляет к правильному
// инструменту (поле добавляется ТОЛЬКО при пустом результате — на непустых
// форма ответа не меняется, JSON-потребители не ломаются).

/// search_function / search_class вернул 0 — куда идти дальше.
pub(crate) const HINT_SEARCH_EMPTY: &str = "0 совпадений. Это нечёткий FTS-поиск по словам (OR между словами) в имени/docstring/теле. \
Для ТОЧНОГО имени символа — get_function/get_class/find_symbol. Для произвольного regex по коду — grep_code(regex=…). \
Для текста xml/md/yaml — grep_text(regex=…). Попробуйте меньше слов или синонимы.";
/// search_function вернул 0 на BSL-репо — дополнительно подсказываем search_terms
/// (термы есть только у BSL; по находке бенча 11.06 модель до него сама не доходит).
pub(crate) const HINT_SEARCH_EMPTY_BSL: &str = "0 совпадений. search_function ищет по словам, реально встречающимся \
в имени/docstring/теле функции — он промахивается, если функционал назван по-английски, а ты ищешь русским смыслом, \
либо словоформа не совпала. ▸ search_terms(query=…) идёт по ДРУГОМУ индексу — триграммному FTS по обогащённым термам, \
куда механически добавлен СИНОНИМ объекта-владельца (русское представление ↔ английский идентификатор) и комментарий; \
словоформы/регистр/ё неважны. Поэтому search_terms находит процедуру по смыслу там, где search_function по словам пуст. \
Дай ему 1-3 слова. Для ТОЧНОГО имени — get_function/find_symbol; regex по коду — grep_code(regex=…).";

/// Выбор hint'а пустого search_function по языку репо (search_terms есть только на BSL).
pub(crate) fn search_empty_hint(language: Option<&str>) -> &'static str {
    if language == Some("bsl") {
        HINT_SEARCH_EMPTY_BSL
    } else {
        HINT_SEARCH_EMPTY
    }
}
/// get_function / get_class по точному имени вернул 0.
pub(crate) const HINT_GET_EMPTY: &str = "0 символов с таким ТОЧНЫМ именем (регистр игнорируется). \
Для нечёткого поиска по словам — search_function('<слова>') / search_class('<слова>'); универсально (функции+классы+переменные+импорты) — find_symbol('<имя>').";

/// get_callers / get_callees вернул 0 рёбер.
pub(crate) const HINT_CALL_GRAPH_EMPTY: &str = "0 рёбер в графе вызовов. Имя должно быть ТОЧНЫМ \
(без скобок и имени модуля-владельца) — проверьте через get_function/find_symbol. \
Пусто также когда функцию реально никто не вызывает / она ничего не вызывает.";

/// list_files вернул 0 файлов.
pub(crate) const HINT_LIST_FILES_EMPTY: &str = "0 файлов. pattern — glob от корня репо \
(например '**/*.bsl', '**/Documents/**'). Проверьте path_prefix=/language=; \
файлы без расширения не индексируются.";

/// get_imports(file_id=…): у файла нет import-конструкций.
pub(crate) const HINT_IMPORTS_FILE_EMPTY: &str = "0 импортов: в файле нет import-конструкций. \
Для BSL это норма — в языке нет импортов; зависимости кода — get_callers/get_callees/get_file_summary.";

/// get_imports(module=…): никто не импортирует модуль с таким именем.
pub(crate) const HINT_IMPORTS_MODULE_EMPTY: &str = "0 импортов. module — ИМЯ импортируемого \
модуля как в import-операторе ('os', 'serde_json'), НЕ путь к файлу. Импорты внутри \
конкретного файла — get_imports(file_id=…) или get_file_summary(path). Для BSL импортов \
нет в принципе — зависимости через get_callers/get_callees.";

/// get_function/get_class: совпадений больше порога MULTI_DEF_THRESHOLD — тела опущены
/// (иначе на горячем имени ответ раздувается телами всех совпадений до сотен K токенов).
pub(crate) const HINT_GET_MULTI: &str = "Совпадений больше порога — тела опущены, показаны локации \
(имя/путь/строки/сигнатура). Тело конкретного — get_function/get_class с path_glob к нужному файлу, \
либо read_file(line_start,line_end). Навигация без тел — find_symbol.";

/// Порог числа одноимённых определений, выше которого get_function/get_class
/// опускают тела и возвращают локации (защита от взрыва токенов на горячих именах).
pub(crate) const MULTI_DEF_THRESHOLD: usize = 5;
/// Максимум ЛОКАЦИЙ в ответе get_function/get_class(multi)/find_symbol. На
/// сверхгорячем имени (сотни одноимённых определений) даже локации без тел
/// раздувают ответ (352 шт ≈ 32K токенов). Показываем первые, число всего — в totals.
pub(crate) const LOCATION_CAP: usize = 50;
/// find_symbol вернул 0 по всем категориям.
pub(crate) const HINT_FIND_SYMBOL_EMPTY: &str = "Символ не найден среди функций/классов/переменных/импортов (точное имя, регистр игнорируется). \
Для нечёткого поиска по словам — search_function/search_class; по коду — grep_code(regex=…).";
/// grep_code вернул 0 совпадений.
pub(crate) const HINT_GREP_CODE_EMPTY: &str = "0 совпадений. Параметр называется regex= (синтаксис crate regex), не query=. \
Поиск по ВСЕМУ коду файла. Только по телам функций/классов — grep_body; по xml/md/yaml/json — grep_text. \
Проверьте language=/path_glob= (oversize-файлы пропускаются).";
/// grep_body вернул 0 совпадений.
pub(crate) const HINT_GREP_BODY_EMPTY: &str = "0 совпадений в телах функций/классов. \
Для module-level кода, комментариев и идентификаторов ВНЕ тел — grep_code(regex=…); по xml/md/yaml — grep_text. \
Параметры: pattern= (подстрока) или regex= (regexp). Проверьте language=. \
СОСТАВНОЕ имя (Объект.Поле): в коде и особенно в тексте запроса 1С части и точка бывают разорваны \
переносами/пробелами/символом | — возьмите КОРОТКИЙ якорь (только Объект ИЛИ только Поле) либо \
regex с гибким пробелом, напр. Объект\\s*\\.\\s*Поле. Тексты запросов внутри строк — это тоже тело, \
ищите по одному слову, не по всей цепочке.";
/// grep_text вернул 0 совпадений.
pub(crate) const HINT_GREP_TEXT_EMPTY: &str = "0 совпадений в text-файлах (xml/md/yaml/json/toml). \
Для кода .bsl/.py/.rs и т.п. — grep_code(regex=…) или grep_body. Проверьте path_glob=/language=.";
/// search_text вернул 0 совпадений.
pub(crate) const HINT_SEARCH_TEXT_EMPTY: &str = "0 совпадений в text-файлах. Это нечёткий FTS-поиск по словам. \
Для regex по тексту — grep_text(regex=…); для кода — grep_code(regex=…)/grep_body.";

// ── BSL-варианты пустых hint'ов поиска процедур (бенч 11.06) ────────────────
// Пустой поиск процедуры по имени/тексту на BSL-репо часто означает, что код
// живёт в общем модуле БСП и привязан подпиской — точный поиск его не находит.
// Хвост `▸ search_terms…` приклеен к каждому такому пустому ответу (НЕ только
// первому: hint выдаётся всякий раз, когда результат пуст), чтобы оборвать
// серию слепых grep'ов (наблюдение test03: ходы 17/20/21 ушли вслепую, т.к.
// пустой grep_body не звал в search_terms). std::concat! требует литералы —
// поэтому хвост выписан в каждой константе целиком (без внешних крейтов).
pub(crate) const HINT_GREP_CODE_EMPTY_BSL: &str = "0 совпадений. Параметр называется regex= (синтаксис crate regex), не query=. \
Поиск по ВСЕМУ коду файла. Только по телам функций/классов — grep_body; по xml/md/yaml/json — grep_text. \
Проверьте language=/path_glob= (oversize-файлы пропускаются). \
▸ grep ищет ТОЧНЫЙ текст в файлах — он пуст, если слово в другой форме/регистре или код в общем модуле БСП. \
search_terms(query=…) идёт по ДРУГОМУ, триграммному FTS-индексу (словоформы, регистр/ё неважны, подстроки ≥3) \
по обогащённым термам = имя процедуры + СИНОНИМ объекта-владельца + комментарий — находит по смыслу то, что точный grep пропускает. Дай 1-3 слова.";
pub(crate) const HINT_GREP_BODY_EMPTY_BSL: &str = "0 совпадений в телах функций/классов. \
Для module-level кода, комментариев и идентификаторов ВНЕ тел — grep_code(regex=…); по xml/md/yaml — grep_text. \
Параметры: pattern= (подстрока) или regex= (regexp). Проверьте language=. \
СОСТАВНОЕ имя (Объект.Поле): в коде и особенно в тексте запроса 1С части и точка бывают разорваны \
переносами/пробелами/символом | — возьмите КОРОТКИЙ якорь (только Объект ИЛИ только Поле) либо \
regex с гибким пробелом, напр. Объект\\s*\\.\\s*Поле. Тексты запросов внутри строк — это тоже тело, \
ищите по одному слову, не по всей цепочке. \
▸ grep_body ищет ТОЧНЫЙ текст в телах — он пуст, если слово в другой форме/регистре или код в общем модуле БСП. \
search_terms(query=…) идёт по ДРУГОМУ, триграммному FTS-индексу (словоформы, регистр/ё неважны, подстроки ≥3) \
по обогащённым термам = имя процедуры + СИНОНИМ объекта-владельца + комментарий — находит по смыслу то, что точный grep пропускает. Дай 1-3 слова.";
pub(crate) const HINT_FIND_SYMBOL_EMPTY_BSL: &str = "Символ не найден среди функций/классов/переменных/импортов (точное имя, регистр игнорируется). \
Для нечёткого поиска по словам — search_function/search_class; по коду — grep_code(regex=…). \
▸ find_symbol ищет ТОЧНОЕ имя символа — он пуст, если функционал назван иначе/в другой словоформе или код в общем модуле БСП. \
search_terms(query=…) идёт по ДРУГОМУ, триграммному FTS-индексу (словоформы, регистр/ё неважны, подстроки ≥3) \
по обогащённым термам = имя процедуры + СИНОНИМ объекта-владельца + комментарий — находит по смыслу то, что точное имя пропускает. Дай 1-3 слова.";

/// Выбор hint'а пустого grep_code по языку репо.
pub(crate) fn grep_code_empty_hint(language: Option<&str>) -> &'static str {
    if language == Some("bsl") { HINT_GREP_CODE_EMPTY_BSL } else { HINT_GREP_CODE_EMPTY }
}
/// Выбор hint'а пустого grep_body по языку репо.
pub(crate) fn grep_body_empty_hint(language: Option<&str>) -> &'static str {
    if language == Some("bsl") { HINT_GREP_BODY_EMPTY_BSL } else { HINT_GREP_BODY_EMPTY }
}
/// Выбор hint'а пустого find_symbol по языку репо.
pub(crate) fn find_symbol_empty_hint(language: Option<&str>) -> &'static str {
    if language == Some("bsl") { HINT_FIND_SYMBOL_EMPTY_BSL } else { HINT_FIND_SYMBOL_EMPTY }
}

/// Сериализовать `ToolUnavailable` в JSON-строку.
pub fn format_unavailable(value: ToolUnavailable) -> String {
    match serde_json::to_string(&value) {
        Ok(s) => s,
        Err(e) => format!("{{\"status\":\"error\",\"message\":\"Сериализация: {}\"}}", e),
    }
}

/// Проверить у демона статус папки репо. `None` — папка Ready, можно продолжать.
/// `Some(json)` — нужно отдать клиенту этот ToolUnavailable-ответ вместо данных.
pub async fn check_path_status(entry: &RepoEntry) -> Option<String> {
    let root = entry.local_root();
    match client::path_status_async(root).await {
        Ok(resp) => match resp.status {
            PathStatus::Ready => None,
            PathStatus::InitialIndexing | PathStatus::ReindexingBatch => Some(format_unavailable(
                ToolUnavailable::Indexing {
                    progress: resp.progress.unwrap_or_default(),
                    message: match resp.status {
                        PathStatus::InitialIndexing => "Первичная индексация в процессе".into(),
                        _ => "Применяется батч изменений".into(),
                    },
                },
            )),
            PathStatus::NotStarted => Some(format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Путь {} не отслеживается демоном. Добавьте его в daemon.toml и вызовите 'code-index daemon reload'.",
                    root.display()
                ),
            })),
            PathStatus::Error => Some(format_unavailable(ToolUnavailable::Error {
                message: resp
                    .error
                    .unwrap_or_else(|| "Неизвестная ошибка индексации".into()),
            })),
        },
        Err(e) => Some(format_unavailable(ToolUnavailable::DaemonOffline {
            message: format!(
                "Демон code-index не доступен ({}). Запустите 'code-index daemon run' или Scheduled Task / systemd user unit.",
                e
            ),
        })),
    }
}

/// Макрос-хелпер: если папка не Ready — вернуть unavailable JSON немедленно.
macro_rules! bail_if_not_ready {
    ($entry:expr) => {{
        if let Some(json) = crate::mcp::tools::check_path_status($entry).await {
            return json;
        }
    }};
}

/// Макрос-хелпер: взять read-only соединение из пула репо. При ошибке открытия
/// (битый/недоступный файл индекса) — немедленно вернуть error-JSON. Заменяет
/// прежний `entry.local_storage().lock().await` (один мьютекс → пул соединений).
macro_rules! acquire_storage {
    ($entry:expr) => {
        match $entry.storage_pool().get().await {
            Ok(s) => s,
            Err(e) => return format!("{{\"error\": \"storage pool: {}\"}}", e),
        }
    };
}

/// Конкуррентное исполнение mass-mode (`names[]`/`full_names[]`): на каждый
/// элемент — checkout соединения из пула + `spawn_blocking` (rusqlite синхронный,
/// блокировать общий async-runtime нельзя). Параллелизм естественно ограничен
/// семафором пула (`max_size`). Порядок результатов = порядок `items`: задачи
/// запускаются конкуррентно, а handles собираются последовательно.
pub async fn mass_map<T, R, F>(
    pool: &std::sync::Arc<crate::storage::StoragePool>,
    items: Vec<T>,
    f: F,
) -> Vec<Result<R, String>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(&crate::storage::Storage, T) -> R + Send + Sync + 'static,
{
    let f = std::sync::Arc::new(f);
    let handles: Vec<_> = items
        .into_iter()
        .map(|it| {
            let pool = pool.clone();
            let f = f.clone();
            tokio::spawn(async move {
                let storage = pool
                    .get()
                    .await
                    .map_err(|e| format!("storage pool: {}", e))?;
                tokio::task::spawn_blocking(move || f(&storage, it))
                    .await
                    .map_err(|e| format!("task join: {}", e))
            })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(match h.await {
            Ok(r) => r,
            Err(e) => Err(format!("task join: {}", e)),
        });
    }
    out
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(s) => s,
        Err(e) => format!("{{\"error\": \"Сериализация: {}\"}}", e),
    }
}

// ── Event-based invalidation helpers (Phase 2) ──────────────────────────────

/// Завернуть результат tool'а в `{result, _meta: {dependent_files: [...]}}`.
///
/// Целевой потребитель — `mcp-cache-ci`: при cache-fill он парсит payload и
/// регистрирует связи `cache_key → file_path` в `reverse_index`. По
/// последующему `POST /invalidate {file_paths: [...]}` от daemon после
/// `transaction.commit()` SQLite (этап 3) cache-ci мгновенно сносит ровно те
/// entries, что зависят от изменённых файлов — не задевая соседних.
///
/// `dependent_files` пустой → entry попадёт в кэш без file-зависимостей и будет
/// чиститься только по TTL (как раньше). Это нормально для tools без явной
/// привязки к файлам (часть BSL-инструментов).
///
/// Дубликаты в `dependent_files` дедуплицируются (HashSet → Vec, без гарантии
/// порядка — cache-ci порядок не использует).
///
/// Дополнительно в `_meta` кладётся `file_mtimes: {<rel_path>: <i64>}` — индексный
/// mtime (unix-секунды) каждого зависимого файла. Это вход для write-triggered
/// ленивой ревалидации в `mcp-cache-ci`: прокси сверяет его с observed-mtime из
/// `mark-dirty` и кладёт ответ в кэш только когда индекс реально догнал диск
/// (`index_mtime >= observed_mtime`). См. карточку #1471. Файлы, для которых mtime
/// в индексе отсутствует, просто не попадают в карту (cache-ci трактует это как
/// «не могу сверить» → продолжает форвардить, пока путь dirty).
pub(crate) fn wrap_with_meta<T: serde::Serialize>(
    storage: &crate::storage::Storage,
    result: &T,
    dependent_files: Vec<String>,
) -> String {
    wrap_with_meta_hint(storage, result, dependent_files, None)
}

/// Как [`wrap_with_meta`], но с опциональной подсказкой `hint` на верхнем уровне.
/// Используется, когда результат ПУСТ: модель часто повторяет тот же неподходящий
/// вызов ×3. На непустом результате `hint=None` — форма ответа не меняется.
pub(crate) fn wrap_with_meta_hint<T: serde::Serialize>(
    storage: &crate::storage::Storage,
    result: &T,
    dependent_files: Vec<String>,
    hint: Option<&str>,
) -> String {
    let extra = hint.map(|h| serde_json::json!({ "hint": h }));
    wrap_with_meta_extra(storage, result, dependent_files, extra)
}

/// Базовая обёртка `{result, _meta, <extra…>}`. `extra` (если задан — объект)
/// подмешивает свои ключи на верхний уровень рядом с `result`/`_meta`.
/// Используется для `hint` (пустой результат) и для `{truncated,total,limit}`
/// (cap на крупных списках — get_callers/get_callees/get_imports).
pub(crate) fn wrap_with_meta_extra<T: serde::Serialize>(
    storage: &crate::storage::Storage,
    result: &T,
    dependent_files: Vec<String>,
    extra: Option<serde_json::Value>,
) -> String {
    use std::collections::HashSet;
    let deps: Vec<String> = dependent_files
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut file_mtimes = serde_json::Map::with_capacity(deps.len());
    for p in &deps {
        if let Some(m) = storage.mtime_for_path(p) {
            file_mtimes.insert(p.clone(), serde_json::json!(m));
        }
    }
    let result_value = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => return format!("{{\"error\": \"Сериализация result: {}\"}}", e),
    };
    let mut wrapped = serde_json::json!({
        "result": result_value,
        "_meta": { "dependent_files": deps, "file_mtimes": file_mtimes },
    });
    if let (Some(serde_json::Value::Object(m)), Some(obj)) = (extra, wrapped.as_object_mut()) {
        for (k, v) in m {
            obj.insert(k, v);
        }
    }
    serde_json::to_string(&wrapped)
        .unwrap_or_else(|e| format!("{{\"error\": \"Сериализация wrap: {}\"}}", e))
}

/// Собрать `dependent_files` из vec'а записей через extractor file_id.
/// Применяется к Vec<FunctionRecord>, Vec<ClassRecord>, Vec<CallRecord> и т.п.
/// Дубликаты не нужно дедуплицировать здесь — `wrap_with_meta` сам сделает.
pub(crate) fn collect_paths_via<R>(
    storage: &crate::storage::Storage,
    records: &[R],
    extract: impl Fn(&R) -> i64,
) -> Vec<String> {
    records
        .iter()
        .map(|r| lookup_path(storage, extract(r)))
        .filter(|p| !p.is_empty())
        .collect()
}

// ── Phase 1 helpers ─────────────────────────────────────────────────────────

/// Скомпилировать glob → matcher через `globset`. Применяется к результатам
/// после SQL-выборки в search_*/get_*. Использует `storage::normalize_glob`
/// для приведения `**` к `*` (см. SQLite GLOB-семантику).
pub(crate) fn build_path_matcher(glob: &str) -> Result<globset::GlobMatcher, String> {
    let normalized = crate::storage::normalize_glob(glob);
    globset::Glob::new(&normalized)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("невалидный glob '{}': {}", glob, e))
}

/// Lookup пути по file_id через storage. Любая ошибка/отсутствие → пустая строка
/// (она не пройдёт ни один matcher, так что результат честно отбросится).
/// Storage уже заблокирован вызывающей стороной (передаётся через `&MutexGuard`).
pub(crate) fn lookup_path(
    storage: &crate::storage::Storage,
    file_id: i64,
) -> String {
    storage
        .get_path_by_file_id(file_id)
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub(crate) fn matches_with(matcher: &globset::GlobMatcher, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    matcher.is_match(path)
}

// ── Реализации инструментов ─────────────────────────────────────────────────

/// Обрезка docstring для компактных выдач (карта файла, поисковые hit'ы):
/// схлопывание пробелов + ограничение DOC_CAP символов. Тело символа не входит.
pub(crate) fn truncate_doc(d: &Option<String>) -> Option<String> {
    const DOC_CAP: usize = 200;
    d.as_ref().map(|raw| {
        let one = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > DOC_CAP {
            let cut: String = one.chars().take(DOC_CAP).collect();
            format!("{cut}…")
        } else {
            one
        }
    })
}

/// Облегчённая проекция функции для ПОИСКОВОЙ выдачи (search_function): без тела
/// (body), docstring обрезан, добавлен file_path. Тело конкретной функции агент
/// берёт точечным get_function(name)/read_file. Без этого search_function отдавал
/// до 20 полных тел = 20-45K символов на запрос и раздувал контекст (УТ-11).
fn function_search_hit(fr: &FunctionRecord, path: &str) -> serde_json::Value {
    serde_json::json!({
        "name": fr.name,
        "qualified_name": fr.qualified_name,
        "file_path": path,
        "line_start": fr.line_start,
        "line_end": fr.line_end,
        "args": fr.args,
        "return_type": fr.return_type,
        "docstring": truncate_doc(&fr.docstring),
        "override_type": fr.override_type,
        "override_target": fr.override_target,
    })
}

/// Облегчённая проекция класса/структуры для ПОИСКОВОЙ выдачи (search_class):
/// без тела, docstring обрезан, добавлен file_path.
fn class_search_hit(cr: &ClassRecord, path: &str) -> serde_json::Value {
    serde_json::json!({
        "name": cr.name,
        "file_path": path,
        "line_start": cr.line_start,
        "line_end": cr.line_end,
        "bases": cr.bases,
        "docstring": truncate_doc(&cr.docstring),
    })
}

pub async fn search_function(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let want = limit.unwrap_or(20);
    // Если path_glob задан — берём с запасом (5×, до 500), потом фильтруем по пути,
    // потом обрезаем до want. Это компромисс между точностью и нагрузкой.
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_functions(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.truncate(want);
            }
            let deps = collect_paths_via(&storage, &r, |fr| fr.file_id);
            // W-бенч 11.06: на BSL-репо пустой результат ведёт в search_terms.
            let hint =
                if r.is_empty() { Some(search_empty_hint(entry.language.as_deref())) } else { None };
            // Поисковая выдача БЕЗ тел функций: только имя/путь/строки/сигнатура +
            // обрезанный docstring. Полные тела 20 результатов раздували ответ до
            // 20-45K символов (слабое место прогона УТ-11). Тело — get_function.
            let hits: Vec<serde_json::Value> = r
                .iter()
                .map(|fr| function_search_hit(fr, &lookup_path(&storage, fr.file_id)))
                .collect();
            wrap_with_meta_hint(&storage, &hits, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

pub async fn search_class(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let want = limit.unwrap_or(20);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_classes(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.truncate(want);
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            let hint = if r.is_empty() { Some(HINT_SEARCH_EMPTY) } else { None };
            // Поисковая выдача БЕЗ тел классов: имя/путь/строки/базы + обрезанный
            // docstring. Тело — get_class/read_file.
            let hits: Vec<serde_json::Value> = r
                .iter()
                .map(|cr| class_search_hit(cr, &lookup_path(&storage, cr.file_id)))
                .collect();
            wrap_with_meta_hint(&storage, &hits, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

pub async fn get_function(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    get_function_with(&storage, name, path_glob)
}

/// Sync-ядро get_function: вся работа на уже взятом соединении. Вызывается
/// inline одиночным путём и из `spawn_blocking` массовым (`mass_map`).
pub fn get_function_with(
    storage: &crate::storage::Storage,
    name: String,
    path_glob: Option<String>,
) -> String {
    match storage.get_function_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(storage, fr.file_id)));
            }
            let deps = collect_paths_via(storage, &r, |fr| fr.file_id);
            // Горячее имя (много одноимённых определений): тела опускаем — иначе ответ
            // раздувается телами всех совпадений до сотен K токенов. Возвращаем локации.
            if r.len() > MULTI_DEF_THRESHOLD {
                let total = r.len();
                let hits: Vec<serde_json::Value> = r
                    .iter()
                    .take(LOCATION_CAP)
                    .map(|fr| function_search_hit(fr, &lookup_path(storage, fr.file_id)))
                    .collect();
                let extra = serde_json::json!({
                    "hint": HINT_GET_MULTI,
                    "total": total,
                    "shown": hits.len(),
                    "truncated": total > hits.len(),
                });
                return wrap_with_meta_extra(storage, &hits, deps, Some(extra));
            }
            let hint = if r.is_empty() { Some(HINT_GET_EMPTY) } else { None };
            wrap_with_meta_hint(storage, &r, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

pub async fn get_class(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    get_class_with(&storage, name, path_glob)
}

/// Sync-ядро get_class — зеркало [`get_function_with`].
pub fn get_class_with(
    storage: &crate::storage::Storage,
    name: String,
    path_glob: Option<String>,
) -> String {
    match storage.get_class_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(storage, cr.file_id)));
            }
            let deps = collect_paths_via(storage, &r, |cr| cr.file_id);
            if r.len() > MULTI_DEF_THRESHOLD {
                let total = r.len();
                let hits: Vec<serde_json::Value> = r
                    .iter()
                    .take(LOCATION_CAP)
                    .map(|cr| class_search_hit(cr, &lookup_path(storage, cr.file_id)))
                    .collect();
                let extra = serde_json::json!({
                    "hint": HINT_GET_MULTI,
                    "total": total,
                    "shown": hits.len(),
                    "truncated": total > hits.len(),
                });
                return wrap_with_meta_extra(storage, &hits, deps, Some(extra));
            }
            let hint = if r.is_empty() { Some(HINT_GET_EMPTY) } else { None };
            wrap_with_meta_hint(storage, &r, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"get_class: {}\"}}", e),
    }
}

pub async fn get_callers(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.get_callers(&function_name, language.as_deref()) {
        Ok(mut r) => {
            let cap = limit.unwrap_or(CALL_GRAPH_DEFAULT_LIMIT);
            let total = r.len();
            let truncated = total > cap;
            if truncated {
                r.truncate(cap);
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            let extra = if truncated {
                Some(serde_json::json!({
                    "truncated": true, "total": total, "limit": cap,
                    "hint": "Показаны первые N вызывателей (горячая функция). Уточните limit= для большего числа или сузьте language=.",
                }))
            } else if r.is_empty() {
                // На пустом результате — hint (модель повторяет тот же вызов).
                Some(serde_json::json!({ "hint": HINT_CALL_GRAPH_EMPTY }))
            } else {
                None
            };
            wrap_with_meta_extra(&storage, &r, deps, extra)
        }
        Err(e) => format!("{{\"error\": \"get_callers: {}\"}}", e),
    }
}

pub async fn get_callees(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.get_callees(&function_name, language.as_deref()) {
        Ok(mut r) => {
            let cap = limit.unwrap_or(CALL_GRAPH_DEFAULT_LIMIT);
            let total = r.len();
            let truncated = total > cap;
            if truncated {
                r.truncate(cap);
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            let extra = if truncated {
                Some(serde_json::json!({
                    "truncated": true, "total": total, "limit": cap,
                    "hint": "Показаны первые N вызываемых. Уточните limit= для большего числа или сузьте language=.",
                }))
            } else if r.is_empty() {
                Some(serde_json::json!({ "hint": HINT_CALL_GRAPH_EMPTY }))
            } else {
                None
            };
            wrap_with_meta_extra(&storage, &r, deps, extra)
        }
        Err(e) => format!("{{\"error\": \"get_callees: {}\"}}", e),
    }
}

pub async fn find_path(
    entry: &RepoEntry,
    from: String,
    to: String,
    max_depth: Option<i64>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let depth = max_depth.unwrap_or(5);
    let storage = acquire_storage!(entry);
    match storage.find_call_path(&from, &to, depth, language.as_deref()) {
        Ok(opt) => {
            let found = opt.is_some();
            let path = opt.unwrap_or_default();
            let result = serde_json::json!({
                "from": from,
                "to": to,
                "found": found,
                "path": path,
                "max_depth": depth.clamp(1, 10),
            });
            // На пустом результате — hint (модель часто повторяет тот же вызов).
            let hint = (!found).then_some(
                "Путь не найден в графе вызовов. Увеличьте max_depth, проверьте точные имена функций (get_function) или снимите language=.",
            );
            wrap_with_meta_hint(&storage, &result, Vec::new(), hint)
        }
        Err(e) => format!("{{\"error\": \"find_path: {}\"}}", e),
    }
}

pub async fn get_call_tree(
    entry: &RepoEntry,
    root: String,
    direction: Option<String>,
    max_depth: Option<i64>,
    max_nodes: Option<i64>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    // direction: callees|down (что вызывает root, вглубь) | callers|up (кто вызывает root).
    let down = !matches!(direction.as_deref(), Some("callers") | Some("up"));
    let depth = max_depth.unwrap_or(3);
    let cap = max_nodes.unwrap_or(200);
    let storage = acquire_storage!(entry);
    match storage.get_call_tree(&root, down, depth, cap, language.as_deref()) {
        Ok((edges, truncated)) => {
            let tree = build_call_tree_json(&root, down, &edges);
            let empty = edges.is_empty();
            let result = serde_json::json!({
                "root": root,
                "direction": if down { "callees" } else { "callers" },
                "max_depth": depth.clamp(1, 10),
                "edge_count": edges.len(),
                "edges": edges,
                "tree": tree,
            });
            let extra = if truncated {
                Some(serde_json::json!({
                    "truncated": true,
                    "limit": cap.clamp(1, 5000),
                    "hint": "Дерево обрезано по max_nodes. Уменьшите max_depth или увеличьте max_nodes.",
                }))
            } else if empty {
                Some(serde_json::json!({
                    "hint": "Дерево пустое: у корня нет рёбер в этом направлении. Проверьте точное имя (get_function) или смените direction.",
                }))
            } else {
                None
            };
            wrap_with_meta_extra(&storage, &result, Vec::new(), extra)
        }
        Err(e) => format!("{{\"error\": \"get_call_tree: {}\"}}", e),
    }
}

/// Собрать вложенное дерево `{name, children:[...]}` из плоских рёбер
/// `get_call_tree`. Обход строго по уровням глубины (ребёнок узла на глубине d —
/// это ребро глубины d+1 с этим узлом в роли родителя), поэтому дерево не глубже
/// `max_depth` и циклы невозможны (глубина строго растёт). `down=true` — дети
/// узла его callee; `down=false` — его caller.
fn build_call_tree_json(
    root: &str,
    down: bool,
    edges: &[crate::storage::models::CallTreeEdge],
) -> serde_json::Value {
    use std::collections::{BTreeSet, HashMap, HashSet};
    // by_depth: глубина → родитель → отсортированное множество (ребёнок, line).
    let mut by_depth: HashMap<i64, HashMap<&str, BTreeSet<(&str, i64)>>> = HashMap::new();
    for e in edges {
        let (parent, child) = if down {
            (e.caller.as_str(), e.callee.as_str())
        } else {
            (e.callee.as_str(), e.caller.as_str())
        };
        by_depth
            .entry(e.depth)
            .or_default()
            .entry(parent)
            .or_default()
            .insert((child, e.line));
    }
    let max_d = edges.iter().map(|e| e.depth).max().unwrap_or(0);

    // expanded: узлы, уже развёрнутые в дереве. Повтор узла (несколько родителей
    // на разных путях) НЕ разворачивается заново → {name, repeated:true}. Иначе
    // при большой глубине дерево раздувается экспоненциально дублированием
    // поддеревьев (get_call_tree callers depth=5 на горячей функции = 178K токенов).
    fn build(
        node: &str,
        depth: i64,
        max_d: i64,
        by_depth: &HashMap<i64, HashMap<&str, BTreeSet<(&str, i64)>>>,
        expanded: &mut HashSet<String>,
    ) -> serde_json::Value {
        let first_time = expanded.insert(node.to_string());
        let mut children = Vec::new();
        if first_time && depth < max_d {
            if let Some(kids) = by_depth.get(&(depth + 1)).and_then(|m| m.get(node)) {
                for (child, line) in kids {
                    let mut sub = build(child, depth + 1, max_d, by_depth, expanded);
                    if let Some(obj) = sub.as_object_mut() {
                        obj.insert("line".to_string(), serde_json::json!(line));
                    }
                    children.push(sub);
                }
            }
        }
        if first_time {
            serde_json::json!({ "name": node, "children": children })
        } else {
            serde_json::json!({ "name": node, "repeated": true })
        }
    }

    let mut expanded = HashSet::new();
    build(root, 0, max_d, &by_depth, &mut expanded)
}

pub async fn find_symbol(
    entry: &RepoEntry,
    name: String,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.functions
                    .retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.classes
                    .retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.variables
                    .retain(|vr| matches_with(&matcher, &lookup_path(&storage, vr.file_id)));
                r.imports
                    .retain(|ir| matches_with(&matcher, &lookup_path(&storage, ir.file_id)));
            }
            let mut deps = collect_paths_via(&storage, &r.functions, |fr| fr.file_id);
            deps.extend(collect_paths_via(&storage, &r.classes, |cr| cr.file_id));
            deps.extend(collect_paths_via(&storage, &r.variables, |vr| vr.file_id));
            deps.extend(collect_paths_via(&storage, &r.imports, |ir| ir.file_id));
            let empty = r.functions.is_empty()
                && r.classes.is_empty()
                && r.variables.is_empty()
                && r.imports.is_empty();
            let hint = if empty { Some(find_symbol_empty_hint(entry.language.as_deref())) } else { None };
            // Навигационная выдача БЕЗ тел (как search_function/search_class): локации символа.
            // Тело конкретного — get_function/get_class. Иначе на горячем имени find_symbol
            // раздувался телами всех совпадений (десятки-сотни K токенов).
            // Cap на ЧИСЛО локаций по каждой категории: на сверхгорячем имени
            // даже локации без тел раздувают ответ. Показываем первые LOCATION_CAP.
            let (f_total, c_total, v_total, i_total) =
                (r.functions.len(), r.classes.len(), r.variables.len(), r.imports.len());
            let functions: Vec<serde_json::Value> = r
                .functions
                .iter()
                .take(LOCATION_CAP)
                .map(|fr| function_search_hit(fr, &lookup_path(&storage, fr.file_id)))
                .collect();
            let classes: Vec<serde_json::Value> = r
                .classes
                .iter()
                .take(LOCATION_CAP)
                .map(|cr| class_search_hit(cr, &lookup_path(&storage, cr.file_id)))
                .collect();
            let variables: Vec<&_> = r.variables.iter().take(LOCATION_CAP).collect();
            let imports: Vec<&_> = r.imports.iter().take(LOCATION_CAP).collect();
            let truncated = f_total > LOCATION_CAP
                || c_total > LOCATION_CAP
                || v_total > LOCATION_CAP
                || i_total > LOCATION_CAP;
            let mut payload = serde_json::Map::new();
            payload.insert("functions".into(), serde_json::Value::Array(functions));
            payload.insert("classes".into(), serde_json::Value::Array(classes));
            payload.insert(
                "variables".into(),
                serde_json::to_value(&variables).unwrap_or(serde_json::Value::Null),
            );
            payload.insert(
                "imports".into(),
                serde_json::to_value(&imports).unwrap_or(serde_json::Value::Null),
            );
            let payload = serde_json::Value::Object(payload);
            let extra = if let Some(h) = hint {
                Some(serde_json::json!({ "hint": h }))
            } else if truncated {
                Some(serde_json::json!({
                    "truncated": true,
                    "shown_cap": LOCATION_CAP,
                    "totals": { "functions": f_total, "classes": c_total, "variables": v_total, "imports": i_total },
                    "hint": "Локаций больше cap — показаны первые по каждой категории. Сузьте path_glob к нужному файлу.",
                }))
            } else {
                None
            };
            wrap_with_meta_extra(&storage, &payload, deps, extra)
        }
        Err(e) => format!("{{\"error\": \"find_symbol: {}\"}}", e),
    }
}

pub async fn get_imports(
    entry: &RepoEntry,
    file_id: Option<i64>,
    module: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let cap = limit.unwrap_or(IMPORTS_DEFAULT_LIMIT);
    let cap_extra = |total: usize| {
        (total > cap).then(|| serde_json::json!({ "truncated": true, "total": total, "limit": cap }))
    };
    if let Some(fid) = file_id {
        return match storage.get_imports_by_file(fid) {
            Ok(mut r) => {
                let extra = cap_extra(r.len()).or_else(|| {
                    r.is_empty()
                        .then(|| serde_json::json!({ "hint": HINT_IMPORTS_FILE_EMPTY }))
                });
                r.truncate(cap);
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta_extra(&storage, &r, deps, extra)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_file: {}\"}}", e),
        };
    }
    if let Some(ref m) = module {
        return match storage.get_imports_by_module(m, language.as_deref()) {
            Ok(mut r) => {
                let extra = cap_extra(r.len()).or_else(|| {
                    r.is_empty()
                        .then(|| serde_json::json!({ "hint": HINT_IMPORTS_MODULE_EMPTY }))
                });
                r.truncate(cap);
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta_extra(&storage, &r, deps, extra)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_module: {}\"}}", e),
        };
    }
    "{\"error\": \"Укажите file_id или module\"}".to_string()
}

pub async fn get_file_summary(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.get_file_summary(&path) {
        Ok(Some(s)) => {
            // Это КАРТА/оглавление файла, а не исходник: тела функций/классов
            // НЕ включаем. На больших BSL-модулях (десятки тысяч строк) полный
            // summary с телами раздувал ответ до мегабайт и упирался в лимит
            // одного MCP-результата — слабое место #4 прогона УТ-11
            // («get_file_summary ОШИБКА на больших модулях»). docstring
            // обрезаем. Тело конкретной функции — get_function / read_file.
            //
            // Зависимость одна и явная — путь из args; при изменении файла сюда
            // придёт invalidate от daemon.
            // docstring обрезаем общим helper'ом truncate_doc (см. выше).
            // На гигантских модулях 1С (ОбщегоНазначения, ПроведениеДокументов —
            // сотни процедур) даже карта без тел раздувается до 22-34K символов
            // (слабое место прогона УТ-11). Выше порога — компактная форма: только
            // имя + строки + признак переопределения, без сигнатур и docstring.
            // Все имена видны (навигация сохранена); детали/тело — get_function.
            const MAP_DETAIL_CAP: usize = 120;
            let compact_fn = s.functions.len() > MAP_DETAIL_CAP;
            let compact_cls = s.classes.len() > MAP_DETAIL_CAP;
            let functions: Vec<serde_json::Value> = s
                .functions
                .iter()
                .map(|f| {
                    if compact_fn {
                        serde_json::json!({
                            "name": f.name,
                            "line_start": f.line_start,
                            "line_end": f.line_end,
                            "override_type": f.override_type,
                        })
                    } else {
                        serde_json::json!({
                            "name": f.name,
                            "qualified_name": f.qualified_name,
                            "line_start": f.line_start,
                            "line_end": f.line_end,
                            "args": f.args,
                            "return_type": f.return_type,
                            "is_async": f.is_async,
                            "docstring": truncate_doc(&f.docstring),
                            "override_type": f.override_type,
                            "override_target": f.override_target,
                        })
                    }
                })
                .collect();
            let classes: Vec<serde_json::Value> = s
                .classes
                .iter()
                .map(|c| {
                    if compact_cls {
                        serde_json::json!({
                            "name": c.name,
                            "line_start": c.line_start,
                            "line_end": c.line_end,
                        })
                    } else {
                        serde_json::json!({
                            "name": c.name,
                            "bases": c.bases,
                            "line_start": c.line_start,
                            "line_end": c.line_end,
                            "docstring": truncate_doc(&c.docstring),
                        })
                    }
                })
                .collect();
            let payload = serde_json::json!({
                "file": s.file,
                "functions": functions,
                "functions_total": s.functions.len(),
                "classes": classes,
                "classes_total": s.classes.len(),
                "imports": s.imports,
                "variables": s.variables,
                "note": if compact_fn || compact_cls {
                    "большой модуль: компактная карта (только имена+строки); сигнатура/docstring/тело — get_function(name) / read_file(line_start,line_end)"
                } else {
                    "карта файла без тел функций/классов; тело — get_function(name) или read_file(line_start,line_end)"
                },
            });
            wrap_with_meta(&storage, &payload, vec![path.clone()])
        }
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_summary: {}\"}}", e),
    }
}

/// Статистика по одному репо: читает локальный SQLite. Для remote — паника
/// (диспатчер не должен сюда попадать). get_stats остаётся диагностическим:
/// возвращает данные даже если папка не Ready.
async fn local_stats(alias: &str, entry: &RepoEntry) -> serde_json::Value {
    let root = entry.local_root();
    let path_info = client::path_status_async(root).await.ok();
    let storage = match entry.storage_pool().get().await {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({
                "repo": alias,
                "error": format!("storage pool: {}", e),
                "path": root.display().to_string(),
            });
        }
    };
    match storage.get_stats() {
        Ok(mut stats) => {
            stats.indexing_status = None;
            serde_json::json!({
                "repo": alias,
                "db": stats,
                "path": root.display().to_string(),
                "daemon": path_info,
            })
        }
        Err(e) => serde_json::json!({
            "repo": alias,
            "error": format!("get_stats: {}", e),
            "path": root.display().to_string(),
        }),
    }
}

/// Запрос статистики у удалённого serve через `/federate/get_stats` с таймаутом.
async fn remote_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    use tokio::time::{timeout, Duration};

    let fut = crate::federation::dispatcher::dispatch_remote_value(
        &server.clients,
        &entry.ip,
        entry.port,
        "get_stats",
        serde_json::json!({ "repo": alias }),
    );
    let body = match timeout(Duration::from_secs(5), fut).await {
        Ok(b) => b,
        Err(_) => {
            return serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "status": "unreachable",
                "error": "timeout 5s",
            });
        }
    };
    // Удалённый сервер отвечает строкой JSON (тот же формат, что local_stats).
    // Если парсинг падает — остаётся хотя бы raw для диагностики.
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|_| {
        serde_json::json!({
            "repo": alias,
            "ip": entry.ip,
            "status": "parse_error",
            "raw": body,
        })
    })
}

/// Диспатч одного запроса по `repo` (с учётом is_local). Используется и через
/// MCP-tool, и через `/federate/get_stats` для конкретного алиаса.
pub async fn one_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    if entry.is_local {
        local_stats(alias, entry).await
    } else {
        remote_stats(server, alias, entry).await
    }
}

/// Полная сводка: для одного `repo` или fan-out по всем подключённым.
pub async fn get_stats(server: &CodeIndexServer, repo: Option<String>) -> String {
    if let Some(alias) = repo {
        return match server.repos.get(&alias) {
            Some(entry) => to_json(&one_stats(server, &alias, entry).await),
            None => format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Неизвестный repo '{}'. Доступные: {:?}.",
                    alias,
                    server.repo_aliases()
                ),
            }),
        };
    }

    // Fan-out по всем репо. Параллельно через JoinSet, удалённые с таймаутом 5с.
    let mut set = tokio::task::JoinSet::new();
    for alias in server.repos.keys().cloned().collect::<Vec<_>>() {
        let server_clone = server.clone();
        set.spawn(async move {
            let entry = server_clone
                .repos
                .get(&alias)
                .expect("alias только что взят из repos.keys()");
            one_stats(&server_clone, &alias, entry).await
        });
    }

    let mut all = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(v) => all.push(v),
            Err(e) => all.push(serde_json::json!({
                "status": "join_error",
                "error": e.to_string(),
            })),
        }
    }
    // JoinSet не сохраняет порядок — сортируем по `repo` для стабильности вывода.
    all.sort_by(|a, b| {
        let ka = a.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    to_json(&serde_json::json!({ "repos": all }))
}

pub async fn search_text(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let want = limit.unwrap_or(20);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_text(&query, sql_limit, language.as_deref()) {
        Ok(mut results) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                results.retain(|(p, _)| matches_with(&matcher, p));
                results.truncate(want);
            }
            let deps: Vec<String> = results.iter().map(|(p, _)| p.clone()).collect();
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(path, snippet)| serde_json::json!({ "path": path, "snippet": snippet }))
                .collect();
            let hint = if items.is_empty() { Some(HINT_SEARCH_TEXT_EMPTY) } else { None };
            wrap_with_meta_hint(&storage, &items, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"search_text: {}\"}}", e),
    }
}

pub async fn grep_body(
    entry: &RepoEntry,
    pattern: Option<String>,
    regex: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    path_glob: Option<String>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    // Если есть либо path_glob, либо context_lines — идём через grep_body_with_options
    // (он отдаёт флаг обрезки). Иначе старый grep_body для обратной совместимости с
    // CHANGELOG / тестами; там байтового потолка нет, поэтому truncated выводим из
    // того, что выборка упёрлась в limit.
    let ctx = context_lines.unwrap_or(0);
    let want = limit.unwrap_or(100);
    let result = if path_glob.is_some() || ctx > 0 {
        storage.grep_body_with_options(
            pattern.as_deref(),
            regex.as_deref(),
            language.as_deref(),
            path_glob.as_deref(),
            want,
            ctx,
            GREP_TOTAL_BYTES_CAP,
        )
    } else {
        storage
            .grep_body(pattern.as_deref(), regex.as_deref(), language.as_deref(), want)
            .map(|r| {
                let truncated = r.len() >= want;
                (r, truncated)
            })
    };
    match result {
        Ok((matches, truncated)) => {
            let deps: Vec<String> = matches.iter().map(|m| m.file_path.clone()).collect();
            let shown = matches.len();
            // Группировка находок по файлу: путь — один раз как ключ в `files`,
            // не дублируется в каждой находке (тот же приём, что у grep_code, v0.14.0).
            let mut files = serde_json::Map::new();
            for m in &matches {
                let mut obj = serde_json::Map::new();
                obj.insert("name".to_string(), serde_json::Value::String(m.name.clone()));
                obj.insert("kind".to_string(), serde_json::Value::String(m.kind.clone()));
                obj.insert("line_start".to_string(), serde_json::json!(m.line_start));
                obj.insert("line_end".to_string(), serde_json::json!(m.line_end));
                obj.insert("match_lines".to_string(), serde_json::json!(m.match_lines));
                if let Some(mc) = m.match_count {
                    obj.insert("match_count".to_string(), serde_json::json!(mc));
                }
                if !m.context.is_empty() {
                    obj.insert(
                        "context".to_string(),
                        serde_json::to_value(&m.context).unwrap_or(serde_json::Value::Null),
                    );
                }
                let arr = files
                    .entry(m.file_path.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let serde_json::Value::Array(a) = arr {
                    a.push(serde_json::Value::Object(obj));
                }
            }
            let payload = serde_json::json!({
                "files": files,
                "shown": shown,
                "limit": want,
                "truncated": truncated,
            });
            let hint =
                if shown == 0 { Some(grep_body_empty_hint(entry.language.as_deref())) } else { None };
            wrap_with_meta_hint(&storage, &payload, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
    }
}

// ── Phase 1 tool-handlers ───────────────────────────────────────────────────

pub async fn stat_file(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    // stat_file намеренно НЕ заворачиваем в `_meta` — он non-cacheable по
    // policy (всегда быстрая прямая выборка, к тому же быстро меняется на
    // тонких операциях типа `oversize` после реиндексации). Прокси даже не
    // увидит этот ответ в кэше.
    match storage.stat_file_meta(&path) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"stat_file: {}\"}}", e),
    }
}

pub async fn list_files(
    entry: &RepoEntry,
    pattern: Option<String>,
    path_prefix: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.list_files_filtered(
        pattern.as_deref(),
        path_prefix.as_deref(),
        language.as_deref(),
        limit.unwrap_or(500),
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|lf| lf.path.clone()).collect();
            let hint = r.is_empty().then_some(HINT_LIST_FILES_EMPTY);
            wrap_with_meta_hint(&storage, &r, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"list_files: {}\"}}", e),
    }
}

pub async fn read_file(
    entry: &RepoEntry,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    match storage.read_file_text(
        &path,
        line_start,
        line_end,
        READ_FILE_SOFT_CAP_LINES,
        READ_FILE_SOFT_CAP_BYTES,
        READ_FILE_HARD_CAP_BYTES,
        // size_limit_bytes для hint в oversize-ответе. MCP-слой не знает per-repo
        // лимит daemon'а — передаём None, hint будет короткий «файл превышает лимит».
        // file_size в ответе всё равно показывается, оператор может сравнить.
        None,
    ) {
        Ok(Some(r)) => wrap_with_meta(&storage, &r, vec![path.clone()]),
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден в индексе\"}}", path),
        Err(e) => format!("{{\"error\": \"read_file: {}\"}}", e),
    }
}

pub async fn grep_text(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let want = limit.unwrap_or_else(|| {
        // Без path_glob и language full-scan может быть тяжёлым — занижаем default.
        if path_glob.is_none() && language.is_none() {
            GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT
        } else {
            500
        }
    });
    match storage.grep_text_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok((matches, truncated)) => {
            let deps: Vec<String> = matches.iter().map(|m| m.path.clone()).collect();
            let shown = matches.len();
            // Группировка находок по файлу: путь хранится один раз как ключ в
            // `files`, а не повторяется в каждой находке (тот же приём, что у
            // grep_code с v0.14.0). truncated рядом — модель видит, что показано
            // не всё, и при необходимости дошлёт больший limit.
            let mut files = serde_json::Map::new();
            for m in &matches {
                let mut obj = serde_json::Map::new();
                obj.insert("line".to_string(), serde_json::json!(m.line));
                obj.insert(
                    "content".to_string(),
                    serde_json::Value::String(m.content.clone()),
                );
                if !m.context.is_empty() {
                    obj.insert(
                        "context".to_string(),
                        serde_json::to_value(&m.context).unwrap_or(serde_json::Value::Null),
                    );
                }
                let arr = files
                    .entry(m.path.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let serde_json::Value::Array(a) = arr {
                    a.push(serde_json::Value::Object(obj));
                }
            }
            let payload = serde_json::json!({
                "files": files,
                "shown": shown,
                "limit": want,
                "truncated": truncated,
            });
            let hint = if shown == 0 { Some(HINT_GREP_TEXT_EMPTY) } else { None };
            wrap_with_meta_hint(&storage, &payload, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"grep_text: {}\"}}", e),
    }
}

/// grep_code (Phase 2, v0.8.0): regex-поиск по содержимому **code-файлов** через
/// `file_contents` (zstd). Закрывает слепые зоны `grep_body` (ищет только в телах
/// функций/классов): module-level код, имена символов как идентификаторы,
/// комментарии вне тел, макросы, use-импорты. Файлы с `oversize=true` пропускаются —
/// для них нет content в индексе, нужно увеличить `max_code_file_size_bytes` либо
/// читать с диска.
pub async fn grep_code(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = acquire_storage!(entry);
    let want = limit.unwrap_or(GREP_CODE_DEFAULT_LIMIT);
    match storage.grep_code_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok((matches, truncated)) => {
            let deps: Vec<String> = matches.iter().map(|m| m.path.clone()).collect();
            let shown = matches.len();
            // Группировка находок по файлу: путь хранится один раз как ключ в
            // `files`, а не повторяется в каждой находке (находки часто кучкуются
            // в одном файле). Признак обрезки truncated — рядом: модель видит, что
            // показано не всё, и при необходимости дошлёт больший limit.
            let mut files = serde_json::Map::new();
            for m in &matches {
                let mut obj = serde_json::Map::new();
                obj.insert("line".to_string(), serde_json::json!(m.line));
                obj.insert("content".to_string(), serde_json::Value::String(m.content.clone()));
                if !m.context.is_empty() {
                    obj.insert(
                        "context".to_string(),
                        serde_json::to_value(&m.context).unwrap_or(serde_json::Value::Null),
                    );
                }
                let arr = files
                    .entry(m.path.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let serde_json::Value::Array(a) = arr {
                    a.push(serde_json::Value::Object(obj));
                }
            }
            let payload = serde_json::json!({
                "files": files,
                "shown": shown,
                "limit": want,
                "truncated": truncated,
            });
            let hint =
                if shown == 0 { Some(grep_code_empty_hint(entry.language.as_deref())) } else { None };
            wrap_with_meta_hint(&storage, &payload, deps, hint)
        }
        Err(e) => format!("{{\"error\": \"grep_code: {}\"}}", e),
    }
}

/// Живость MCP + демон по каждому репо.
pub async fn health(server: &CodeIndexServer) -> String {
    let daemon_info = client::runtime_info();

    // Сводка по репо: для local — статус пути у демона; для remote —
    // короткая запись без HTTP-ping (ping вне rc6).
    let mut repos = Vec::new();
    for (alias, entry) in server.repos.iter() {
        if !entry.is_local {
            repos.push(serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "kind": "remote",
            }));
            continue;
        }
        let root = entry.local_root();
        let path_status = match client::path_status_async(root).await {
            Ok(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        repos.push(serde_json::json!({
            "repo": alias,
            "root_path": root.display().to_string(),
            "path_status": path_status,
        }));
    }

    let daemon_health = match daemon_info {
        Some(_) => serde_json::json!({ "status": "online" }),
        None => serde_json::json!({
            "status": "offline",
            "message": client::daemon_unavailable_hint(),
        }),
    };

    let obj = serde_json::json!({
        "mcp": {
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "repos": server.repo_aliases(),
        },
        "daemon": daemon_health,
        "repos": repos,
    });
    to_json(&obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{PoolConfig, Storage, StoragePool};
    use std::time::{Duration, Instant};

    /// На BSL-репо пустые поиски процедур (search_function, grep_body, grep_code,
    /// find_symbol) подсказывают search_terms; на прочих языках — старый hint.
    #[test]
    fn search_empty_hint_mentions_search_terms_only_for_bsl() {
        // search_function
        assert!(search_empty_hint(Some("bsl")).contains("search_terms"));
        assert!(!search_empty_hint(Some("rust")).contains("search_terms"));
        assert!(!search_empty_hint(None).contains("search_terms"));
        assert_eq!(search_empty_hint(Some("rust")), HINT_SEARCH_EMPTY);
        // grep_body / grep_code / find_symbol — то же правило (бенч test03:
        // слепые grep_body на BSL должны звать в search_terms).
        for h in [
            grep_body_empty_hint(Some("bsl")),
            grep_code_empty_hint(Some("bsl")),
            find_symbol_empty_hint(Some("bsl")),
        ] {
            assert!(h.contains("search_terms"), "BSL grep/find hint должен звать search_terms");
        }
        assert_eq!(grep_body_empty_hint(Some("rust")), HINT_GREP_BODY_EMPTY);
        assert_eq!(grep_code_empty_hint(None), HINT_GREP_CODE_EMPTY);
        assert_eq!(find_symbol_empty_hint(Some("python")), HINT_FIND_SYMBOL_EMPTY);
        assert!(!HINT_GREP_BODY_EMPTY.contains("search_terms"));
    }

    /// Главное свойство mass_map: элементы исполняются КОНКУРРЕНТНО (каждый со
    /// своим соединением из пула), а порядок результатов = порядку items.
    /// 4 элемента по 100мс на пуле из 4 соединений обязаны уложиться сильно
    /// быстрее последовательных 400мс.
    #[tokio::test]
    async fn mass_map_runs_concurrently_and_preserves_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("index.db");
        Storage::open_file(&db_path).unwrap(); // создать файл + схему

        let cfg = PoolConfig {
            max_size: 4,
            cache_kib: 4096,
            busy_timeout_ms: 1000,
        };
        let pool = StoragePool::open_file_readonly(&db_path, cfg).unwrap();

        let start = Instant::now();
        let rows = mass_map(&pool, vec![0usize, 1, 2, 3], |_st, it| {
            std::thread::sleep(Duration::from_millis(100));
            it
        })
        .await;
        let elapsed = start.elapsed();

        let values: Vec<usize> = rows.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(values, vec![0, 1, 2, 3], "порядок = порядку items");
        assert!(
            elapsed < Duration::from_millis(250),
            "4×100мс заняли {}мс — mass_map исполняет последовательно (ожидалась параллель)",
            elapsed.as_millis()
        );
    }

    /// Пул из одного соединения (StoragePool::single) — деградация до
    /// последовательного исполнения, но корректность и порядок сохраняются.
    #[tokio::test]
    async fn mass_map_on_single_pool_stays_correct() {
        let storage = Storage::open_in_memory().unwrap();
        let pool = StoragePool::single(storage);
        let rows = mass_map(&pool, vec!["a".to_string(), "b".to_string()], |_st, it| it).await;
        let values: Vec<String> = rows.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(values, vec!["a".to_string(), "b".to_string()]);
    }
}
