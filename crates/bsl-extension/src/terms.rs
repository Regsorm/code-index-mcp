// Механическое обогащение процедур бизнес-терминами — БЕЗ LLM.
//
// Наполняет `procedure_enrichment.terms` на этапе индексации из четырёх
// дешёвых источников:
//   1. слова имени процедуры (сплит CamelCase/подчёркиваний/смены алфавита):
//      «УточнитьДанныеПоШтрихкоду» → «уточнить данные по штрихкоду»;
//   2. слова имени объекта-владельца модуля (из пути файла):
//      Catalogs/Номенклатура/… → «номенклатура»;
//   3. синоним объекта-владельца из `metadata_objects.synonym`
//      («Реализация товаров и услуг») — механический мост
//      «русское представление ↔ английский идентификатор»;
//   4. комментарий непосредственно над процедурой (строки `//…`).
//
// Зачем: лексическая «спираль уточнения» — модель знает понятие по-русски,
// но не знает точного написания в коде (CamelCase, словоформа, английский
// идентификатор) и перебирает варианты regex'ом впустую. Термы + триграммный
// FTS (см. schema.rs) закрывают словоформы, подписи и большую часть
// кросс-языка детерминированно, за секунды на парсинге.
//
// LLM-проход `enrich` остаётся опциональной командой: механика помечает свои
// записи `signature = 'mech:v1'` и НЕ трогает строки с другой подписью.

/// Подпись механических записей в `procedure_enrichment.signature`.
/// Менять при изменении алгоритма построения термов — полный проход
/// перезапишет только свои строки.
pub const MECH_SIGNATURE: &str = "mech:v1";

/// Папки выгрузки с модулями → singular meta_type (как в
/// `metadata_objects.full_name`). Шире, чем `index_extras::OBJECT_FOLDERS`
/// (тот — только типы со структурой реквизитов): здесь все типы, у которых
/// бывают .bsl-модули.
const MODULE_FOLDERS: &[(&str, &str)] = &[
    ("Catalogs", "Catalog"),
    ("Documents", "Document"),
    ("DataProcessors", "DataProcessor"),
    ("Reports", "Report"),
    ("CommonModules", "CommonModule"),
    ("Enums", "Enum"),
    ("Constants", "Constant"),
    ("InformationRegisters", "InformationRegister"),
    ("AccumulationRegisters", "AccumulationRegister"),
    ("AccountingRegisters", "AccountingRegister"),
    ("CalculationRegisters", "CalculationRegister"),
    ("ChartsOfAccounts", "ChartOfAccounts"),
    ("ChartsOfCharacteristicTypes", "ChartOfCharacteristicTypes"),
    ("ChartsOfCalculationTypes", "ChartOfCalculationTypes"),
    ("ExchangePlans", "ExchangePlan"),
    ("BusinessProcesses", "BusinessProcess"),
    ("Tasks", "Task"),
    ("CommonForms", "CommonForm"),
    ("CommonCommands", "CommonCommand"),
    ("WebServices", "WebService"),
    ("HTTPServices", "HTTPService"),
    ("DocumentJournals", "DocumentJournal"),
    ("Sequences", "Sequence"),
    ("SettingsStorages", "SettingsStorage"),
    ("ExternalDataSources", "ExternalDataSource"),
    ("FilterCriteria", "FilterCriterion"),
];

/// Кириллическая ли буква (для границы смены алфавита).
fn is_cyr(c: char) -> bool {
    matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')
}

