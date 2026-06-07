// Обратный индекс использований объектов метаданных 1С В КОДЕ (`.bsl`).
//
// В отличие от `data_links` (декларативные ссылки из XML-метаданных) и
// `proc_call_graph` (граф вызовов процедур), эта таблица — КОД-производная:
// «где в коде упоминается объект X». Лёгкий source-aware regex-слой, порт
// логики rlm-tools-bsl (`_extract_code_usages` / `_scan_module`).
//
// Ловит три вида обращений:
//   * `manager`  — обращение к менеджеру коллекции `Документы.X` / `Documents.X`
//     (ищется в РЕАЛЬНОМ коде: строковые литералы и комментарии вырезаны,
//     включая многострочные литералы);
//   * `ref_type` — тип-ссылка в строковом литерале: `"ДокументСсылка.X"` (RU)
//     либо `"DocumentRef.X"` (EN);
//   * `query`    — путь метаданных в строковом литерале (текст запроса):
//     `Документ.X` и `Документ.X.Товары` (3-й сегмент = имя ТЧ → `member_path`).
//
// Что НЕ ловит (осознанно): доступ через локальные переменные
// (`Док = Документы.X; Док.Товары`) — нужен type inference, вне охвата.
//
// Русские формы множественного числа/типов нерегулярны (РегистрСведений →
// РегистрыСведений, Обработка → Обработки) — задаются явной таблицей, не правилом.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Одно обращение к объекту метаданных в коде.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeUsage {
    /// Канонический объект: `Document.РеализацияТоваровУслуг`.
    pub object_ref: String,
    /// `object_ref.to_lowercase()` — для индексного поиска без UDF (кириллица).
    pub object_ref_key: String,
    /// Имя ТЧ для `query` (3-й сегмент пути), иначе `None`.
    pub member_path: Option<String>,
    /// Вид обращения: `manager` | `ref_type` | `query`.
    pub usage_kind: &'static str,
    /// Номер строки в модуле (1-based).
    pub line: usize,
}

/// Описание форм одной категории метаданных, адресуемой в коде.
struct MetaForm {
    /// Канонический префикс без точки: `Document`.
    canonical: &'static str,
    ru_singular: &'static str,
    ru_plural: &'static str,
    en_singular: &'static str,
    // en_plural выводится как en_singular+"s" не всегда — храним явно.
    en_plural: &'static str,
    /// RU-формы тип-ссылок (`ДокументСсылка`, `ДокументОбъект`, …) без точки.
    reftypes: &'static [&'static str],
}

