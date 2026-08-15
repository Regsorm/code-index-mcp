//! Generic-страж размера ответа MCP-инструмента (`cap_response`).
//!
//! # Зачем
//!
//! Клиент (Claude Code / `claude` CLI) держит лимит на размер одного
//! `tool_result`, который он вливает inline в контекст модели
//! (`MAX_MCP_OUTPUT_TOKENS`, дефолт ≈25 000 токенов). Если ответ его
//! превышает — harness **сбрасывает весь payload в файл** на диск и отдаёт
//! модели только путь + короткий preview. После этого модель теряет
//! структурный inline-доступ и вынуждена грепать файл лишними ходами.
//!
//! Реальный класс срывов на бою — BSL-инструменты с неограниченными массивами
//! (значения системных перечислений `ХозяйственныеОперации` ≈816 элементов →
//! ~87К символов, источники подписок, реквизиты). Хард-капы ядра (`grep_*`
//! 1 МБ, `read_file` 2 МБ) этот класс не ловят — там не громадная строка, а
//! длинный массив.
//!
//! # Что делает страж
//!
//! Вместо слепого байтового отреза у harness'а мы режем **в источнике**:
//! пока сериализованный JSON не уложится в бюджет, повторно находим
//! самый «тяжёлый» массив (значение ключа в объекте) и усекаем его вдвое,
//! оставляя рядом маркеры:
//!
//! - `<ключ>_total` — исходное число элементов (ставится один раз);
//! - `<ключ>_truncated: true`.
//!
//! Так модель видит, что список сокращён, знает полное число и может
//! дозапросить точечно (по конкретному имени/фильтру) вместо чтения файла.
//!
//! # Единица измерения
//!
//! Бюджет — в **байтах** сериализованного JSON (`serde_json::to_string(..).len()`).
//! Это приближение к токенам: у кириллицы в UTF-8 ~2 байта/символ и ~2–4
//! байта/токен, у ASCII ~4 байта/токен. Дефолт держит кириллический JSON
//! заметно ниже 25k-токенного порога offload. Настраивается
//! `[mcp].max_response_bytes` (0 — страж выключен).
//!
//! Усекаются **только массивы** — большие строки (содержимое файлов из
//! `read_file`/`grep`) не трогаются (у них свои хард-капы), поэтому страж
//! безопасен для контент-инструментов.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use serde_json::{json, Value};

/// Дефолтный бюджет в байтах сериализованного JSON. ≈48 КБ ≈ 12–24k токенов
/// на кириллице — с запасом под 25k-токенный disk-offload клиента, и при этом
/// достаточно для полноценного ответа большинства инструментов.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 48_000;

/// Подсказка, добавляемая на верхний уровень ответа при усечении.
pub const CAP_HINT: &str = "Ответ усечён до лимита размера ([mcp].max_response_bytes) во избежание \
сброса в файл на стороне клиента. Самые длинные массивы сокращены — рядом с каждым `<ключ>_total` \
(исходное число элементов) и `<ключ>_truncated`. Нужен полный перечень — запросите точечно \
(по конкретному имени/фильтру) либо поднимите [mcp].max_response_bytes.";

/// Глобальный бюджет, выставляется при старте serve из `[mcp].max_response_bytes`.
/// 0 — страж выключен. До инициализации действует дефолт.
static RESPONSE_CAP_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_RESPONSE_BYTES);

/// Выставить бюджет (вызывается из serve-init по `[mcp].max_response_bytes`).
/// `None` → дефолт; `Some(0)` → страж выключен; `Some(n)` → n байт.
pub fn set_response_cap(bytes: Option<usize>) {
    RESPONSE_CAP_BYTES.store(bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES), Ordering::Relaxed);
}

/// Текущий бюджет в байтах (0 — выключен). Читается обёртками `wrap_with_meta`.
pub fn response_cap() -> usize {
    RESPONSE_CAP_BYTES.load(Ordering::Relaxed)
}

/// Потолок бюджета, который клиент вправе запросить на ОДИН вызов параметром
/// `max_response_bytes`. Запрос выше потолка не отклоняется, а зажимается.
/// Дефолт — 4× обычного бюджета: выше этого ответ рвёт контекст самого клиента
/// (на бою ловили `request (261008 tokens) exceeds available context (131072)`).
pub const DEFAULT_MAX_RESPONSE_BYTES_HARD: usize = DEFAULT_MAX_RESPONSE_BYTES * 4;