/// Разбить идентификатор на слова в нижнем регистре.
///
/// Границы: не-буквенно-цифровой символ (подчёркивание, точка, пробел),
/// lower→Upper («уточнитьДанные»), буква↔цифра, смена алфавита
/// (кириллица↔латиница: «ent_ДоработкаОбмен»), конец аббревиатуры
/// (UPPER UPPER lower: «XMLReader» → «xml reader»).
pub fn split_identifier(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();

    let flush = |cur: &mut String, words: &mut Vec<String>| {
        if !cur.is_empty() {
            words.push(std::mem::take(cur));
        }
    };

    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            flush(&mut cur, &mut words);
            continue;
        }
        if !cur.is_empty() {
            let prev = chars[i - 1];
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_alphabetic() != c.is_alphabetic())
                || (prev.is_alphabetic() && c.is_alphabetic() && is_cyr(prev) != is_cyr(c))
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && i + 1 < chars.len()
                    && chars[i + 1].is_lowercase());
            if boundary {
                flush(&mut cur, &mut words);
            }
        }
        // Ё→Е сразу при нормализации: модели пишут «расчёт», в идентификаторах
        // 1С — «Расчет». Без свёртки триграммы «чёт»/«чет» не совпадают.
        for lc in c.to_lowercase() {
            cur.push(if lc == 'ё' { 'е' } else { lc });
        }
    }
    flush(&mut cur, &mut words);
    words
}

/// Нормализация свободного текста для термов: нижний регистр + ё→е
/// (та же свёртка, что в `split_identifier`, — термы и запросы должны
/// нормализоваться одинаково).
pub fn fold_text(s: &str) -> String {
    s.to_lowercase().replace('ё', "е")
}

/// По repo-relative пути .bsl-модуля определить объект-владельца:
/// `(meta_type, имя)` — `Catalogs/Номенклатура/Ext/ObjectModule.bsl` →
/// `("Catalog", "Номенклатура")`. Работает и для форм
/// (`…/Forms/ФормаЭлемента/Ext/Form/Module.bsl` — тот же объект), и для
/// sub-config-префиксов (`base/Catalogs/…`, `extensions/X/Catalogs/…`).
/// `None` — модуль вне объектных папок (например, `Configuration/…`).
pub fn object_from_module_path(path: &str) -> Option<(&'static str, String)> {
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    for i in 0..comps.len().saturating_sub(1) {
        for (folder, meta_type) in MODULE_FOLDERS {
            if comps[i] == *folder {
                return Some((meta_type, comps[i + 1].to_string()));
            }
        }
    }
    None
}