// Единый источник истины. Порт `_RU_META_FORMS` из rlm. Только категории,
// адресуемые через менеджер коллекции в коде; чисто метаданные-объекты
// (Subsystem, Role, …) сюда не входят — в коде так не обращаются.
const META_FORMS: &[MetaForm] = &[
    MetaForm { canonical: "Catalog", ru_singular: "Справочник", ru_plural: "Справочники", en_singular: "Catalog", en_plural: "Catalogs", reftypes: &["СправочникСсылка", "СправочникОбъект", "СправочникМенеджер"] },
    MetaForm { canonical: "Document", ru_singular: "Документ", ru_plural: "Документы", en_singular: "Document", en_plural: "Documents", reftypes: &["ДокументСсылка", "ДокументОбъект", "ДокументМенеджер"] },
    MetaForm { canonical: "Enum", ru_singular: "Перечисление", ru_plural: "Перечисления", en_singular: "Enum", en_plural: "Enums", reftypes: &["ПеречислениеСсылка", "ПеречислениеМенеджер"] },
    MetaForm { canonical: "InformationRegister", ru_singular: "РегистрСведений", ru_plural: "РегистрыСведений", en_singular: "InformationRegister", en_plural: "InformationRegisters", reftypes: &["РегистрСведенийЗапись", "РегистрСведенийКлючЗаписи", "РегистрСведенийМенеджер", "РегистрСведенийНаборЗаписей"] },
    MetaForm { canonical: "AccumulationRegister", ru_singular: "РегистрНакопления", ru_plural: "РегистрыНакопления", en_singular: "AccumulationRegister", en_plural: "AccumulationRegisters", reftypes: &["РегистрНакопленияЗапись", "РегистрНакопленияКлючЗаписи", "РегистрНакопленияМенеджер", "РегистрНакопленияНаборЗаписей"] },
    MetaForm { canonical: "AccountingRegister", ru_singular: "РегистрБухгалтерии", ru_plural: "РегистрыБухгалтерии", en_singular: "AccountingRegister", en_plural: "AccountingRegisters", reftypes: &["РегистрБухгалтерииЗапись", "РегистрБухгалтерииКлючЗаписи", "РегистрБухгалтерииМенеджер", "РегистрБухгалтерииНаборЗаписей"] },
    MetaForm { canonical: "CalculationRegister", ru_singular: "РегистрРасчета", ru_plural: "РегистрыРасчета", en_singular: "CalculationRegister", en_plural: "CalculationRegisters", reftypes: &["РегистрРасчетаЗапись", "РегистрРасчетаКлючЗаписи", "РегистрРасчетаМенеджер", "РегистрРасчетаНаборЗаписей"] },
    MetaForm { canonical: "ChartOfCharacteristicTypes", ru_singular: "ПланВидовХарактеристик", ru_plural: "ПланыВидовХарактеристик", en_singular: "ChartOfCharacteristicTypes", en_plural: "ChartsOfCharacteristicTypes", reftypes: &["ПланВидовХарактеристикСсылка", "ПланВидовХарактеристикОбъект", "ПланВидовХарактеристикМенеджер"] },
    MetaForm { canonical: "ChartOfAccounts", ru_singular: "ПланСчетов", ru_plural: "ПланыСчетов", en_singular: "ChartOfAccounts", en_plural: "ChartsOfAccounts", reftypes: &["ПланСчетовСсылка", "ПланСчетовОбъект", "ПланСчетовМенеджер"] },
    MetaForm { canonical: "ChartOfCalculationTypes", ru_singular: "ПланВидовРасчета", ru_plural: "ПланыВидовРасчета", en_singular: "ChartOfCalculationTypes", en_plural: "ChartsOfCalculationTypes", reftypes: &["ПланВидовРасчетаСсылка", "ПланВидовРасчетаОбъект", "ПланВидовРасчетаМенеджер"] },
    MetaForm { canonical: "ExchangePlan", ru_singular: "ПланОбмена", ru_plural: "ПланыОбмена", en_singular: "ExchangePlan", en_plural: "ExchangePlans", reftypes: &["ПланОбменаСсылка", "ПланОбменаОбъект", "ПланОбменаМенеджер"] },
    MetaForm { canonical: "BusinessProcess", ru_singular: "БизнесПроцесс", ru_plural: "БизнесПроцессы", en_singular: "BusinessProcess", en_plural: "BusinessProcesses", reftypes: &["БизнесПроцессСсылка", "БизнесПроцессОбъект", "БизнесПроцессМенеджер"] },
    MetaForm { canonical: "Task", ru_singular: "Задача", ru_plural: "Задачи", en_singular: "Task", en_plural: "Tasks", reftypes: &["ЗадачаСсылка", "ЗадачаОбъект", "ЗадачаМенеджер"] },
    MetaForm { canonical: "Report", ru_singular: "Отчет", ru_plural: "Отчеты", en_singular: "Report", en_plural: "Reports", reftypes: &[] },
    MetaForm { canonical: "DataProcessor", ru_singular: "Обработка", ru_plural: "Обработки", en_singular: "DataProcessor", en_plural: "DataProcessors", reftypes: &[] },
    MetaForm { canonical: "Constant", ru_singular: "Константа", ru_plural: "Константы", en_singular: "Constant", en_plural: "Constants", reftypes: &[] },
];

/// Суффиксы EN-форм тип-ссылок (`DocumentRef`, `CatalogObject`, …). Сверх-набор:
/// несуществующие комбинации (`CatalogRecordSet`) не навредят — они просто не
/// встретятся в коде.
const EN_REF_SUFFIXES: &[&str] = &[
    "Ref", "Object", "Manager", "List", "Selection", "RecordSet", "RecordKey", "Record",
];

/// lower(множественная форма) → канонический префикс (только `manager`).
fn manager_map() -> &'static HashMap<String, &'static str> {
    static M: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::new();
        for f in META_FORMS {
            m.insert(f.ru_plural.to_lowercase(), f.canonical);
            m.insert(f.en_plural.to_lowercase(), f.canonical);
        }
        m
    })
}

/// lower(единственная форма) → канонический префикс (только `query`).
fn query_map() -> &'static HashMap<String, &'static str> {
    static M: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::new();
        for f in META_FORMS {
            m.insert(f.ru_singular.to_lowercase(), f.canonical);
            m.insert(f.en_singular.to_lowercase(), f.canonical);
        }
        m
    })
}

/// lower(RU-форма тип-ссылки без точки) → канонический префикс (`ref_type`).
fn ru_reftype_map() -> &'static HashMap<String, &'static str> {
    static M: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::new();
        for f in META_FORMS {
            for rt in f.reftypes {
                m.insert(rt.to_lowercase(), f.canonical);
            }
        }
        m
    })
}