/// Потолок запрашиваемого бюджета, выставляется из `[cap].max_response_bytes_hard`.
static RESPONSE_CAP_HARD_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_RESPONSE_BYTES_HARD);

/// Выставить потолок (serve-init по `[cap].max_response_bytes_hard`).
/// `None` и `Some(0)` → дефолт: «нулевой потолок» означал бы, что любой запрос
/// клиента зажимается в 0 = страж выключен, то есть ровно наоборот смыслу.
pub fn set_response_cap_hard(bytes: Option<usize>) {
    let v = match bytes {
        Some(n) if n > 0 => n,
        _ => DEFAULT_MAX_RESPONSE_BYTES_HARD,
    };
    RESPONSE_CAP_HARD_BYTES.store(v, Ordering::Relaxed);
}

/// Текущий потолок запрашиваемого бюджета в байтах.
pub fn response_cap_hard() -> usize {
    RESPONSE_CAP_HARD_BYTES.load(Ordering::Relaxed)
}

/// Бюджет, применённый к одному ответу: результат разрешения клиентского
/// `max_response_bytes` относительно серверных настроек.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBudget {
    /// Сколько байт применено фактически (0 — страж выключен конфигом сервера).
    pub applied: usize,
    /// Что запросил клиент (`None` — параметр не передавался).
    pub requested: Option<usize>,
    /// Запрос был зажат потолком сервера.
    pub clamped: bool,
}

/// Разрешить бюджет одного вызова: клиентский `max_response_bytes` перекрывает
/// глобальный `[cap].max_response_bytes`, но не выше потолка
/// `[cap].max_response_bytes_hard` (сверх потолка — зажим, не отказ).
/// `Some(0)` не разрешаем: снимать страж можно только конфигом сервера,
/// иначе клиент одним параметром рвёт себе же контекст.
pub fn resolve_request_budget(requested: Option<usize>) -> RequestBudget {
    let global = response_cap();
    let hard = response_cap_hard();
    match requested {
        None | Some(0) => RequestBudget { applied: global, requested, clamped: false },
        Some(n) if n > hard => RequestBudget { applied: hard, requested, clamped: true },
        Some(n) => RequestBudget { applied: n, requested, clamped: false },
    }
}

/// Дефолтный порог тела функции/класса в СИМВОЛАХ. Тело длиннее → `get_function`
/// /`get_class` отдают навигационный стаб (голова+хвост+маркер+hint на read_file)
/// вместо полного тела. ~15k символов кириллицы ≈ заметно ниже 25k-токенного
/// disk-offload клиента; типичные процедуры (< порога) возвращаются целиком.
/// Тело — связный код, поэтому НЕ режем «серединой» (потеря логики): отдаём
/// голову и хвост + точный диапазон строк для точечного read_file.
pub const DEFAULT_MAX_FUNCTION_BODY_CHARS: usize = 15_000;

/// Порог тела функции/класса (символы). 0 — выключен (тело всегда целиком).
static FUNCTION_BODY_CAP_CHARS: AtomicUsize =
    AtomicUsize::new(DEFAULT_MAX_FUNCTION_BODY_CHARS);

/// Выставить порог тела (serve-init по `[mcp].max_function_body_chars`).
/// `None` → дефолт; `Some(0)` → выключено; `Some(n)` → n символов.
pub fn set_function_body_cap(chars: Option<usize>) {
    FUNCTION_BODY_CAP_CHARS
        .store(chars.unwrap_or(DEFAULT_MAX_FUNCTION_BODY_CHARS), Ordering::Relaxed);
}

/// Текущий порог тела в символах (0 — выключен). Читается в get_function/get_class.
pub fn function_body_cap() -> usize {
    FUNCTION_BODY_CAP_CHARS.load(Ordering::Relaxed)
}

// ── Параметр сервера: к каким инструментам применяется cap_response ──────────
//
// `cap_response` (обрез массивов с сэмплом) уместен для list-подобных выдач, где
// сэмпл + total достаточны. Какие именно tools под cap — задаётся параметром
// сервера `[mcp].cap_tools` (см. config). Пустой/отсутствующий список → дефолт
// ниже. Инструмент НЕ в списке → ответ не капается (отдаётся как есть; крупные
// структурные tools вроде get_object_structure управляют размером сами через
// omit_oversize_sections).

/// Дефолтный набор инструментов под cap_response (если `[mcp].cap_tools` пуст).
/// list-подобные BSL-tools, где обрез до сэмпла + total приемлем.
pub const DEFAULT_CAP_TOOLS: &[&str] =
    &["get_event_subscriptions", "bsl_sql", "find_references", "get_register_writers"];