/// Комментарий непосредственно над процедурой: идём вверх от строки
/// `line_start` (1-based), пропуская аннотации (`&НаСервере`) и
/// декоративные разделители (`//////`), собираем содержательные `//`-строки.
/// Останавливаемся на первой не-комментарной строке. Результат обрезается
/// до ~240 символов (термы — не полнотекст, а сигнал для FTS).
pub fn extract_leading_comment<S: AsRef<str>>(lines: &[S], line_start: usize) -> Option<String> {
    if line_start < 2 {
        return None;
    }
    let mut collected: Vec<&str> = Vec::new();
    // line_start 1-based → индекс строки процедуры = line_start-1; выше неё — line_start-2.
    let mut i = line_start - 2;
    loop {
        let t = lines.get(i)?.as_ref().trim();
        if t.starts_with("//") {
            let body = t.trim_start_matches('/').trim();
            // Декоративный разделитель («////////», «//====») — пропустить.
            if !body.is_empty() && !body.chars().all(|c| matches!(c, '=' | '-' | '*' | '/')) {
                collected.push(body);
            }
        } else if t.starts_with('&') {
            // Аннотация компиляции между комментарием и процедурой.
        } else {
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    let mut joined = collected.join(" ");
    if joined.chars().count() > 240 {
        joined = joined.chars().take(240).collect();
    }
    Some(joined)
}

/// Собрать строку термов для одной процедуры. Формат — фразы через запятую
/// (как у LLM-enrich), всё в нижнем регистре. Пустые источники опускаются;
/// дубли фраз схлопываются.
pub fn build_terms(
    proc_name: &str,
    object_name: Option<&str>,
    object_synonym: Option<&str>,
    comment: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let name_words = split_identifier(proc_name).join(" ");
    if !name_words.is_empty() {
        parts.push(name_words);
    }
    if let Some(obj) = object_name {
        let obj_words = split_identifier(obj).join(" ");
        if !obj_words.is_empty() && !parts.contains(&obj_words) {
            parts.push(obj_words);
        }
    }
    if let Some(syn) = object_synonym {
        let s = fold_text(syn);
        if !s.is_empty() && !parts.contains(&s) {
            parts.push(s);
        }
    }
    if let Some(c) = comment {
        let c = fold_text(c);
        if !c.is_empty() && !parts.contains(&c) {
            parts.push(c);
        }
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_camel_cyrillic() {
        assert_eq!(
            split_identifier("УточнитьДанныеПоШтрихкоду"),
            vec!["уточнить", "данные", "по", "штрихкоду"]
        );
    }

    #[test]
    fn yo_folds_to_e_everywhere() {
        // Идентификаторы: «РасчётСебестоимости» нормализуется без ё.
        assert_eq!(split_identifier("РасчётСебестоимости"), vec!["расчет", "себестоимости"]);
        // Свободный текст (синонимы, комментарии): та же свёртка.
        assert_eq!(fold_text("Учёт партий"), "учет партий");
    }

    #[test]
    fn split_latin_and_underscores() {
        assert_eq!(split_identifier("RefineBarcode"), vec!["refine", "barcode"]);
        assert_eq!(
            split_identifier("ent_ДоработкаОбмена"),
            vec!["ent", "доработка", "обмена"]
        );
    }

    #[test]
    fn split_acronym_and_digits() {
        assert_eq!(split_identifier("XMLReader"), vec!["xml", "reader"]);
        assert_eq!(
            split_identifier("ПолучитьHTTPОтвет"),
            vec!["получить", "http", "ответ"]
        );
        assert_eq!(split_identifier("Форма2Элемент"), vec!["форма", "2", "элемент"]);
    }

    #[test]
    fn object_from_paths() {
        assert_eq!(
            object_from_module_path("Catalogs/Номенклатура/Ext/ObjectModule.bsl"),
            Some(("Catalog", "Номенклатура".to_string()))
        );
        // Форма того же объекта → тот же владелец.
        assert_eq!(
            object_from_module_path(
                "Documents/Реализация/Forms/ФормаДокумента/Ext/Form/Module.bsl"
            ),
            Some(("Document", "Реализация".to_string()))
        );
        // Sub-config-префикс (base/, extensions/<имя>/) не мешает.
        assert_eq!(
            object_from_module_path("base/CommonModules/РаботаСоСкидками/Ext/Module.bsl"),
            Some(("CommonModule", "РаботаСоСкидками".to_string()))
        );
        assert_eq!(object_from_module_path("Configuration/ManagedApplicationModule.bsl"), None);
    }

    #[test]
    fn comment_extraction() {
        let lines = vec![
            "////////////////////////////////",
            "// Уточняет данные по штрихкоду.",
            "// Параметры: Штрихкод - Строка.",
            "&НаСервере",
            "Процедура УточнитьДанные()",
        ];
        // Процедура на строке 5 (1-based).
        assert_eq!(
            extract_leading_comment(&lines, 5),
            Some("Уточняет данные по штрихкоду. Параметры: Штрихкод - Строка.".to_string())
        );
        // Нет комментария над строкой 1.
        assert_eq!(extract_leading_comment(&lines, 1), None);
    }

    #[test]
    fn comment_stops_at_code() {
        let lines = vec![
            "КонецПроцедуры",
            "",
            "Процедура Другая()",
        ];
        // Пустая строка над процедурой → комментария нет.
        assert_eq!(extract_leading_comment(&lines, 3), None);
    }

    #[test]
    fn build_terms_full_and_dedup() {
        let t = build_terms(
            "УточнитьШтрихкод",
            Some("Номенклатура"),
            Some("Номенклатура"),
            Some("Уточняет штрихкод товара"),
        );
        // Синоним «Номенклатура» дублирует слова объекта → схлопнут.
        assert_eq!(t, "уточнить штрихкод, номенклатура, уточняет штрихкод товара");

        let t2 = build_terms("RefineBarcode", None, None, None);
        assert_eq!(t2, "refine barcode");
    }
}