/// lower(EN-форма тип-ссылки без точки) → канонический префикс (`ref_type`).
fn en_reftype_map() -> &'static HashMap<String, &'static str> {
    static M: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::new();
        for f in META_FORMS {
            for suf in EN_REF_SUFFIXES {
                m.insert(format!("{}{}", f.en_singular, suf).to_lowercase(), f.canonical);
            }
        }
        m
    })
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Разделить строку кода на «реальный код» (без строк/комментариев) и список
/// фрагментов строковых литералов. Порт `_scan_module` (по одной строке).
/// `in_string` — открыт ли многострочный литерал с предыдущей строки.
/// Возвращает `(code, strings, in_string_after)`.
fn scan_line(raw: &str, mut in_string: bool) -> (String, Vec<String>, bool) {
    let chars: Vec<char> = raw.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut code = String::new();
    let mut strings: Vec<String> = Vec::new();
    // Начало текущего строкового сегмента. -1 = вне строки.
    let mut seg_start: isize = if in_string { 0 } else { -1 };

    while i < n {
        let ch = chars[i];
        if in_string {
            if ch == '"' {
                if i + 1 < n && chars[i + 1] == '"' {
                    i += 2; // экранированная "" — остаёмся в строке
                    continue;
                }
                strings.push(chars[seg_start as usize..i].iter().collect());
                in_string = false;
                seg_start = -1;
                i += 1;
            } else {
                i += 1;
            }
        } else if ch == '"' {
            in_string = true;
            seg_start = (i + 1) as isize;
            i += 1;
        } else if ch == '/' && i + 1 < n && chars[i + 1] == '/' {
            break; // комментарий до конца строки — ни код, ни строка
        } else {
            code.push(ch);
            i += 1;
        }
    }
    if in_string {
        // Литерал продолжается на следующей строке — отдаём фрагмент этой строки.
        strings.push(chars[seg_start as usize..].iter().collect());
    }
    (code, strings, in_string)
}

