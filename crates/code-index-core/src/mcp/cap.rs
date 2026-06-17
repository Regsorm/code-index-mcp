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
}