fn default_cap_set() -> HashSet<String> {
    DEFAULT_CAP_TOOLS.iter().map(|s| s.to_string()).collect()
}

static CAP_TOOLS: LazyLock<ArcSwap<HashSet<String>>> =
    LazyLock::new(|| ArcSwap::from_pointee(default_cap_set()));

/// Выставить список инструментов под cap (serve-init по `[mcp].cap_tools`).
/// `None`/пустой → дефолтный набор `DEFAULT_CAP_TOOLS`.
pub fn set_cap_tools(tools: Option<Vec<String>>) {
    let set = match tools {
        Some(v) if !v.is_empty() => v.into_iter().collect(),
        _ => default_cap_set(),
    };
    CAP_TOOLS.store(Arc::new(set));
}

/// Глобальный выключатель cap_response. `true` (дефолт) → cap применяется к
/// инструментам из `CAP_TOOLS`; `false` → cap не применяется НИ К ОДНОМУ
/// инструменту (omit структурных и навигационный кап тела работают независимо —
/// у них свои гейты). Выставляется из `[mcp].cap_enabled`.
static CAP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Выставить глобальный выключатель cap (serve-init по `[mcp].cap_enabled`).
/// `None` → дефолт (включён); `Some(b)` → b.
pub fn set_cap_enabled(enabled: Option<bool>) {
    CAP_ENABLED.store(enabled.unwrap_or(true), Ordering::Relaxed);
}

/// Включён ли cap_response глобально.
pub fn cap_enabled() -> bool {
    CAP_ENABLED.load(Ordering::Relaxed)
}

/// Применяется ли cap_response к ответу инструмента `tool`.
/// Глобальный выключатель `cap_enabled` имеет приоритет над списком: при
/// `cap_enabled = false` cap не применяется ни к чему, что бы ни лежало в `CAP_TOOLS`.
pub fn cap_applies(tool: &str) -> bool {
    cap_enabled() && CAP_TOOLS.load().contains(tool)
}

// ── Механизм: к каким инструментам cap_response НЕ применяется ───────────────
//
// `cap_response` (слепой обрез массивов с сэмплом) уместен там, где массив —
// это СПИСОК и сэмпла достаточно (get_callers, grep, sources подписок).
// Для «СТРУКТУРНЫХ» инструментов массив/мапа = ПОЛНЫЙ авторитетный ответ
// (структура объекта 1С), и частичный обрез исказил бы результат — агент
// решит «вот все значения перечисления» и соврёт. Такие tools исключаются из
// cap_response и сами управляют размером через `omit_oversize_sections`
// (выкидывают тяжёлую секцию ЦЕЛИКОМ с маркером, не обрезая частично).
//
// Единый источник правды — этот список. Расширять сюда.
const STRUCTURAL_TOOLS: &[&str] = &["get_object_structure"];

/// Инструмент «структурный» (исключён из cap_response, использует
/// posекционный `omit_oversize_sections` + structural-wrap)?
pub fn is_structural_tool(tool: &str) -> bool {
    STRUCTURAL_TOOLS.contains(&tool)
}

/// Подсказка верхнего уровня при посекционном omit.
pub const OMIT_HINT: &str = "Крупные секции опущены ЦЕЛИКОМ (массив/мапа = полные данные \
объекта; частичный обрез исказил бы ответ). Рядом — `<секция>_omitted` + `<секция>_count`. \
Нужно значение/секция: проверить КОНКРЕТНОЕ значение — grep_code/grep_body по его имени; \
секция СРЕДНЕГО размера — запроси объект с узким sections=[<секция>] (только её); полный набор \
значений перечисления (сотни) дампом недоступен — бери конкретное значение по имени из кода \
объекта, где оно используется.";

/// Минимум ключей, при котором объект-map считается «секцией данных» (а не
/// структурной обёрткой) и может быть опущен целиком. Защищает result/_meta/
/// attributes (мало ключей) от выкидывания; ловит enum_synonyms (сотни ключей).
const OMIT_OBJECT_MIN_KEYS: usize = 16;