/// Непересекающиеся пары `ident.ident` (аналог regex `(\w+)\.(\w+)` finditer).
fn dotted_pairs(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if is_word(chars[i]) {
            let st = i;
            while i < n && is_word(chars[i]) {
                i += 1;
            }
            if i + 1 < n && chars[i] == '.' && is_word(chars[i + 1]) {
                let id1: String = chars[st..i].iter().collect();
                i += 1; // skip '.'
                let s2 = i;
                while i < n && is_word(chars[i]) {
                    i += 1;
                }
                let id2: String = chars[s2..i].iter().collect();
                out.push((id1, id2));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Непересекающиеся пути `ident.ident(.ident)?` (аналог regex
/// `(\w+)\.(\w+)(?:\.(\w+))?` finditer; 3-й сегмент опционален и жадный).
fn path_triples(s: &str) -> Vec<(String, String, Option<String>)> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if is_word(chars[i]) {
            let st = i;
            while i < n && is_word(chars[i]) {
                i += 1;
            }
            if i + 1 < n && chars[i] == '.' && is_word(chars[i + 1]) {
                let id1: String = chars[st..i].iter().collect();
                i += 1;
                let s2 = i;
                while i < n && is_word(chars[i]) {
                    i += 1;
                }
                let id2: String = chars[s2..i].iter().collect();
                // опциональный 3-й сегмент
                let mut id3: Option<String> = None;
                if i + 1 < n && chars[i] == '.' && is_word(chars[i + 1]) {
                    i += 1;
                    let s3 = i;
                    while i < n && is_word(chars[i]) {
                        i += 1;
                    }
                    id3 = Some(chars[s3..i].iter().collect());
                }
                out.push((id1, id2, id3));
            }
        } else {
            i += 1;
        }
    }
    out
}

fn make_usage(canonical: &str, name: &str, member: Option<String>, kind: &'static str, line: usize) -> CodeUsage {
    let object_ref = format!("{}.{}", canonical, name);
    let object_ref_key = object_ref.to_lowercase();
    CodeUsage { object_ref, object_ref_key, member_path: member, usage_kind: kind, line }
}

/// Извлечь обращения к объектам метаданных из тела `.bsl`-модуля.
pub fn extract_code_usages(content: &str) -> Vec<CodeUsage> {
    let mut out: Vec<CodeUsage> = Vec::new();
    let mut in_string = false;

    for (idx, raw) in content.split('\n').enumerate() {
        let lineno = idx + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (code, strings, carry) = scan_line(raw, in_string);
        in_string = carry;

        // manager — только в реальном коде (строки/комментарии вырезаны).
        if code.contains('.') {
            for (g1, g2) in dotted_pairs(&code) {
                if let Some(&canon) = manager_map().get(&g1.to_lowercase()) {
                    out.push(make_usage(canon, &g2, None, "manager", lineno));
                }
            }
        }

        // ref_type / query — только внутри строковых литералов.
        for content in &strings {
            if !content.contains('.') {
                continue;
            }
            for (g1, g2, g3) in path_triples(content) {
                let low = g1.to_lowercase();
                if let Some(&canon) = ru_reftype_map().get(&low) {
                    out.push(make_usage(canon, &g2, None, "ref_type", lineno));
                    continue;
                }
                if let Some(&canon) = query_map().get(&low) {
                    out.push(make_usage(canon, &g2, g3, "query", lineno));
                    continue;
                }
                if let Some(&canon) = en_reftype_map().get(&low) {
                    out.push(make_usage(canon, &g2, None, "ref_type", lineno));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(c: &str) -> Vec<(String, Option<String>, &'static str)> {
        extract_code_usages(c)
            .into_iter()
            .map(|u| (u.object_ref, u.member_path, u.usage_kind))
            .collect()
    }

    #[test]
    fn manager_in_code() {
        let r = kinds("Док = Документы.ПриобретениеТоваровУслуг.СоздатьДокумент();");
        assert_eq!(r, vec![("Document.ПриобретениеТоваровУслуг".to_string(), None, "manager")]);
    }

    #[test]
    fn manager_ignored_in_string_and_comment() {
        // В строке и в комментарии Документы.X не должен ловиться как manager.
        assert!(kinds("Сообщить(\"Документы.Заказ внутри строки\");")
            .iter()
            .all(|(_, _, k)| *k != "manager"));
        assert!(kinds("// Документы.Заказ в комментарии").is_empty());
    }

    #[test]
    fn ref_type_ru_in_string() {
        let r = kinds("Т = Тип(\"СправочникСсылка.Контрагенты\");");
        assert_eq!(r, vec![("Catalog.Контрагенты".to_string(), None, "ref_type")]);
    }

    #[test]
    fn ref_type_en_in_string() {
        let r = kinds("Т = Тип(\"DocumentRef.Заказ\");");
        assert_eq!(r, vec![("Document.Заказ".to_string(), None, "ref_type")]);
    }

    #[test]
    fn query_path_with_tabular() {
        // Путь метаданных в тексте запроса: 3-й сегмент = имя ТЧ.
        let r = kinds("Запрос.Текст = \"ВЫБРАТЬ Ссылка ИЗ Документ.РеализацияТоваровУслуг.Товары\";");
        assert_eq!(
            r,
            vec![(
                "Document.РеализацияТоваровУслуг".to_string(),
                Some("Товары".to_string()),
                "query"
            )]
        );
    }

    #[test]
    fn query_path_head_only() {
        let r = kinds("Текст = \"ИЗ Справочник.Номенклатура КАК Н\";");
        assert_eq!(r, vec![("Catalog.Номенклатура".to_string(), None, "query")]);
    }

    #[test]
    fn escaped_quote_keeps_string_intact() {
        // "" — экранированная кавычка, строка не закрывается раньше времени.
        let r = kinds("А = \"x\"\"y\"; Док = Документы.Заказ.Создать();");
        assert_eq!(r, vec![("Document.Заказ".to_string(), None, "manager")]);
    }

    #[test]
    fn multiline_string_literal() {
        // Многострочный литерал запроса: путь на 2-й физической строке.
        let src = "Запрос.Текст =\n\"ВЫБРАТЬ * ИЗ Документ.Заказ\nГДЕ Истина\";";
        let r = extract_code_usages(src);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].object_ref, "Document.Заказ");
        assert_eq!(r[0].usage_kind, "query");
        assert_eq!(r[0].line, 2, "путь на 2-й физической строке литерала");
    }

    #[test]
    fn unknown_collection_skipped() {
        // Не-метаданная коллекция (СписокЗаказов) и примитивы не дают рёбер.
        assert!(kinds("Список.Количество();").is_empty());
        assert!(kinds("Х = 1.5;").is_empty());
    }

    #[test]
    fn register_irregular_plural() {
        let r = kinds("РегистрыСведений.КурсыВалют.СоздатьНаборЗаписей();");
        assert_eq!(r, vec![("InformationRegister.КурсыВалют".to_string(), None, "manager")]);
    }
}