/// Найти самую тяжёлую опускаемую секцию — значение-ключа, являющееся массивом
/// (>1 элемента) ЛИБО объектом-map (> OMIT_OBJECT_MIN_KEYS ключей). Возвращает
/// (pointer_родителя, ключ, count, ser_size).
fn heaviest_section(root: &Value) -> Option<(String, String, usize, usize)> {
    fn walk(
        v: &Value,
        ptr: &str,
        parent: &str,
        key: Option<&str>,
        best: &mut Option<(String, String, usize, usize)>,
    ) {
        match v {
            Value::Array(arr) => {
                if let Some(k) = key {
                    if arr.len() > 1 {
                        let size = ser_len(v);
                        if best.as_ref().map_or(true, |b| size > b.3) {
                            *best = Some((parent.to_string(), k.to_string(), arr.len(), size));
                        }
                    }
                }
                for (i, c) in arr.iter().enumerate() {
                    walk(c, &format!("{}/{}", ptr, i), ptr, None, best);
                }
            }
            Value::Object(map) => {
                if let Some(k) = key {
                    if map.len() > OMIT_OBJECT_MIN_KEYS {
                        let size = ser_len(v);
                        if best.as_ref().map_or(true, |b| size > b.3) {
                            *best = Some((parent.to_string(), k.to_string(), map.len(), size));
                        }
                    }
                }
                for (k2, c) in map {
                    walk(c, &format!("{}/{}", ptr, esc(k2)), ptr, Some(k2), best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(root, "", "", None, &mut best);
    best
}

/// Посекционный страж размера для СТРУКТУРНЫХ ответов: пока сериализованный
/// размер превышает `budget` байт — выкидывает самую тяжёлую секцию (массив/мапа)
/// ЦЕЛИКОМ, заменяя её на `<ключ>_omitted: true` + `<ключ>_count: N` в родителе.
/// В отличие от `cap_response` НЕ режет частично (никаких «1 значение из 816»):
/// секция либо целиком в ответе, либо честно опущена с числом элементов.
/// budget == 0 → no-op. Возвращает (value, omitted_anything).
pub fn omit_oversize_sections(mut value: Value, budget: usize) -> (Value, bool) {
    if budget == 0 || ser_len(&value) <= budget {
        return (value, false);
    }
    let mut any = false;
    for _ in 0..256 {
        if ser_len(&value) <= budget {
            break;
        }
        let Some((parent_ptr, key, count, _)) = heaviest_section(&value) else {
            break; // больше нет опускаемых секций
        };
        match value.pointer_mut(&parent_ptr) {
            Some(Value::Object(parent)) => {
                parent.remove(&key);
                parent.insert(format!("{}_omitted", key), json!(true));
                parent.insert(format!("{}_count", key), json!(count));
                any = true;
            }
            _ => break,
        }
    }
    (value, any)
}

// ── Страж размера для ответа-ПОИСКА (`{matched, results:[...]}`) ────────────
//
// `omit_oversize_sections` для ответа-поиска ведёт себя разрушительно: самая
// тяжёлая секция там — сам внешний массив `results`, других претендентов рядом
// нет. Он вылетает первым же шагом, и клиент получает три числа вместо списка
// найденного («38 объектов, results_omitted: true»). Слабая модель из такого
// ответа не понимает даже того, что объекты есть.
//
// Здесь порядок деградации обратный: опознание элементов
// (`full_name`/`meta_type`/`name`/`synonym`) неприкосновенно, выбрасываются
// тяжёлые секции ВНУТРИ элементов, и лишь если этого не хватило — укорачивается
// сам массив с честными `results_total`/`results_shown`. Массив не обнуляется
// никогда: при любом бюджете больше нуля отдаётся хотя бы один элемент.

/// Ключ внешнего массива в ответе-поиске.
const RESULTS_KEY: &str = "results";

/// Запас байт под маркеры `results_total`/`results_shown`, которые ставятся
/// уже после подгонки массива под бюджет.
const RESULTS_MARKERS_BYTES: usize = 48;

/// Что именно пришлось сделать с ответом-поиском, чтобы уложиться в бюджет.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchShrink {
    /// Размер ПОЛНОГО ответа до ужатия, байт (для подсказки клиенту).
    pub full_bytes: usize,
    /// Ступень 1: сняты тяжёлые секции внутри элементов.
    pub sections_omitted: bool,
    /// Ступень 2: укорочен сам массив результатов.
    pub results_shortened: bool,
    /// Сколько элементов нашлось (заполняется при укорачивании).
    pub results_total: usize,
    /// Сколько элементов отдано (заполняется при укорачивании).
    pub results_shown: usize,
}

impl SearchShrink {
    /// Ответ вообще ужимался?
    pub fn any(&self) -> bool {
        self.sections_omitted || self.results_shortened
    }
}

/// Ужать ответ-поиск под `budget` байт, НЕ теряя опознания найденного.
/// `budget == 0` → no-op. Возвращает `(value, SearchShrink)`.
pub fn shrink_search_results(mut value: Value, budget: usize) -> (Value, SearchShrink) {
    let mut out = SearchShrink { full_bytes: ser_len(&value), ..Default::default() };
    if budget == 0 || out.full_bytes <= budget {
        return (value, out);
    }
    // Ступень 1: снимать тяжёлые секции ВНУТРИ элементов, пока не уложимся.
    // Потолок шагов — с запасом на 50 объектов × десяток секций каждый.
    for _ in 0..1024 {
        if ser_len(&value) <= budget {
            break;
        }
        let Some((parent_ptr, key, count, _)) = heaviest_section_in_items(&value) else {
            break; // внутри элементов опускать больше нечего
        };
        match value.pointer_mut(&parent_ptr) {
            Some(Value::Object(parent)) => {
                parent.remove(&key);
                parent.insert(format!("{}_omitted", key), json!(true));
                parent.insert(format!("{}_count", key), json!(count));
                out.sections_omitted = true;
            }
            _ => break,
        }
    }
    // Ступень 2: одни имена всё ещё не влезают — укоротить массив, оставив
    // минимум один элемент (клиент должен видеть, ЧТО найдено, а не только сколько).
    if ser_len(&value) > budget {
        let total = value
            .get(RESULTS_KEY)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let mut shown = total;
        while shown > 1 && ser_len(&value) + RESULTS_MARKERS_BYTES > budget {
            shown -= 1;
            if let Some(Value::Array(arr)) = value.get_mut(RESULTS_KEY) {
                arr.truncate(shown);
            }
        }
        if shown < total {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("results_total".to_string(), json!(total));
                obj.insert("results_shown".to_string(), json!(shown));
            }
            out.results_shortened = true;
            out.results_total = total;
            out.results_shown = shown;
        }
    }
    (value, out)
}

/// Самая тяжёлая опускаемая секция среди ЭЛЕМЕНТОВ массива `results`.
/// Сам массив кандидатом не является — в этом всё отличие от
/// `heaviest_section`. Возвращает (pointer_родителя, ключ, count, ser_size),
/// pointer — абсолютный от корня ответа.
fn heaviest_section_in_items(root: &Value) -> Option<(String, String, usize, usize)> {
    let items = root.get(RESULTS_KEY)?.as_array()?;
    let mut best: Option<(String, String, usize, usize)> = None;
    for (i, item) in items.iter().enumerate() {
        if let Some((parent, key, count, size)) = heaviest_section(item) {
            if best.as_ref().map_or(true, |b| size > b.3) {
                best = Some((
                    format!("/{}/{}{}", RESULTS_KEY, i, parent),
                    key,
                    count,
                    size,
                ));
            }
        }
    }
    best
}

/// Длина сериализованного JSON в байтах.
fn ser_len(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

/// Экранировать сегмент JSON-pointer по RFC 6901: `~`→`~0`, `/`→`~1`.
/// Ключи метаданных 1С спецсимволов обычно не содержат, но экранируем честно.
fn esc(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// Найти массив-значение-ключа с максимальным сериализованным размером.
/// Возвращает `(pointer_массива, pointer_родителя, ключ, ser_size)`.
/// Рассматриваются только массивы с >1 элементом, у которых есть
/// родитель-объект и ключ (по нему вешаются маркеры `<ключ>_total`).
fn heaviest_array(root: &Value) -> Option<(String, String, String, usize)> {
    fn walk(
        v: &Value,
        ptr: &str,
        parent: &str,
        key: Option<&str>,
        best: &mut Option<(String, String, String, usize)>,
    ) {
        match v {
            Value::Array(arr) => {
                if let Some(k) = key {
                    if arr.len() > 1 {
                        let size = ser_len(v);
                        if best.as_ref().map_or(true, |b| size > b.3) {
                            *best =
                                Some((ptr.to_string(), parent.to_string(), k.to_string(), size));
                        }
                    }
                }
                for (i, child) in arr.iter().enumerate() {
                    walk(child, &format!("{}/{}", ptr, i), ptr, None, best);
                }
            }
            Value::Object(map) => {
                for (k, child) in map {
                    walk(child, &format!("{}/{}", ptr, esc(k)), ptr, Some(k), best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(root, "", "", None, &mut best);
    best
}

/// Ужать `value`, пока сериализованный размер не уложится в `budget` байт.
///
/// `budget == 0` → no-op. Возвращает `(value, truncated_anything)`. Каждый шаг
/// ополовинивает самый тяжёлый массив (минимум 1 элемент), так что сходимся за
/// O(log) шагов на массив; потолок итераций — страховка от зацикливания.
pub fn cap_response(mut value: Value, budget: usize) -> (Value, bool) {
    if budget == 0 || ser_len(&value) <= budget {
        return (value, false);
    }
    let mut any = false;
    for _ in 0..256 {
        if ser_len(&value) <= budget {
            break;
        }
        let Some((arr_ptr, parent_ptr, key, _)) = heaviest_array(&value) else {
            break; // больше нечего усекать (нет массивов >1)
        };
        // Усечь самый тяжёлый массив вдвое; запомнить исходную длину ДО усечения.
        let orig_len = match value.pointer_mut(&arr_ptr) {
            Some(Value::Array(arr)) => {
                if arr.len() <= 1 {
                    break; // самый тяжёлый уже неуменьшаем → стоп
                }
                let orig = arr.len();
                arr.truncate((orig / 2).max(1));
                orig
            }
            _ => break,
        };
        // Маркеры рядом с массивом. `_total` — через or_insert: на повторном
        // усечении того же массива сохраняется ПЕРВОЕ (истинно исходное) число.
        if let Some(Value::Object(parent)) = value.pointer_mut(&parent_ptr) {
            parent
                .entry(format!("{}_total", key))
                .or_insert(json!(orig_len));
            parent.insert(format!("{}_truncated", key), json!(true));
        }
        any = true;
    }
    (value, any)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_budget_unchanged() {
        let v = json!({"a": [1, 2, 3], "b": "hello"});
        let (out, trunc) = cap_response(v.clone(), 10_000);
        assert!(!trunc);
        assert_eq!(out, v);
    }

    #[test]
    fn budget_zero_disables() {
        let big: Vec<i64> = (0..5000).collect();
        let v = json!({"items": big});
        let (out, trunc) = cap_response(v.clone(), 0);
        assert!(!trunc);
        assert_eq!(out, v);
    }

    #[test]
    fn truncates_single_big_array_and_marks_total() {
        // 2000 объектов-элементов — заведомо больше бюджета.
        let items: Vec<Value> = (0..2000)
            .map(|i| json!({"name": format!("Операция_{}", i), "v": i}))
            .collect();
        let v = json!({"object": "Документ", "enum_values": items});
        let budget = 4_000;
        let (out, trunc) = cap_response(v, budget);
        assert!(trunc);
        // Уложились в бюджет.
        assert!(ser_len(&out) <= budget, "ser_len={}", ser_len(&out));
        // Массив реально сокращён.
        let arr = out["enum_values"].as_array().unwrap();
        assert!(arr.len() < 2000 && !arr.is_empty());
        // Маркеры на месте, _total — исходное число.
        assert_eq!(out["enum_values_total"], json!(2000));
        assert_eq!(out["enum_values_truncated"], json!(true));
    }

    #[test]
    fn truncates_nested_array_under_object() {
        let items: Vec<Value> = (0..3000).map(|i| json!(format!("реквизит_{}", i))).collect();
        let v = json!({
            "result": {
                "structure": {"attributes": items, "name": "Контрагенты"}
            }
        });
        let budget = 3_000;
        let (out, trunc) = cap_response(v, budget);
        assert!(trunc);
        assert!(ser_len(&out) <= budget);
        let st = &out["result"]["structure"];
        assert_eq!(st["attributes_total"], json!(3000));
        assert_eq!(st["attributes_truncated"], json!(true));
        assert!(st["attributes"].as_array().unwrap().len() < 3000);
        // Соседние скалярные ключи не тронуты.
        assert_eq!(st["name"], json!("Контрагенты"));
    }

    #[test]
    fn picks_heaviest_among_several_arrays() {
        let small: Vec<Value> = (0..5).map(|i| json!(i)).collect();
        let big: Vec<Value> = (0..4000).map(|i| json!(format!("x{}", i))).collect();
        let v = json!({"small": small, "big": big});
        let budget = 5_000;
        let (out, trunc) = cap_response(v, budget);
        assert!(trunc);
        assert!(ser_len(&out) <= budget);
        // Тяжёлый усечён, маленький — нет.
        assert_eq!(out["big_truncated"], json!(true));
        assert!(out.get("small_truncated").is_none());
        assert_eq!(out["small"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn is_structural_tool_policy() {
        assert!(is_structural_tool("get_object_structure"));
        assert!(!is_structural_tool("get_event_subscriptions"));
        assert!(!is_structural_tool("get_callers"));
    }

    #[test]
    fn cap_enabled_gates_cap_applies() {
        // Список содержит инструмент, но глобальный выключатель главнее.
        set_cap_tools(Some(vec!["get_event_subscriptions".to_string()]));
        set_cap_enabled(Some(true));
        assert!(cap_applies("get_event_subscriptions"), "enabled+в списке → cap применяется");
        set_cap_enabled(Some(false));
        assert!(!cap_applies("get_event_subscriptions"), "disabled → cap не применяется ни к чему");
        // Восстановить дефолты, чтобы не влиять на другие тесты.
        set_cap_enabled(Some(true));
        set_cap_tools(None);
    }

    #[test]
    fn default_cap_tools_include_list_tools() {
        let set = default_cap_set();
        assert!(set.contains("find_references"));
        assert!(set.contains("get_register_writers"));
        assert!(set.contains("get_event_subscriptions"));
        assert!(set.contains("bsl_sql"));
    }

    #[test]
    fn omit_drops_heavy_map_wholesale_keeps_small() {
        // enum-подобная структура: большая map (синонимы) + массив имён + мелочь
        let mut syn = serde_json::Map::new();
        for i in 0..800 {
            syn.insert(format!("Значение_{}", i), json!(format!("Синоним значения номер {}", i)));
        }
        let values: Vec<Value> = (0..800).map(|i| json!(format!("Значение_{}", i))).collect();
        let v = json!({
            "attributes": {
                "enum_synonyms": Value::Object(syn),
                "enum_values": values,
            },
            "counts": { "enum_values": 800 },
            "full_name": "Enum.Тест",
            "meta_type": "Enum",
        });
        let budget = 30_000;
        let (out, omitted) = omit_oversize_sections(v, budget);
        assert!(omitted);
        assert!(ser_len(&out) <= budget, "ser_len={}", ser_len(&out));
        let a = &out["attributes"];
        // самая тяжёлая секция (map синонимов) выкинута целиком + count
        assert_eq!(a["enum_synonyms_omitted"], json!(true));
        assert_eq!(a["enum_synonyms_count"], json!(800));
        assert!(a.get("enum_synonyms").is_none(), "map должна быть удалена целиком");
        // НЕ частичный обрез: оставшийся массив значений — ЦЕЛИКОМ (не схлопнут)
        assert_eq!(a["enum_values"].as_array().unwrap().len(), 800);
        // мелкие структурные поля целы
        assert_eq!(out["full_name"], json!("Enum.Тест"));
        assert_eq!(out["counts"]["enum_values"], json!(800));
    }

    #[test]
    fn omit_noop_when_small() {
        let v = json!({"attributes": {"enum_values": [1, 2, 3]}, "full_name": "X"});
        let (out, omitted) = omit_oversize_sections(v.clone(), 30_000);
        assert!(!omitted);
        assert_eq!(out, v);
    }

    // ── shrink_search_results ────────────────────────────────────────────────

    /// Ответ-поиск: N объектов, у каждого паспорт + тяжёлая секция реквизитов.
    fn search_response(objects: usize, attrs_per_object: usize) -> Value {
        let results: Vec<Value> = (0..objects)
            .map(|i| {
                let attrs: Vec<Value> = (0..attrs_per_object)
                    .map(|j| json!({"name": format!("Реквизит{}_{}", i, j), "type": "СправочникСсылка.Номенклатура"}))
                    .collect();
                json!({
                    "full_name": format!("Catalog.Объект{}", i),
                    "meta_type": "Catalog",
                    "name": format!("Объект{}", i),
                    "synonym": format!("Объект {}", i),
                    "attributes": { "attributes": attrs },
                    "counts": { "attributes": attrs_per_object },
                })
            })
            .collect();
        json!({ "matched": objects, "truncated": false, "results": results })
    }

    /// Опознание найденного неприкосновенно: массив результатов не выбрасывается
    /// целиком ни при каком бюджете больше нуля (главное требование ТЗ).
    #[test]
    fn search_results_never_dropped_wholesale() {
        for budget in [50usize, 500, 5_000, 20_000, 48_000] {
            let (out, sh) = shrink_search_results(search_response(38, 20), budget);
            let arr = out
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("бюджет {}: массив results выброшен целиком", budget));
            assert!(!arr.is_empty(), "бюджет {}: отдан пустой массив", budget);
            // У каждого отданного элемента паспорт на месте.
            for item in arr {
                assert!(item.get("full_name").is_some(), "бюджет {}: потерян full_name", budget);
                assert!(item.get("meta_type").is_some(), "бюджет {}: потерян meta_type", budget);
            }
            assert!(sh.any(), "бюджет {}: ужатие не отмечено", budget);
        }
    }

    /// Разобранный в ТЗ случай: 38 объектов с секциями не влезают, одни имена —
    /// влезают. Ожидание: все 38 элементов на месте и опознаваемы, лишние секции
    /// сняты ровно в том объёме, которого хватило для бюджета (кто уместился —
    /// отдаётся с секциями), снятые помечены `_omitted` + `_count`.
    #[test]
    fn search_omits_sections_inside_items_keeping_all_names() {
        let (out, sh) = shrink_search_results(search_response(38, 20), 20_000);
        let arr = out.get("results").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 38, "укорачивать массив не требовалось");
        assert!(sh.sections_omitted);
        assert!(!sh.results_shortened);
        assert!(ser_len(&out) <= 20_000);
        // Паспорт и счётчики — у КАЖДОГО элемента.
        for (i, item) in arr.iter().enumerate() {
            assert_eq!(item["full_name"], json!(format!("Catalog.Объект{}", i)));
            assert_eq!(item["meta_type"], json!("Catalog"));
            assert_eq!(item["counts"]["attributes"], json!(20));
        }
        // Снятые секции честно помечены.
        let omitted: Vec<&Value> = arr
            .iter()
            .map(|it| &it["attributes"])
            .filter(|a| a.get("attributes_omitted").is_some())
            .collect();
        assert!(!omitted.is_empty(), "ни одна секция не снята — бюджет не соблюдён?");
        for a in omitted {
            assert_eq!(a["attributes_omitted"], json!(true));
            assert_eq!(a["attributes_count"], json!(20));
            assert!(a.get("attributes").is_none());
        }
    }

    /// Бюджет настолько мал, что не влезают даже имена: массив укорачивается,
    /// выставляются `results_total`/`results_shown`, элемент остаётся хотя бы один.
    #[test]
    fn search_shortens_array_when_names_alone_exceed_budget() {
        let (out, sh) = shrink_search_results(search_response(38, 20), 600);
        let arr = out.get("results").unwrap().as_array().unwrap();
        assert!(!arr.is_empty());
        assert!(sh.results_shortened);
        assert_eq!(sh.results_total, 38);
        assert_eq!(sh.results_shown, arr.len());
        assert_eq!(out["results_total"], json!(38));
        assert_eq!(out["results_shown"], json!(arr.len()));
        // Маркер «массив опущен целиком» (старое поведение) не выставляется.
        assert!(out.get("results_omitted").is_none());
    }

    #[test]
    fn search_shrink_noop_when_under_budget() {
        let v = search_response(2, 3);
        let (out, sh) = shrink_search_results(v.clone(), 48_000);
        assert_eq!(out, v);
        assert!(!sh.any());
        assert!(sh.full_bytes > 0);
    }

    #[test]
    fn search_shrink_disabled_on_zero_budget() {
        let v = search_response(38, 20);
        let (out, sh) = shrink_search_results(v.clone(), 0);
        assert_eq!(out, v);
        assert!(!sh.any());
    }

    // ── resolve_request_budget ───────────────────────────────────────────────

    #[test]
    fn request_budget_overrides_and_clamps() {
        set_response_cap(Some(48_000));
        set_response_cap_hard(Some(192_000));
        // Параметр не передан → глобальный бюджет.
        let b = resolve_request_budget(None);
        assert_eq!(b.applied, 48_000);
        assert!(!b.clamped);
        // В пределах потолка → как запрошено.
        let b = resolve_request_budget(Some(70_000));
        assert_eq!(b.applied, 70_000);
        assert!(!b.clamped);
        // Сверх потолка → зажим до потолка, не отказ.
        let b = resolve_request_budget(Some(500_000));
        assert_eq!(b.applied, 192_000);
        assert!(b.clamped);
        // Снятие стража на вызов не разрешено.
        let b = resolve_request_budget(Some(0));
        assert_eq!(b.applied, 48_000);
        // Нулевой потолок в конфиге → дефолт (иначе любой запрос зажимался бы в 0).
        set_response_cap_hard(Some(0));
        assert_eq!(response_cap_hard(), DEFAULT_MAX_RESPONSE_BYTES_HARD);
        set_response_cap_hard(None);
        set_response_cap(None);
    }
}
